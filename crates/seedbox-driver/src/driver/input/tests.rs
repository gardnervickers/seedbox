use super::*;
#[test]
fn keys_accept_only_the_documented_alphabet_and_length() {
    for valid in ["a", "A-z_0", &"k".repeat(128)] {
        assert_eq!(InputKey::new(valid).unwrap().as_str(), valid);
    }
    for invalid in ["", " ", "a.b", "é", "a/b", &"k".repeat(129)] {
        assert_eq!(
            InputKey::new(invalid),
            Err(InvalidInputKey(invalid.to_owned()))
        );
    }
}
