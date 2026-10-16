use super::*;
#[test]
fn complete_record_including_framing_obeys_newc_u32_boundary_before_reserve() {
    let maximum = usize::try_from(u32::MAX).unwrap();
    assert_eq!(record_length(maximum - 16).unwrap(), maximum);
    assert!(record_length(maximum - 15).is_err());
    assert!(record_length(usize::MAX).is_err());
}
