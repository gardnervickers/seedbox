#[test]
fn rtc_base_is_the_utc_civil_time_of_the_epoch() {
    assert_eq!(super::utc(0), "1970-01-01T00:00:00");
    assert_eq!(super::utc(946_684_800), "2000-01-01T00:00:00");
    assert_eq!(super::utc(951_782_399), "2000-02-28T23:59:59");
    assert_eq!(super::utc(951_868_800), "2000-03-01T00:00:00");
    assert_eq!(super::utc(1_700_000_000), "2023-11-14T22:13:20");
    assert_eq!(super::utc(u32::MAX), "2106-02-07T06:28:15");
}
