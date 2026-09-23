#![forbid(unsafe_code)]

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use srep::path::strip_srep_suffix;
use srep::{
    Checksum, CompressionConfig, Error, ErrorKind, Layout, Method, RepConfig, ResourceConfig,
    ResourceContext, Result, compress_with_context, decompress_with_context,
    inspect_with_resources, parse_size, verify_with_resources,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("srep: {error}");
        std::process::exit(error.code());
    }
}

fn run() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let command = args.next().ok_or_else(|| Error::invalid_config(usage()))?;
    match command.to_str() {
        Some("compress") => run_compress(args.collect()),
        Some("decompress") => run_decompress(args.collect()),
        Some("info") => run_info(args.collect()),
        Some("test") => run_test(args.collect()),
        Some("-h") | Some("--help") => {
            println!("{}", usage());
            Ok(())
        }
        Some("-V") | Some("--version") => {
            println!("{}", software_version_line());
            Ok(())
        }
        Some(other) => Err(Error::invalid_config(format!(
            "unknown command '{other}'\n{}",
            usage()
        ))),
        None => Err(Error::invalid_config("command is not valid UTF-8")),
    }
}

fn software_version_line() -> String {
    format!("srep {}", env!("CARGO_PKG_VERSION"))
}

fn run_compress(args: Vec<OsString>) -> Result<()> {
    let (options, positional) = parse_options(args, CommandKind::Compress)?;
    if positional.is_empty() || positional.len() > 2 {
        return Err(Error::invalid_config(
            "compress requires INPUT and optional OUTPUT",
        ));
    }
    let input = &positional[0];
    let input_path = Path::new(input);
    let output = positional
        .get(1)
        .cloned()
        .or_else(|| default_compress_path(input_path));
    let output =
        output.ok_or_else(|| Error::invalid_config("output is required when input is stdin"))?;
    options.config.validate()?;
    if options.force && output == OsStr::new("-") {
        return Err(Error::invalid_config("--force is invalid for stdout"));
    }
    if input == OsStr::new("-") && output == OsStr::new("-") {
        return compress_stream(options, io::stdin().lock(), io::stdout().lock());
    }
    if input != OsStr::new("-") && output != OsStr::new("-") {
        reject_input_collision(input_path, Path::new(&output))?;
        if !options.force && path_exists(Path::new(&output))? {
            return Err(Error::invalid_config(
                "output exists; pass --force to replace a non-directory destination",
            ));
        }
    }
    let config = options.config.clone();
    if input == OsStr::new("-") {
        write_archive(
            Path::new(&output),
            options.force,
            io::stdin().lock(),
            &config,
        )
    } else if output == OsStr::new("-") {
        compress_stream(
            options,
            BufReader::new(open_input(input_path)?),
            io::stdout().lock(),
        )
    } else {
        write_archive(
            Path::new(&output),
            options.force,
            BufReader::new(open_input(input_path)?),
            &config,
        )
    }
}

fn run_decompress(args: Vec<OsString>) -> Result<()> {
    let (options, positional) = parse_options(args, CommandKind::Decompress)?;
    if positional.is_empty() || positional.len() > 2 {
        return Err(Error::invalid_config(
            "decompress requires INPUT and optional OUTPUT",
        ));
    }
    let input = &positional[0];
    let input_path = Path::new(input);
    let output = positional
        .get(1)
        .cloned()
        .or_else(|| default_decompress_path(input_path));
    let output = output.ok_or_else(|| {
        Error::invalid_config("output is required when input is stdin or has no .srep suffix")
    })?;
    if options.force && output == OsStr::new("-") {
        return Err(Error::invalid_config("--force is invalid for stdout"));
    }
    if input == OsStr::new("-") && output == OsStr::new("-") {
        return decompress_stream(
            io::stdin().lock(),
            io::stdout().lock(),
            &options.config.resources,
        );
    }
    if input != OsStr::new("-") && output != OsStr::new("-") {
        reject_input_collision(input_path, Path::new(&output))?;
        if !options.force && path_exists(Path::new(&output))? {
            return Err(Error::invalid_config(
                "output exists; pass --force to replace a non-directory destination",
            ));
        }
    }
    if input == OsStr::new("-") {
        decompress_to_path(
            Path::new(&output),
            options.force,
            io::stdin().lock(),
            &options.config.resources,
        )
    } else if output == OsStr::new("-") {
        decompress_stream(
            BufReader::new(open_input(input_path)?),
            io::stdout().lock(),
            &options.config.resources,
        )
    } else {
        decompress_to_path(
            Path::new(&output),
            options.force,
            BufReader::new(open_input(input_path)?),
            &options.config.resources,
        )
    }
}

fn run_info(args: Vec<OsString>) -> Result<()> {
    let (options, positional) = parse_options(args, CommandKind::Info)?;
    if positional.len() != 1 {
        return Err(Error::invalid_config("info requires INPUT"));
    }
    let input = &positional[0];
    let info = if input == OsStr::new("-") {
        inspect_with_resources(io::stdin().lock(), &options.config.resources)?
    } else {
        inspect_with_resources(
            BufReader::new(open_input(Path::new(input))?),
            &options.config.resources,
        )?
    };
    if !options.quiet {
        // NG archives carry typed method/layout/checksum fields; legacy archives
        // carry only the legacy_layout/legacy_checksum family fields.  The family
        // discriminator is authoritative: legacy versions must not be mislabeled as
        // SREP-NG merely because their version number collides with an NG one.
        let format_name = if info.legacy_layout.is_some() {
            format!("legacy SREP v{}", info.version)
        } else {
            format!("SREP-NG v{}", info.version)
        };
        println!("format: {format_name}");
        println!(
            "method: {}",
            info.method.map(Method::name).unwrap_or("unknown")
        );
        println!(
            "layout: {}",
            info.layout
                .map(Layout::name)
                .or(info.legacy_layout.as_deref())
                .unwrap_or("unknown")
        );
        println!(
            "checksum: {}",
            info.checksum
                .map(Checksum::name)
                .or(info.legacy_checksum.as_deref())
                .unwrap_or("unknown")
        );
        println!(
            "block size: {}",
            info.block_size
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
        );
        println!(
            "minimum match: {}",
            info.min_match
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
        );
        if let Some(base_len) = info.legacy_base_len {
            println!("legacy BASE_LEN: {base_len}");
        }
        println!("original size: {}", info.original_size);
        println!("payload size: {}", info.payload_size);
        println!("blocks: {}", info.block_count);
        println!("semantic matches: {}", info.semantic_match_count);
        println!("covered bytes: {}", info.covered_bytes);
        println!("literal bytes: {}", info.literal_bytes);
    }
    Ok(())
}

fn run_test(args: Vec<OsString>) -> Result<()> {
    let (options, positional) = parse_options(args, CommandKind::Test)?;
    if positional.len() != 1 {
        return Err(Error::invalid_config("test requires INPUT"));
    }
    let input = &positional[0];
    let stats = if input == OsStr::new("-") {
        verify_with_resources(io::stdin().lock(), &options.config.resources)?
    } else {
        verify_with_resources(
            BufReader::new(open_input(Path::new(input))?),
            &options.config.resources,
        )?
    };
    if !options.quiet {
        eprintln!(
            "verified {} blocks ({} bytes)",
            stats.block_count, stats.original_size
        );
    }
    Ok(())
}

#[derive(Clone)]
struct Options {
    config: CompressionConfig,
    force: bool,
    quiet: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    Compress,
    Decompress,
    Info,
    Test,
}

impl CommandKind {
    const fn allows_compression_options(self) -> bool {
        matches!(self, Self::Compress)
    }

    const fn allows_force(self) -> bool {
        matches!(self, Self::Compress | Self::Decompress)
    }
}

fn parse_options(args: Vec<OsString>, command: CommandKind) -> Result<(Options, Vec<OsString>)> {
    let mut options = Options {
        config: CompressionConfig::default(),
        force: false,
        quiet: false,
    };
    let mut positional = Vec::new();
    let mut seen = SeenOptions::default();
    let mut index_path: Option<OsString> = None;
    let mut selected_method: Option<Method> = None;
    let mut selected_layout: Option<Layout> = None;
    let mut selected_checksum: Option<Checksum> = None;
    let mut selected_block_size: Option<u64> = None;
    let mut selected_min_match: Option<u64> = None;
    let mut selected_seed_size: Option<u64> = None;
    let mut selected_target_chunk: Option<u64> = None;
    let mut selected_max_distance: Option<u64> = None;
    let mut rep_overlay = false;
    let mut rep_distance: Option<u64> = None;
    let mut rep_min_match: Option<u64> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let bytes = arg.as_encoded_bytes();
        if arg == OsStr::new("--force") {
            if !command.allows_force() {
                return Err(Error::invalid_config("--force is invalid for this command"));
            }
            note_once(&mut seen.force, "--force")?;
            options.force = true;
        } else if arg == OsStr::new("--quiet") || arg == OsStr::new("-q") {
            note_once(&mut seen.quiet, "--quiet")?;
            options.quiet = true;
        } else if let Some(value) = option_value(arg, &args, &mut index, "--layout")? {
            require_compression(command, "--layout")?;
            note_once(&mut seen.layout, "--layout")?;
            selected_layout = Some(Layout::parse(parse_utf8(&value, "--layout")?)?);
        } else if let Some(value) = option_value(arg, &args, &mut index, "--checksum")? {
            require_compression(command, "--checksum")?;
            note_once(&mut seen.checksum, "--checksum")?;
            selected_checksum = Some(Checksum::parse(parse_utf8(&value, "--checksum")?)?);
        } else if let Some(value) = option_value(arg, &args, &mut index, "--block-size")? {
            require_compression(command, "--block-size")?;
            note_once(&mut seen.block_size, "--block-size")?;
            selected_block_size = Some(parse_size(parse_utf8(&value, "--block-size")?)?);
        } else if let Some(value) = option_value(arg, &args, &mut index, "--min-match")? {
            require_compression(command, "--min-match")?;
            note_once(&mut seen.min_match, "--min-match")?;
            selected_min_match = Some(parse_size(parse_utf8(&value, "--min-match")?)?);
        } else if let Some(value) = option_value(arg, &args, &mut index, "--seed-size")? {
            require_compression(command, "--seed-size")?;
            note_once(&mut seen.seed_size, "--seed-size")?;
            selected_seed_size = Some(parse_size(parse_utf8(&value, "--seed-size")?)?);
        } else if let Some(value) = option_value(arg, &args, &mut index, "--target-chunk")? {
            require_compression(command, "--target-chunk")?;
            note_once(&mut seen.target_chunk, "--target-chunk")?;
            selected_target_chunk = Some(parse_size(parse_utf8(&value, "--target-chunk")?)?);
        } else if let Some(value) = option_value(arg, &args, &mut index, "--max-distance")? {
            require_compression(command, "--max-distance")?;
            note_once(&mut seen.max_distance, "--max-distance")?;
            selected_max_distance = Some(parse_size(parse_utf8(&value, "--max-distance")?)?);
        } else if arg == OsStr::new("--rep-overlay") {
            require_compression(command, "--rep-overlay")?;
            note_once(&mut seen.rep_overlay, "--rep-overlay")?;
            rep_overlay = true;
        } else if let Some(value) = option_value(arg, &args, &mut index, "--rep-distance")? {
            require_compression(command, "--rep-distance")?;
            note_once(&mut seen.rep_distance, "--rep-distance")?;
            rep_distance = Some(parse_size(parse_utf8(&value, "--rep-distance")?)?);
        } else if let Some(value) = option_value(arg, &args, &mut index, "--rep-min-match")? {
            require_compression(command, "--rep-min-match")?;
            note_once(&mut seen.rep_min_match, "--rep-min-match")?;
            rep_min_match = Some(parse_size(parse_utf8(&value, "--rep-min-match")?)?);
        } else if let Some(value) = option_value(arg, &args, &mut index, "--memory")? {
            note_once(&mut seen.memory, "--memory")?;
            options.config.resources.memory = parse_size(parse_utf8(&value, "--memory")?)?;
        } else if let Some(value) = option_value(arg, &args, &mut index, "--temp-dir")? {
            note_once(&mut seen.temp_dir, "--temp-dir")?;
            if value.is_empty() {
                return Err(Error::invalid_config("--temp-dir must not be empty"));
            }
            options.config.resources.temp_dir = PathBuf::from(value);
        } else if let Some(value) = option_value(arg, &args, &mut index, "--temp-limit")? {
            note_once(&mut seen.temp_limit, "--temp-limit")?;
            options.config.resources.temp_limit = parse_size(parse_utf8(&value, "--temp-limit")?)?;
        } else if let Some(value) = option_value(arg, &args, &mut index, "--output-limit")? {
            note_once(&mut seen.output_limit, "--output-limit")?;
            options.config.resources.output_limit =
                parse_size(parse_utf8(&value, "--output-limit")?)?;
        } else if let Some(value) = option_value(arg, &args, &mut index, "--method")? {
            require_compression(command, "--method")?;
            note_once(&mut seen.method, "--method")?;
            selected_method = Some(Method::parse(parse_utf8(&value, "--method")?)?);
        } else if let Some(method) = method_alias(arg) {
            require_compression(command, arg.to_string_lossy().as_ref())?;
            note_once(&mut seen.method, "method selector")?;
            selected_method = Some(method);
        } else if let Some(value) = sidecar_value(arg, "--index")? {
            note_index(&mut seen, "--index")?;
            index_path = Some(value);
        } else if let Some(value) = sidecar_value(arg, "-index")? {
            note_index(&mut seen, "-index")?;
            index_path = Some(value);
        } else if arg != OsStr::new("-") && bytes.first() == Some(&b'-') {
            let text = parse_utf8(arg, "option")?;
            return Err(Error::invalid_config(format!("unknown option '{text}'")));
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }
    if command.allows_compression_options() {
        let method = selected_method.unwrap_or(Method::M3FixedDigest);
        let mut config = CompressionConfig::for_method(method);
        if let Some(layout) = selected_layout {
            config.layout = layout;
        }
        if let Some(checksum) = selected_checksum {
            config.checksum = checksum;
        }
        if let Some(block_size) = selected_block_size {
            config.block_size = block_size;
        }
        if let Some(min_match) = selected_min_match {
            config.min_match = min_match;
            if matches!(method, Method::M3FixedDigest | Method::M4Reread)
                && selected_seed_size.is_none()
            {
                config.seed_size = Some(min_match);
            }
        }
        if let Some(seed_size) = selected_seed_size {
            config.seed_size = Some(seed_size);
        }
        if let Some(target_chunk) = selected_target_chunk {
            config.target_chunk = Some(target_chunk);
        }
        if let Some(max_distance) = selected_max_distance {
            config.max_distance = Some(max_distance);
        }
        if rep_overlay || rep_distance.is_some() || rep_min_match.is_some() {
            if !rep_overlay {
                return Err(Error::invalid_config(
                    "--rep-distance and --rep-min-match require --rep-overlay",
                ));
            }
            let mut overlay = RepConfig::default();
            if let Some(distance) = rep_distance {
                overlay.distance = distance;
            }
            if let Some(min_match) = rep_min_match {
                overlay.min_match = min_match;
            }
            config.rep_overlay = Some(overlay);
        }
        config.resources = options.config.resources;
        options.config = config;
        options.config.validate()?;
    } else if options.config.resources.memory == 0
        || options.config.resources.temp_limit == 0
        || options.config.resources.output_limit == 0
        || options.config.resources.output_limit > srep::config::MAX_UNCOMPRESSED
    {
        return Err(Error::invalid_config(
            "resource limit is outside valid range",
        ));
    }
    if index_path.is_some() {
        return Err(Error::unsupported_legacy_split_index(
            "external legacy index is unsupported",
        ));
    }
    Ok((options, positional))
}

#[derive(Default)]
struct SeenOptions {
    force: bool,
    quiet: bool,
    layout: bool,
    checksum: bool,
    block_size: bool,
    min_match: bool,
    method: bool,
    seed_size: bool,
    target_chunk: bool,
    max_distance: bool,
    rep_overlay: bool,
    rep_distance: bool,
    rep_min_match: bool,
    memory: bool,
    temp_dir: bool,
    temp_limit: bool,
    output_limit: bool,
    index: bool,
}

fn note_once(flag: &mut bool, name: &str) -> Result<()> {
    if *flag {
        return Err(Error::invalid_config(format!("repeated option {name}")));
    }
    *flag = true;
    Ok(())
}

fn note_index(seen: &mut SeenOptions, name: &str) -> Result<()> {
    if seen.index {
        return Err(Error::invalid_config(format!(
            "repeated sidecar selector {name}"
        )));
    }
    seen.index = true;
    Ok(())
}

fn require_compression(command: CommandKind, name: &str) -> Result<()> {
    if !command.allows_compression_options() {
        return Err(Error::invalid_config(format!(
            "{name} is valid only for compress"
        )));
    }
    Ok(())
}

fn method_alias(arg: &OsStr) -> Option<Method> {
    match arg {
        value if value == OsStr::new("-m0") => Some(Method::M0Rep),
        value if value == OsStr::new("-m1") => Some(Method::M1RollingCdc),
        value if value == OsStr::new("-m2") => Some(Method::M2Order1Cdc),
        value if value == OsStr::new("-m3") => Some(Method::M3FixedDigest),
        value if value == OsStr::new("-m4") => Some(Method::M4Reread),
        value if value == OsStr::new("-m5") => Some(Method::M5Exhaustive),
        _ => None,
    }
}

fn option_value(
    arg: &OsStr,
    args: &[OsString],
    index: &mut usize,
    name: &str,
) -> Result<Option<OsString>> {
    let prefix = format!("{name}=");
    if arg == OsStr::new(name) {
        *index += 1;
        let value = args
            .get(*index)
            .ok_or_else(|| Error::invalid_config(format!("{name} needs a value")))?;
        return Ok(Some(value.clone()));
    }
    let bytes = arg.as_encoded_bytes();
    let prefix_bytes = prefix.as_bytes();
    if bytes.starts_with(prefix_bytes) {
        let rest = bytes[prefix_bytes.len()..].to_vec();
        return Ok(Some(os_from_vec(rest)));
    }
    Ok(None)
}

fn sidecar_value(arg: &OsStr, name: &str) -> Result<Option<OsString>> {
    if arg == OsStr::new(name) {
        return Err(Error::invalid_config(format!("{name}=PATH is required")));
    }
    let prefix = format!("{name}=");
    let bytes = arg.as_encoded_bytes();
    if bytes.starts_with(prefix.as_bytes()) {
        let rest = bytes[prefix.len()..].to_vec();
        if rest.is_empty() {
            return Err(Error::invalid_config(format!("{name}=PATH is required")));
        }
        return Ok(Some(os_from_vec(rest)));
    }
    Ok(None)
}

fn os_from_vec(bytes: Vec<u8>) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn parse_utf8<'a>(value: &'a OsStr, context: &str) -> Result<&'a str> {
    value
        .to_str()
        .ok_or_else(|| Error::invalid_config(format!("{context} must be valid UTF-8")))
}

fn open_input(path: &Path) -> Result<File> {
    File::open(path).map_err(Error::input_io)
}

fn compress_stream<R: Read, W: Write>(options: Options, input: R, output: W) -> Result<()> {
    let context = ResourceContext::with_resources(&options.config.resources)?;
    let stats = compress_with_context(input, output, &options.config, &context)?;
    if !options.quiet {
        eprintln!("compressed {} bytes", stats.original_size);
    }
    Ok(())
}

fn write_archive<R: Read>(
    path: &Path,
    force: bool,
    input: R,
    config: &CompressionConfig,
) -> Result<()> {
    let context = ResourceContext::with_resources(&config.resources)?;
    let (temp, writer) = temporary_output(path)?;
    let mut writer = srep::resource::BudgetedWriter::new(writer, &context.temp)?;
    let result = compress_with_context(input, &mut writer, config, &context);
    finish_temporary(path, temp, writer, result.map(|_| ()), force)
}

fn decompress_stream<R: Read, W: Write>(
    input: R,
    output: W,
    resources: &ResourceConfig,
) -> Result<()> {
    srep::decompress_with_resources(input, output, resources).map(|_| ())
}

fn decompress_to_path<R: Read>(
    path: &Path,
    force: bool,
    input: R,
    resources: &ResourceConfig,
) -> Result<()> {
    let context = ResourceContext::with_resources(resources)?;
    let (temp, writer) = temporary_output(path)?;
    let mut writer = srep::resource::BudgetedWriter::new(writer, &context.temp)?;
    let result = decompress_with_context(input, &mut writer, resources, &context);
    finish_temporary(path, temp, writer, result.map(|_| ()), force)
}

fn temporary_output(path: &Path) -> Result<(PathBuf, tempfile::NamedTempFile)> {
    if path == Path::new("-") {
        return Err(Error::invalid_config("internal output path error"));
    }
    let parent = normalized_parent(path);
    if fs::symlink_metadata(parent)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(Error::invalid_config(
            "refusing a destination whose parent is a symlink",
        ));
    }
    let temp = temp_builder(tempfile::Builder::new())
        .prefix(&temp_prefix(path))
        .tempfile_in(parent)
        .map_err(Error::temp_storage)?;
    let temp_path = temp.path().to_path_buf();
    Ok((temp_path, temp))
}

fn temp_builder<'a, 'b>(builder: tempfile::Builder<'a, 'b>) -> tempfile::Builder<'a, 'b> {
    #[cfg(unix)]
    {
        let mut builder = builder;
        builder.permissions(fs::Permissions::from_mode(0o600));
        builder
    }
    #[cfg(not(unix))]
    {
        builder
    }
}

fn finish_temporary(
    path: &Path,
    temp: PathBuf,
    mut writer: srep::resource::BudgetedWriter<tempfile::NamedTempFile>,
    result: Result<()>,
    force: bool,
) -> Result<()> {
    let result = result
        .and_then(|_| writer.flush().map_err(Error::output_io))
        .and_then(|_| {
            writer
                .inner_mut()
                .as_file_mut()
                .sync_all()
                .map_err(Error::output_io)
        });
    let budget_error = writer.budget_error().is_some();
    let (writer, _) = writer.into_parts();
    if let Err(error) = result {
        let _ = writer.close();
        let _ = fs::remove_file(&temp);
        if budget_error {
            return Err(Error::temp_limit(
                "temporary output exceeds temporary budget",
            ));
        }
        return Err(error);
    }
    if force {
        writer.persist(path).map(|_| ()).map_err(|error| {
            let _ = error.file.close();
            Error::atomic_publish(error.error)
        })
    } else {
        writer.persist_noclobber(path).map(|_| ()).map_err(|error| {
            let already_exists = error.error.kind() == io::ErrorKind::AlreadyExists;
            let source = error.error;
            let _ = error.file.close();
            if already_exists {
                Error::invalid_config(
                    "output exists; pass --force to replace a non-directory destination",
                )
            } else {
                Error::atomic_publish(source)
            }
        })
    }
}

fn normalized_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn temp_prefix(path: &Path) -> OsString {
    let mut name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("output"));
    name.push(".srep.tmp-");
    name
}

fn reject_input_collision(input: &Path, output: &Path) -> Result<()> {
    if existing_regular_entries_are_same(input, output)? {
        return Err(Error::invalid_config(
            "input and output refer to the same file",
        ));
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::input_io(error)),
    }
}

fn existing_regular_entries_are_same(input: &Path, output: &Path) -> Result<bool> {
    let input_metadata = fs::metadata(input).map_err(Error::input_io)?;
    let output_metadata = match fs::symlink_metadata(output) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::input_io(error)),
    };

    if output_metadata.file_type().is_symlink()
        || !input_metadata.is_file()
        || !output_metadata.is_file()
    {
        return Ok(false);
    }

    same_file::is_same_file(input, output).map_err(|error| {
        Error::with_source(
            ErrorKind::InputIo,
            format!("could not compare input and output identity: {error}"),
            error,
        )
    })
}

fn default_compress_path(input: &Path) -> Option<OsString> {
    if input == Path::new("-") {
        None
    } else {
        let mut result = OsString::from(input.as_os_str());
        result.push(".srep");
        Some(result)
    }
}

fn default_decompress_path(input: &Path) -> Option<OsString> {
    if input == Path::new("-") {
        return None;
    }
    strip_srep_suffix(input)
}

fn usage() -> &'static str {
    "Usage: srep <compress|decompress|info|test|--version|--help> [OPTIONS] INPUT [OUTPUT]\n\n\
     Software version (`srep --version` / `-V`, Cargo package version) is distinct from archive format. This CLI writes SREP-NG v3 by default; `srep info` reports the archive format family (SREP-NG v3 vs legacy SREP v1-v4).\n\
     SREP-NG v3 is the default archive format (per-block CRC32C plus global xxh3/blake3 digests). Stage 8 implements real m0 through m5 matching with a deterministic RAM-or-spill CandidateIndex; m5 is exhaustive and supports the REP overlay.\n\
     compress options: --method m0|m1|m2|m3|m4|m5 (-m0..-m5) --layout index|future|io --checksum xxh3|blake3 --block-size SIZE --min-match SIZE --seed-size SIZE --target-chunk SIZE --max-distance SIZE --rep-overlay --rep-distance SIZE --rep-min-match SIZE --force --quiet\n\
     --seed-size is valid for m3/m4 (default equals --min-match); --target-chunk is valid for m1/m2 (default 4096).\n\
other options: --force --quiet --memory SIZE --temp-dir PATH --temp-limit SIZE --output-limit SIZE\n\
SIZE units: B, KiB/K, MiB/M, GiB/G (binary powers of 1024); '-' means stdin/stdout\n\
default output suffix is .srep"
}

#[cfg(test)]
mod tests {
    use super::existing_regular_entries_are_same;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "srep-identity-test-{}-{}",
            std::process::id(),
            unique()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    fn unique() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn identity_helper_distinguishes_same_and_distinct_regular_entries() {
        let dir = temp_dir();
        let input = dir.join("input");
        let alias = dir.join("alias");
        let distinct = dir.join("distinct");
        fs::write(&input, b"input").unwrap();
        fs::hard_link(&input, &alias).unwrap();
        fs::write(&distinct, b"input").unwrap();

        assert!(existing_regular_entries_are_same(&input, &input).unwrap());
        assert!(existing_regular_entries_are_same(&input, &alias).unwrap());
        assert!(!existing_regular_entries_are_same(&input, &distinct).unwrap());
        assert!(!existing_regular_entries_are_same(&input, &dir.join("missing")).unwrap());

        fs::remove_dir_all(dir).unwrap();
    }
}
