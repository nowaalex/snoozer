use super::*;

#[test]
fn detection_is_cached() {
    let first = capabilities() as *const Capabilities;
    let second = capabilities() as *const Capabilities;
    assert_eq!(first, second);
}
