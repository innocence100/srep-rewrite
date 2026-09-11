use std::io::Read;

use crate::error::{Error, Result};
use crate::format::{
    HEADER_LEN, LEGACY_SIGNATURE, NG_V2_MAGIC, PROTOTYPE_V1_MAGIC, parse_archive_header,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    NgV2,
    PrototypeV1,
    Legacy,
}

pub fn classify_prefix(prefix: &[u8]) -> Result<ArchiveKind> {
    if prefix.len() >= 8 {
        let magic: [u8; 8] = prefix[..8].try_into().expect("prefix length");
        if magic == NG_V2_MAGIC {
            return Ok(ArchiveKind::NgV2);
        }
        if magic == PROTOTYPE_V1_MAGIC || magic.starts_with(b"SREPNG\0") {
            return Err(Error::unsupported_version(
                "experimental SREP-NG v1 is not supported",
            ));
        }
        if magic[..4] == LEGACY_SIGNATURE[..4] {
            if magic[4..] == LEGACY_SIGNATURE[4..] {
                return Ok(ArchiveKind::Legacy);
            }
            return Err(Error::corrupt_header("invalid legacy signature words"));
        }
        return Err(Error::corrupt_header(
            "unknown magic (not SREP-NG v2 or legacy SREP)",
        ));
    }
    if prefix.is_empty() {
        return Err(Error::corrupt_header("missing archive magic"));
    }
    if NG_V2_MAGIC.starts_with(prefix)
        || PROTOTYPE_V1_MAGIC.starts_with(prefix)
        || LEGACY_SIGNATURE.starts_with(prefix)
        || b"SREPNG\0".starts_with(prefix)
    {
        return Err(Error::truncated("truncated archive magic"));
    }
    Err(Error::corrupt_header(
        "unknown magic (not SREP-NG v2 or legacy SREP)",
    ))
}

pub fn read_and_classify<R: Read>(reader: &mut R) -> Result<(ArchiveKind, Vec<u8>)> {
    let mut magic = [0u8; 8];
    let mut read = 0usize;
    while read < 8 {
        match reader.read(&mut magic[read..]) {
            Ok(0) => {
                return classify_prefix(&magic[..read]).map(|_| unreachable!());
            }
            Ok(n) => read += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::input_io(error)),
        }
    }
    match classify_prefix(&magic)? {
        ArchiveKind::NgV2 => {
            let mut header = vec![0u8; HEADER_LEN];
            header[..8].copy_from_slice(&magic);
            reader
                .read_exact(&mut header[8..])
                .map_err(|error| Error::map_eof(error, "truncated NG v2 archive header"))?;
            let parsed = parse_archive_header(&header)?;
            if parsed.version != 2 {
                return Err(Error::unsupported_version(format!(
                    "SREP-NG version {} is not supported",
                    parsed.version
                )));
            }
            Ok((ArchiveKind::NgV2, header))
        }
        ArchiveKind::PrototypeV1 => Err(Error::unsupported_version(
            "experimental SREP-NG v1 is not supported",
        )),
        ArchiveKind::Legacy => {
            let mut rest = [0u8; 8];
            reader
                .read_exact(&mut rest)
                .map_err(|error| Error::map_eof(error, "truncated legacy archive header"))?;
            let packed = u32::from_le_bytes(rest[0..4].try_into().unwrap());
            let version = (packed & 0xff) as u8;
            if !(1..=4).contains(&version) {
                return Err(Error::unsupported_version(format!(
                    "legacy SREP version {version} is not supported"
                )));
            }
            let seed_len = ((packed >> 16) & 0xff) as usize;
            crate::legacy::header::descriptor(packed)?;
            let mut header = Vec::with_capacity(16 + seed_len);
            header.extend_from_slice(&magic);
            header.extend_from_slice(&rest);
            let mut seed = vec![0u8; seed_len];
            reader
                .read_exact(&mut seed)
                .map_err(|error| Error::map_eof(error, "truncated legacy archive seed"))?;
            header.extend_from_slice(&seed);
            Ok((ArchiveKind::Legacy, header))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prototype_v1_is_unsupported_version() {
        let error = classify_prefix(&PROTOTYPE_V1_MAGIC).unwrap_err();
        assert_eq!(error.code(), 3);
        assert_eq!(error.message_id(), "SREP_E_UNSUPPORTED_VERSION");
    }

    #[test]
    fn unknown_magic_is_corrupt_header() {
        let error = classify_prefix(b"NOTSREP!").unwrap_err();
        assert_eq!(error.code(), 6);
        assert_eq!(error.message_id(), "SREP_E_CORRUPT_HEADER");
    }

    #[test]
    fn legacy_signature_is_recognized_and_header_is_returned() {
        assert_eq!(
            classify_prefix(&LEGACY_SIGNATURE).unwrap(),
            ArchiveKind::Legacy
        );
        let mut prefix = LEGACY_SIGNATURE.to_vec();
        prefix.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
        let (kind, header) = read_and_classify(&mut prefix.as_slice()).unwrap();
        assert_eq!(kind, ArchiveKind::Legacy);
        assert_eq!(header.len(), 16);
    }

    #[test]
    fn truncated_known_magic_is_truncated() {
        let error = classify_prefix(b"SREPNG").unwrap_err();
        assert_eq!(error.code(), 7);
    }
}
