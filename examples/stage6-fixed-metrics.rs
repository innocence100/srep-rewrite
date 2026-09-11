use std::env;
use std::io::Cursor;

use srep::{
    CompressionConfig, Method, ResourceConfig, find_matches_m3, find_matches_m4,
    normalize_matches_with_budget,
};

fn input(name: &str) -> Vec<u8> {
    match name {
        "m3" | "m4" => (0u8..24).cycle().take(240).collect(),
        "m3-conformance" => b"abc"
            .repeat(20)
            .into_iter()
            .chain(b"XYZ".repeat(7))
            .chain(b"abc".repeat(20))
            .collect(),
        _ => panic!("unknown evidence vector"),
    }
}

fn main() {
    let name = env::args().nth(1).expect("evidence vector");
    let (method, seed, minimum) = match name.as_str() {
        "m3" | "m4" => (Method::M3FixedDigest, 16, 16),
        "m3-conformance" => (Method::M3FixedDigest, 3, 7),
        _ => (Method::M4Reread, 16, 16),
    };
    let method = if name == "m4" {
        Method::M4Reread
    } else {
        method
    };
    let resources = ResourceConfig {
        memory: 64 * 1024 * 1024,
        ..ResourceConfig::default()
    };
    let mut config = CompressionConfig::for_method(method);
    config.seed_size = Some(seed);
    config.min_match = minimum;
    config.resources = resources.clone();
    let context = srep::ResourceContext::with_resources(&resources).unwrap();
    let data = input(&name);
    let candidates = match method {
        Method::M3FixedDigest => find_matches_m3(Cursor::new(&data), &config, &context),
        Method::M4Reread => find_matches_m4(Cursor::new(&data), &config, &context),
        _ => unreachable!(),
    }
    .unwrap();
    let raw_count = candidates.len();
    let witness = if method == Method::M4Reread {
        candidates.iter().find_map(|candidate| {
            (seed + 1..data.len() as u64).find_map(|backward| {
                let source = candidate.src.checked_add(backward)?;
                let target = candidate.dst.checked_add(backward)?;
                if source % seed != 0
                    || source + seed > data.len() as u64
                    || target + seed > data.len() as u64
                    || candidate.len < backward + seed
                    || data[source as usize..(source + seed) as usize]
                        != data[target as usize..(target + seed) as usize]
                {
                    return None;
                }
                Some((
                    source,
                    target,
                    backward,
                    candidate.src,
                    candidate.dst,
                    candidate.len,
                ))
            })
        })
    } else {
        None
    };
    let normalized = normalize_matches_with_budget(
        candidates.iter().copied(),
        data.len() as u64,
        config.effective_min_match().unwrap(),
        &context.memory,
    )
    .unwrap();
    print!(
        "{{\"raw_candidate_count\":{},\"normalized_match_count\":{},\"covered_bytes\":{},\"literal_bytes\":{}",
        raw_count,
        normalized.matches.len(),
        normalized.covered_bytes,
        normalized.literal_bytes
    );
    if let Some((seed_source, seed_target, backward, source, destination, length)) = witness {
        print!(
            ",\"m4_witness\":{{\"seed_source\":{},\"seed_target\":{},\"backward_bytes\":{},\"source\":{},\"destination\":{},\"length\":{}}}",
            seed_source, seed_target, backward, source, destination, length
        );
    }
    println!("}}");
}
