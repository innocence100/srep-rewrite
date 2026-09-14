//! Checksum primitives shared by SREP-NG v3 and the historical readers.
//!
//! XXH3-128 uses `twox-hash` 2.1.4 with the official default secret and seed
//! zero, serialized as low64 little-endian then high64 little-endian (see
//! [`encode_xxh3`]); BLAKE3-256 is serialized as the raw 32-byte digest.
//!
//! The unit tests below check internal consistency (oneshot vs. streaming
//! within the same crate). Independent evidence for both the algorithm output
//! and the serialization is maintained separately: the golden fixture
//! `tests/fixtures/checksum/xxh3_128_reference.json` is generated from the
//! official xxHash C reference by `scripts/checksum-generate-goldens.sh` and
//! consumed by `tests/checksum_vectors.rs`. See `docs/checksum-audit.md` for
//! the dependency unsafe inventory, cross-platform behaviour, and evidence
//! status.

use crate::config::Checksum;
use blake3::Hasher as Blake3Hasher;
use twox_hash::XxHash3_128;

#[derive(Clone)]
enum Inner {
    Xxh3(Box<XxHash3_128>),
    Blake3(Box<Blake3Hasher>),
}

#[derive(Clone)]
pub struct Digest {
    inner: Inner,
}

impl Digest {
    pub fn new(kind: Checksum) -> Self {
        Self {
            inner: match kind {
                Checksum::Xxh3 => Inner::Xxh3(Box::new(XxHash3_128::new())),
                Checksum::Blake3 => Inner::Blake3(Box::new(Blake3Hasher::new())),
            },
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        match &mut self.inner {
            Inner::Xxh3(hasher) => hasher.write(bytes),
            Inner::Blake3(hasher) => {
                hasher.update(bytes);
            }
        }
    }

    pub fn finalize(self) -> Vec<u8> {
        match self.inner {
            Inner::Xxh3(hasher) => encode_xxh3(hasher.finish_128()),
            Inner::Blake3(hasher) => hasher.finalize().as_bytes().to_vec(),
        }
    }
}

pub fn oneshot(kind: Checksum, bytes: &[u8]) -> Vec<u8> {
    match kind {
        Checksum::Xxh3 => encode_xxh3(XxHash3_128::oneshot(bytes)),
        Checksum::Blake3 => blake3::hash(bytes).as_bytes().to_vec(),
    }
}

pub fn encode_xxh3(value: u128) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    let low = value as u64;
    let high = (value >> 64) as u64;
    out.extend_from_slice(&low.to_le_bytes());
    out.extend_from_slice(&high.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xxh3_vector(input: &[u8]) -> Vec<u8> {
        encode_xxh3(XxHash3_128::oneshot(input))
    }

    #[test]
    fn xxh3_empty_and_abc_match_oneshot_and_streaming() {
        for input in [b"" as &[u8], b"abc", b"0123456789", &[0u8; 241]] {
            let expected = xxh3_vector(input);
            assert_eq!(oneshot(Checksum::Xxh3, input), expected);
            let mut hasher = Digest::new(Checksum::Xxh3);
            hasher.update(input);
            assert_eq!(hasher.finalize(), expected);
        }
    }

    #[test]
    fn xxh3_chunk_splits_match_oneshot() {
        let input: Vec<u8> = (0u8..=255).cycle().take(10_000).collect();
        let expected = oneshot(Checksum::Xxh3, &input);
        for chunk in [1usize, 16, 31, 240, 241, 1024, 4096] {
            let mut hasher = Digest::new(Checksum::Xxh3);
            for piece in input.chunks(chunk) {
                hasher.update(piece);
            }
            assert_eq!(hasher.finalize(), expected, "chunk size {chunk}");
        }
    }

    #[test]
    fn xxh3_serialization_is_low64_le_then_high64_le() {
        let value = XxHash3_128::oneshot(b"srep-ng-v3");
        let encoded = encode_xxh3(value);
        assert_eq!(&encoded[..8], &(value as u64).to_le_bytes());
        assert_eq!(&encoded[8..], &((value >> 64) as u64).to_le_bytes());
        assert_eq!(encoded.len(), 16);
    }

    fn blake3_input(len: usize) -> Vec<u8> {
        (0u8..=250).cycle().take(len).collect()
    }

    #[test]
    fn blake3_official_vectors() {
        let cases = [
            (
                0usize,
                "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            ),
            (
                1,
                "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213",
            ),
            (
                1024,
                "42214739f095a406f3fc83deb889744ac00df831c10daa55189b5d121c855af7",
            ),
        ];
        for (len, hex) in cases {
            let input = blake3_input(len);
            let digest = oneshot(Checksum::Blake3, &input);
            assert_eq!(hex::encode_or_debug(&digest), hex, "len {len}");
            let mut hasher = Digest::new(Checksum::Blake3);
            hasher.update(&input);
            assert_eq!(hasher.finalize(), digest);
        }
    }

    mod hex {
        pub fn encode_or_debug(bytes: &[u8]) -> String {
            bytes.iter().map(|byte| format!("{byte:02x}")).collect()
        }
    }
}
