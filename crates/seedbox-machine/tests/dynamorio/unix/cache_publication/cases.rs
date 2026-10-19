//! Actual A64 DC/DSB/IC publication. Cross-compilation supplies no runtime evidence.
use super::timer_state_cases::{root, run};

#[test]
fn a64_public_cache_publication_x0_x28_x30() {
    for operand in ["0", "28", "30"] {
        let expected = format!("phase=1 value=1 expected=1 operand=x{operand}\nphase=2 value=2 expected=2 operand=x{operand}\n");
        let (native, _) = run(
            &root(false),
            "unix-cache_publication",
            &[operand],
            false,
            false,
            &[],
            None,
        );
        assert_eq!(native, expected);
        for debug in [false, true] {
            for options in [&[][..], &["-no_hw_cache_consistency"][..]] {
                for client in [None, Some("libunix-cache-publication.so")] {
                    let (out, err) = run(
                        &root(debug),
                        "unix-cache_publication",
                        &[operand],
                        true,
                        debug,
                        options,
                        client,
                    );
                    assert_eq!(out, expected);
                    if client.is_some() {
                        assert!(
                            err.lines().any(|line| line
                                .strip_prefix("PUBLIC_BB blocks=")
                                .and_then(|n| n.parse::<u64>().ok())
                                .is_some_and(|n| n > 0)),
                            "public BB receipt: {err}"
                        );
                    }
                }
            }
        }
    }
}
