use crate::config::Checksum;
use crate::error::{Error, Result};
use blake3::Hasher as Blake3Hasher;
use twox_hash::XxHash3_128;

const RECORD_DOMAIN: &[u8] = b"SREPNG2-RECORD\0";
const BLOCK_DOMAIN: &[u8] = b"SREPNG2-BLOCK\0";
const ARCHIVE_DOMAIN: &[u8] = b"SREPNG2-ARCHIVE\0";

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

pub fn record_checksum(kind: Checksum, frame: &[u8; 12], payload: &[u8]) -> Vec<u8> {
    let mut digest = record_digest_start(kind, frame);
    digest.update(payload);
    digest.finalize()
}

pub fn record_digest_start(kind: Checksum, frame: &[u8; 12]) -> Digest {
    let mut digest = Digest::new(kind);
    digest.update(RECORD_DOMAIN);
    digest.update(frame);
    digest
}

pub fn block_checksum(
    kind: Checksum,
    frame: &[u8; 12],
    payload: &[u8],
    block_id: u64,
    dst_start: u64,
    uncompressed: &[u8],
) -> Result<Vec<u8>> {
    let mut digest = block_digest_start(kind, frame);
    digest.update(payload);
    digest.update(&block_id.to_le_bytes());
    digest.update(&dst_start.to_le_bytes());
    let len = u64::try_from(uncompressed.len())
        .map_err(|_| Error::corrupt_record("uncompressed length overflows u64"))?;
    digest.update(&len.to_le_bytes());
    digest.update(uncompressed);
    Ok(digest.finalize())
}

pub fn block_digest_start(kind: Checksum, frame: &[u8; 12]) -> Digest {
    let mut digest = Digest::new(kind);
    digest.update(BLOCK_DOMAIN);
    digest.update(frame);
    digest
}

pub fn archive_digest(
    kind: Checksum,
    header: &[u8],
    method_parameters: &[u8],
    layout_metadata: &[u8],
    uncompressed: &[u8],
) -> Vec<u8> {
    let mut digest = Digest::new(kind);
    update_archive_digest(
        &mut digest,
        header,
        method_parameters,
        layout_metadata,
        uncompressed,
    );
    digest.finalize()
}

pub fn archive_digest_start(
    kind: Checksum,
    header: &[u8],
    method_parameters: &[u8],
    layout_metadata: &[u8],
) -> Digest {
    let mut digest = Digest::new(kind);
    digest.update(ARCHIVE_DOMAIN);
    digest.update(header);
    digest.update(method_parameters);
    digest.update(layout_metadata);
    digest
}

fn update_archive_digest(
    digest: &mut Digest,
    header: &[u8],
    method_parameters: &[u8],
    layout_metadata: &[u8],
    uncompressed: &[u8],
) {
    digest.update(ARCHIVE_DOMAIN);
    digest.update(header);
    digest.update(method_parameters);
    digest.update(layout_metadata);
    digest.update(uncompressed);
}

pub fn verify_bytes(kind: Checksum, expected: &[u8], actual: &[u8], context: &str) -> Result<()> {
    if expected != actual {
        return Err(Error::checksum_mismatch(context));
    }
    if expected.len() != kind.width() {
        return Err(Error::checksum_mismatch("checksum width mismatch"));
    }
    Ok(())
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
        let value = XxHash3_128::oneshot(b"srep-ng-v2");
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
