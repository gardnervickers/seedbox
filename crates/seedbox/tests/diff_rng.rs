// The offline reader must never consume a runtime random draw.
use seedbox as sb;
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/rng.rs"]
mod rng;
include!("support/diff_fixture.rs");

#[test]
fn offline_diff_preserves_rng_domains_and_exact_draw_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut disk = rng::Rng::new(1009, "disk-loss/server/0");
    let mut timer = rng::Rng::new(1009, "timer-jitter/server");
    let expected_disk = [
        5659177532502815958,
        18152646734479923690,
        15197821826857178923,
        7425220440706095210,
    ];
    let expected_timer = [
        7937987393058502284,
        8223677711474284274,
        9736032864105262339,
        16340458967412450860,
    ];
    for (index, (disk_draw, timer_draw)) in
        expected_disk.into_iter().zip(expected_timer).enumerate()
    {
        let raw = current_report(index >= 2);
        write_fixture(dir.path(), &mut fixture(), Some(&raw), None);
        assert_eq!(disk.next(), disk_draw);
        assert_eq!(
            sb::diff::diff(dir.path(), dir.path()).unwrap().status,
            sb::diff::Status::Match
        );
        assert_eq!(timer.next(), timer_draw);
    }
}

#[test]
fn ram_page_comparison_preserves_runtime_rng_streams_and_draw_order() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = ram_pair();
    for (name, raw, pfns) in [("a", &a, vec![]), ("b", &b, vec![5, 9])] {
        let path = dir.path().join(name);
        let mut record = fixture();
        write_fixture(&path, &mut record, Some(raw), None);
        write_pages(&path, &mut record, &pfns);
        write_fixture(&path, &mut record, Some(raw), None);
    }
    for domain in [
        "disk-loss/server/0",
        "timer-jitter/server",
        "schedule",
        "network",
    ] {
        let mut stream = rng::Rng::new(1009, domain);
        let mut control = stream.clone();
        for _ in 0..4 {
            assert_eq!(stream.next(), control.next());
            let report = sb::diff::diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
            assert_eq!(
                report.machines[0].ram_pages.as_ref().unwrap().first_pfns,
                [5, 9]
            );
            assert_eq!(stream.next(), control.next());
        }
    }
}
