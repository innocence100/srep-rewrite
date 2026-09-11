use srep::polynomial_hash;

#[test]
fn polynomial_hash_uses_wrapping_base_fold() {
    assert_eq!(polynomial_hash(&[]), 0);
    assert_eq!(polynomial_hash(&[1]), 1);
    assert_eq!(polynomial_hash(&[1, 2]), 153193);
    assert_eq!(polynomial_hash(b"abc"), 2_276_360_813_474);
}
