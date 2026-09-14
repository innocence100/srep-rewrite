#![forbid(unsafe_code)]

use std::env;
use std::path::PathBuf;

use srep::requirement::{AuditMode, audit_repository};

fn main() {
    if let Err(error) = run() {
        eprintln!("requirement-audit: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut mode = AuditMode::Check;
    let mut root = env::current_dir()?;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--check") => mode = AuditMode::Check,
            Some("--write") => mode = AuditMode::Write,
            Some("--root") => {
                root = PathBuf::from(args.next().ok_or("--root needs a path")?);
            }
            Some(value) => return Err(format!("unknown option '{value}'").into()),
            None => return Err("option is not valid UTF-8".into()),
        }
    }
    let counts = audit_repository(&root, mode)?;
    println!(
        "{} extracted; {} active; {} retired historical",
        counts.extracted, counts.active, counts.retired
    );
    Ok(())
}
