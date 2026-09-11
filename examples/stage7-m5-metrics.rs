use std::env;
use std::io::Cursor;

use srep::{
    CompressionConfig, Method, ResourceConfig, find_matches_m5, normalize_matches_with_budget,
};

fn witness_input() -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"prefix-");
    data.extend_from_slice(b"abcabcabcabcabcabc");
    data.extend_from_slice(b"--gap--");
    data.extend_from_slice(b"abcabcabcabcabcabc");
    data.extend_from_slice(b"-suffix-");
    data.extend_from_slice(b"abcabcabcabcabcabc");
    data.push(b'\n');
    data
}

fn comparable_input() -> Vec<u8> {
    b"01234567ABCDEFGHIJKLMNOPQRSTUVWXYZ012345-----ABCDEFGHIJKLMNOPQRSTUVWXYZ012345\n".to_vec()
}

fn main() {
    let name = env::args().nth(1).unwrap_or_else(|| "m5".to_owned());
    assert!(
        matches!(name.as_str(), "m5" | "m5-comparable" | "m5-witness16"),
        "unknown evidence vector"
    );
    let data = if name == "m5-comparable" {
        comparable_input()
    } else {
        witness_input()
    };
    let resources = ResourceConfig {
        memory: 64 * 1024 * 1024,
        ..ResourceConfig::default()
    };
    let mut config = CompressionConfig::for_method(Method::M5Exhaustive);
    config.min_match = if matches!(name.as_str(), "m5-comparable" | "m5-witness16") {
        16
    } else {
        7
    };
    config.block_size = 8 * 1024;
    config.resources = resources.clone();
    let context = srep::ResourceContext::with_resources(&resources).unwrap();
    let candidates = find_matches_m5(Cursor::new(&data), &config, &context).unwrap();
    let raw_count = candidates.len();
    let witness = candidates.iter().find(|candidate| {
        candidate.dst % 4 != 0 && candidate.src % 4 == 0 && candidate.len >= config.min_match
    });
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
    if let Some(candidate) = witness {
        print!(
            ",\"m5_witness\":{{\"source\":{},\"destination\":{},\"length\":{},\"nonaligned_target\":true}}",
            candidate.src, candidate.dst, candidate.len
        );
    }
    println!("}}");
}
