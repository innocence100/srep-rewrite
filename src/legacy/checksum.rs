use aes::Aes256;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha512};
use siphasher::sip::SipHasher24;
use std::hash::Hasher;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksumKind {
    Md5,
    None,
    Sha1,
    Sha512,
    Vhash,
    SipHash,
}
impl ChecksumKind {
    pub const fn width(self) -> usize {
        match self {
            Self::Md5 | Self::None | Self::Vhash => 16,
            Self::Sha1 => 20,
            Self::Sha512 => 64,
            Self::SipHash => 8,
        }
    }
    pub const fn seed_len(self) -> usize {
        match self {
            Self::Vhash => 32,
            Self::SipHash => 16,
            _ => 0,
        }
    }
    pub const fn name(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::None => "none",
            Self::Sha1 => "sha1",
            Self::Sha512 => "sha512",
            Self::Vhash => "vhash",
            Self::SipHash => "siphash",
        }
    }
}
#[cfg(test)]
fn digest(kind: ChecksumKind, key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut hasher = LegacyHasher::new(kind, key).expect("validated legacy checksum descriptor");
    hasher.update(data);
    hasher.finalize()
}
pub struct LegacyHasher {
    inner: HasherInner,
}

enum HasherInner {
    Md5(Md5),
    None,
    Sha1(Sha1),
    Sha512(Sha512),
    Sip(SipHasher24),
    Vhash(Box<VhashState>),
}

impl LegacyHasher {
    pub fn new(kind: ChecksumKind, key: &[u8]) -> crate::error::Result<Self> {
        let inner = match kind {
            ChecksumKind::Md5 => HasherInner::Md5(Md5::new()),
            ChecksumKind::None => HasherInner::None,
            ChecksumKind::Sha1 => HasherInner::Sha1(Sha1::new()),
            ChecksumKind::Sha512 => HasherInner::Sha512(Sha512::new()),
            ChecksumKind::SipHash => {
                HasherInner::Sip(SipHasher24::new_with_key(key.try_into().map_err(|_| {
                    crate::error::Error::corrupt_header("invalid SipHash seed")
                })?))
            }
            ChecksumKind::Vhash => HasherInner::Vhash(Box::new(VhashState::new(key)?)),
        };
        Ok(Self { inner })
    }

    pub fn update(&mut self, bytes: &[u8]) {
        match &mut self.inner {
            HasherInner::Md5(h) => h.update(bytes),
            HasherInner::None => {}
            HasherInner::Sha1(h) => h.update(bytes),
            HasherInner::Sha512(h) => h.update(bytes),
            HasherInner::Sip(h) => h.write(bytes),
            HasherInner::Vhash(h) => h.update(bytes),
        }
    }

    pub fn finalize(self) -> Vec<u8> {
        match self.inner {
            HasherInner::Md5(h) => h.finalize().to_vec(),
            HasherInner::None => vec![0; 16],
            HasherInner::Sha1(h) => h.finalize().to_vec(),
            HasherInner::Sha512(h) => h.finalize().to_vec(),
            HasherInner::Sip(h) => h.finish().to_le_bytes().to_vec(),
            HasherInner::Vhash(h) => h.finalize(),
        }
    }
}

pub fn verify_reader(
    kind: ChecksumKind,
    key: &[u8],
    output: &mut super::storage::OutputStore,
    start: u64,
    len: u64,
    expected: &[u8],
) -> crate::error::Result<()> {
    if kind == ChecksumKind::None {
        return Ok(());
    }
    let mut hasher = LegacyHasher::new(kind, key)?;
    let mut buffer = [0u8; 8192];
    let mut position = start;
    let mut remaining = len;
    while remaining > 0 {
        let amount = remaining.min(buffer.len() as u64) as usize;
        output.read_at(position, &mut buffer[..amount])?;
        hasher.update(&buffer[..amount]);
        position = position.checked_add(amount as u64).ok_or_else(|| {
            crate::error::Error::corrupt_record("legacy checksum input offset overflows")
        })?;
        remaining -= amount as u64;
    }
    if hasher.finalize() != expected {
        return Err(crate::error::Error::checksum_mismatch(
            "legacy reconstructed-block checksum mismatch",
        ));
    }
    Ok(())
}

struct VhashState {
    nk: [u64; 514],
    pk: [u128; 2],
    lk: [u64; 4],
    ac: [u128; 2],
    blocks: u64,
    tail: [u8; 4095],
    tail_len: usize,
}

impl VhashState {
    fn new(key: &[u8]) -> crate::error::Result<Self> {
        let cipher = Aes256::new_from_slice(key)
            .map_err(|_| crate::error::Error::corrupt_header("invalid VHASH seed"))?;
        let mut nk = [0u64; 514];
        for i in 0..257 {
            let block = kdf_block(&cipher, 0x80, i as u8);
            nk[i * 2] = u64::from_be_bytes(block[..8].try_into().unwrap());
            nk[i * 2 + 1] = u64::from_be_bytes(block[8..].try_into().unwrap());
        }
        let mut pk = [0u128; 2];
        for (i, slot) in pk.iter_mut().enumerate() {
            let block = kdf_block(&cipher, 0xc0, i as u8);
            let a = u64::from_be_bytes(block[..8].try_into().unwrap()) & 0x1fffffff1fffffff;
            let b = u64::from_be_bytes(block[8..].try_into().unwrap()) & 0x1fffffff1fffffff;
            *slot = ((a as u128) << 64) | b as u128;
        }
        let lk = select_l3_keys(|counter| {
            let block = kdf_block(&cipher, 0xe0, counter);
            (
                u64::from_be_bytes(block[..8].try_into().unwrap()),
                u64::from_be_bytes(block[8..].try_into().unwrap()),
            )
        })?;
        Ok(Self {
            nk,
            pk,
            lk,
            ac: pk,
            blocks: 0,
            tail: [0; 4095],
            tail_len: 0,
        })
    }

    fn update(&mut self, mut bytes: &[u8]) {
        if self.tail_len != 0 {
            let needed = 4096 - self.tail_len;
            if bytes.len() < needed {
                self.tail[self.tail_len..self.tail_len + bytes.len()].copy_from_slice(bytes);
                self.tail_len += bytes.len();
                return;
            }
            let mut block = [0u8; 4096];
            block[..self.tail_len].copy_from_slice(&self.tail[..self.tail_len]);
            block[self.tail_len..].copy_from_slice(&bytes[..needed]);
            self.consume_block(&block);
            self.tail_len = 0;
            bytes = &bytes[needed..];
        }
        while bytes.len() >= 4096 {
            let block: &[u8; 4096] = bytes[..4096].try_into().unwrap();
            self.consume_block(block);
            bytes = &bytes[4096..];
        }
        if !bytes.is_empty() {
            self.tail[..bytes.len()].copy_from_slice(bytes);
            self.tail_len = bytes.len();
        }
    }

    fn consume_block(&mut self, message: &[u8; 4096]) {
        let x = [nh(message, &self.nk, 0), nh(message, &self.nk, 2)];
        if self.blocks == 0 {
            self.ac[0] = add(self.ac[0], x[0]);
            self.ac[1] = add(self.ac[1], x[1]);
        } else {
            self.ac[0] = add(mul(self.ac[0], self.pk[0]), x[0]);
            self.ac[1] = add(mul(self.ac[1], self.pk[1]), x[1]);
        }
        self.blocks += 1;
    }

    fn finalize(self) -> Vec<u8> {
        let mut ac = self.ac;
        if self.tail_len != 0 {
            let mut block = [0u8; 4096];
            block[..self.tail_len].copy_from_slice(&self.tail[..self.tail_len]);
            let padded = self.tail_len.div_ceil(16) * 16;
            let x = [
                nh(&block[..padded], &self.nk, 0),
                nh(&block[..padded], &self.nk, 2),
            ];
            if self.blocks == 0 {
                ac[0] = add(ac[0], x[0]);
                ac[1] = add(ac[1], x[1]);
            } else {
                ac[0] = add(mul(ac[0], self.pk[0]), x[0]);
                ac[1] = add(mul(ac[1], self.pk[1]), x[1]);
            }
        }
        let bits = (self.tail_len as u64) * 8;
        let mut result = [0u8; 16];
        result[..8].copy_from_slice(&l3(ac[0], self.lk[0], self.lk[1], bits).to_le_bytes());
        result[8..].copy_from_slice(&l3(ac[1], self.lk[2], self.lk[3], bits).to_le_bytes());
        result.to_vec()
    }
}

fn kdf_block(cipher: &Aes256, tag: u8, counter: u8) -> [u8; 16] {
    let mut input = [0u8; 16];
    input[0] = tag;
    input[15] = counter;
    let mut block = GenericArray::clone_from_slice(&input);
    cipher.encrypt_block(&mut block);
    block.into()
}

fn select_l3_keys<F>(mut trial: F) -> crate::error::Result<[u64; 4]>
where
    F: FnMut(u8) -> (u64, u64),
{
    let limit = u64::MAX - 256;
    let mut keys = [0u64; 4];
    let mut found = 0;
    for counter in 0..=u8::MAX {
        let (a, b) = trial(counter);
        if a < limit && b < limit {
            keys[found * 2] = a;
            keys[found * 2 + 1] = b;
            found += 1;
            if found == 2 {
                return Ok(keys);
            }
        }
    }
    Err(crate::error::Error::corrupt_header("invalid VHASH L3 key"))
}

fn nh(message: &[u8], nk: &[u64; 514], offset: usize) -> u128 {
    let mut sum = 0u128;
    for i in (0..message.len() / 8).step_by(2) {
        let a = u64::from_le_bytes(message[i * 8..i * 8 + 8].try_into().unwrap())
            .wrapping_add(nk[offset + i]);
        let b = u64::from_le_bytes(message[(i + 1) * 8..(i + 2) * 8].try_into().unwrap())
            .wrapping_add(nk[offset + i + 1]);
        sum = sum.wrapping_add(a as u128 * b as u128);
    }
    sum & ((1u128 << 126) - 1)
}

const P127: u128 = (1u128 << 127) - 1;
const P64: u64 = u64::MAX - 256;

fn add(a: u128, b: u128) -> u128 {
    let z = ((a + b) & P127) + ((a + b) >> 127);
    if z >= P127 { z - P127 } else { z }
}

fn mul(mut a: u128, mut b: u128) -> u128 {
    let mut result = 0;
    for _ in 0..127 {
        if b & 1 != 0 {
            result = add(result, a);
        }
        b >>= 1;
        a = add(a, a);
    }
    result
}

fn l3(x: u128, k1: u64, k2: u64, bits: u64) -> u64 {
    let x = add(x, (bits as u128) << 64);
    let d = (1u128 << 64) - (1u128 << 32);
    let a = (x / d + k1 as u128) % P64 as u128;
    let z = (x % d + k2 as u128) % P64 as u128;
    (a * z % P64 as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i.wrapping_mul(131).wrapping_add(17)) as u8)
            .collect()
    }

    #[test]
    fn vhash_normative_k0_vectors() {
        let key: Vec<u8> = (0u8..=31).collect();
        let cases = [
            (0, "2eca48aefe4117ae20f2769dbdfe8de3"),
            (1, "aebfc5095e65fdc1a95b509b186e53bd"),
            (15, "2d5a98646d8f36eb1ae9aa4196fd762a"),
            (16, "b73b99cadcaf7b1da7a5af7434e74a58"),
            (17, "7aec06e5465bb76cbce2c23334eb11d9"),
            (4095, "4b85e8153acc14a3c435b708fa88a6a6"),
            (4096, "5d9a25548b5a82a6e84e87c5704ae401"),
            (4097, "82396c89fd1e58762c8c9b8d4502b5bb"),
        ];
        for (len, expected) in cases {
            let actual = digest(ChecksumKind::Vhash, &key, &message(len));
            let text: String = actual.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(text, expected, "length {len}");
        }
    }

    #[test]
    fn vhash_l3_advances_past_rejected_trials() {
        let mut calls = 0;
        let keys = select_l3_keys(|counter| {
            calls += 1;
            if counter < 5 {
                (u64::MAX, 0)
            } else if counter < 7 {
                (0, 1)
            } else {
                (u64::MAX, u64::MAX)
            }
        })
        .unwrap();
        assert_eq!(calls, 7);
        assert_eq!(keys, [0, 1, 0, 1]);
    }

    #[test]
    fn vhash_l3_rejects_a_full_counter_cycle() {
        let mut calls = 0;
        let error = select_l3_keys(|_| {
            calls += 1;
            (u64::MAX, u64::MAX)
        })
        .unwrap_err();
        assert_eq!(calls, 256);
        assert_eq!(error.kind(), crate::error::ErrorKind::CorruptHeader);
    }

    #[test]
    fn vhash_normative_k1_and_k2_vectors() {
        let cases = [
            (
                vec![0u8; 32],
                [
                    (0, "4827d443f5773a0b23ac4c6be4b03f67"),
                    (1, "dd70f6c48a19e66074e622ea73f460e8"),
                    (15, "f1b1821ab671d8ab0b4b29dca6f918b3"),
                    (16, "4b7aac6d5111d382f1a7fd8210b4ac02"),
                    (17, "06e3cd6366ff58f53c12c67c8253a0c2"),
                    (4095, "67ee988e726d950013d4bb8dd96255af"),
                    (4096, "640817c8a472fda17359f45b1a95f8a4"),
                    (4097, "21077486253a269eb04176c17ab300e3"),
                ],
            ),
            (
                (0u8..=31).map(|value| 255 - value).collect(),
                [
                    (0, "683e9378e4024ee7239278b91ae6bc05"),
                    (1, "7a631b51a6ba3cd66a9ef2118c067adb"),
                    (15, "4ddfc05d5e5148fb56ef11481651b9c3"),
                    (16, "823806e710a389c6fd1cd6644eda8f98"),
                    (17, "695525c8e43f5ab010404218c7a5515c"),
                    (4095, "320aa79597a5151f932eacc97dffd548"),
                    (4096, "89bf1ed93c224d93ec00f2409c76d734"),
                    (4097, "82dc88ce641347fa995a052da1770817"),
                ],
            ),
        ];
        for (key, vectors) in cases {
            for (len, expected) in vectors {
                let actual = digest(ChecksumKind::Vhash, &key, &message(len));
                let text: String = actual.iter().map(|b| format!("{b:02x}")).collect();
                assert_eq!(text, expected, "length {len}");
            }
        }
    }

    #[test]
    fn vhash_incremental_splits_match_one_shot() {
        let key: Vec<u8> = (0u8..=31).collect();
        for len in [0usize, 1, 15, 16, 17, 4095, 4096, 4097] {
            let input = message(len);
            let expected = digest(ChecksumKind::Vhash, &key, &input);
            for split in [1usize, 15, 16, 4095, 4096] {
                let mut hasher = LegacyHasher::new(ChecksumKind::Vhash, &key).unwrap();
                for chunk in input.chunks(split) {
                    hasher.update(chunk);
                }
                assert_eq!(hasher.finalize(), expected, "length {len}, split {split}");
            }
        }
    }

    #[test]
    fn historical_vhash_archive_tuples_match_exactly() {
        let rows = [
            (
                256usize,
                "0f5acc75c23306230270e4f1e2ac3525f4d18b32bba18f76686b7e9555dd275e",
                "623842bf997fd783fe3abcdbab4305a7",
            ),
            (
                4095,
                "4fb32443d528a88263036a03a329e83fb60c8cad72b5f823126713e5884deb38",
                "cf5707a6fca43381caeb2161e2c5045c",
            ),
            (
                4096,
                "aefc733df24360ef00b7403ab141eaae3552884c485f444d898be4ff54181966",
                "5c4e6d9ce4345c76f97fba98c6fb330f",
            ),
            (
                4097,
                "57260813db7992c42bf0bb3ddb6b755cc5fc54555c45bed688fd8c80216fe62b",
                "d8a5b44516b723ed332df24ac5e13949",
            ),
            (
                8192,
                "08fb1cda79bbe7804dfcbb67516b0dc93c9f407a8f979a46ef826954f8889159",
                "f0494a0189b8f8fb8dee16434837f6c9",
            ),
        ];
        for (len, key_hex, expected) in rows {
            let mut key = [0u8; 32];
            for (index, pair) in key_hex.as_bytes().chunks(2).enumerate() {
                key[index] = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
            }
            let actual = digest(ChecksumKind::Vhash, &key, &message(len));
            let text: String = actual.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(text, expected, "length {len}");
        }
    }

    #[test]
    fn seeded_hashes_have_expected_widths() {
        assert_eq!(digest(ChecksumKind::Md5, &[], b"abc").len(), 16);
        assert_eq!(digest(ChecksumKind::None, &[], b"abc").len(), 16);
        assert_eq!(digest(ChecksumKind::Sha1, &[], b"abc").len(), 20);
        assert_eq!(digest(ChecksumKind::Sha512, &[], b"abc").len(), 64);
        assert_eq!(digest(ChecksumKind::SipHash, &[0; 16], b"abc").len(), 8);
        assert_eq!(digest(ChecksumKind::Vhash, &[0; 32], b"abc").len(), 16);
    }
}
