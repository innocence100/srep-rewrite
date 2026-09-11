const BASE: u64 = 153_191;

/// Computes the specified wrapping polynomial hash.
pub const fn polynomial_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0u64;
    let mut index = 0usize;
    while index < bytes.len() {
        hash = hash.wrapping_mul(BASE).wrapping_add(bytes[index] as u64);
        index += 1;
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_is_explicit_for_long_inputs() {
        let bytes = [0xff; 32];
        let mut expected = 0u64;
        for byte in bytes {
            expected = expected.wrapping_mul(BASE).wrapping_add(byte as u64);
        }
        assert_eq!(polynomial_hash(&bytes), expected);
    }
}
