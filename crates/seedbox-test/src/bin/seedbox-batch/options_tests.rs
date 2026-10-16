//! Portable assertions formerly mixed with mocked process execution in Python.
//! Native launch order, sessions, statuses and reaping stay in Linux subprocesses.
use super::options::Options;
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

fn parse(args: &[&str], settings: &[(&str, &str)]) -> super::options::Result<Options> {
    Options::parse(args.iter().map(|arg| OsString::from(*arg)), |name| {
        settings
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).into())
    })
}

#[test]
fn invalid_options_are_portably_rejected_before_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let untouched = directory.path().join("must-not-exist");
    let mut options = Options {
        root: untouched.clone(),
        world: "world".into(),
        prefix: "control".into(),
        seeds: vec!["1".into()],
        extra: vec![],
        timeout: 1.0,
        vmm: "unused".into(),
    };
    for timeout in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        options.timeout = timeout;
        assert_eq!(
            options.validate().unwrap_err().to_string(),
            "expected nonempty seeds and positive timeout"
        );
        assert!(!untouched.exists());
    }
    options.timeout = 1.0;
    options.seeds.clear();
    assert!(options.validate().is_err());
    assert!(!untouched.exists());
}

#[test]
fn all_seed_commands_and_output_names_are_portably_planned_without_binding_or_locks() {
    let directory = tempfile::tempdir().unwrap();
    let options = Options {
        root: directory.path().into(),
        world: "world.json".into(),
        prefix: "batch".into(),
        seeds: vec!["7".into(), "8".into(), "9".into()],
        extra: vec!["--digest".into()],
        timeout: 1.0,
        vmm: "vmm".into(),
    };
    options.validate().unwrap();
    let plans = options.plan();
    assert_eq!(plans.len(), 3);
    assert_eq!(options.vmm, "vmm", "no taskset or CPU-binding launcher");
    for (index, seed) in [7, 8, 9].iter().enumerate() {
        let expected = directory
            .path()
            .join(format!("runs/batch-s{seed}-r{index}"));
        assert_eq!(plans[index].output, expected);
        assert_eq!(
            plans[index].args,
            vec![
                OsString::from("--config"),
                "world.json".into(),
                "--seed".into(),
                seed.to_string().into(),
                "--out".into(),
                expected.into_os_string(),
                "--digest".into(),
            ]
        );
    }
    assert!(
        !directory.path().join("runs").exists(),
        "planning performs no IO"
    );
    assert!(!fs::read_dir(directory.path()).unwrap().any(|entry| entry
        .unwrap()
        .path()
        .extension()
        .is_some_and(|s| s == "lock")));
}

#[test]
fn parser_defaults_apply_to_missing_and_empty_optional_settings() {
    for settings in [
        vec![("SEEDBOX_ROOT", "ROOT")],
        vec![
            ("SEEDBOX_ROOT", "ROOT"),
            ("VMM", ""),
            ("SEEDBOX_TIMEOUT", ""),
        ],
    ] {
        let options = parse(&["world.json", "batch", "7 8 9", "--digest"], &settings).unwrap();
        assert_eq!(options.root, Path::new("ROOT"));
        assert_eq!(options.world, "world.json");
        assert_eq!(options.prefix, "batch");
        assert_eq!(options.seeds, ["7", "8", "9"]);
        assert_eq!(options.extra, ["--digest"]);
        assert_eq!(options.timeout, 3600.0);
        assert_eq!(
            options.vmm,
            "./crates/seedbox-vmm/target/release/seedbox-vmm"
        );
    }
}

#[test]
fn parser_preserves_explicit_settings_and_argument_boundaries() {
    let options = parse(
        &[
            "world with spaces.json",
            "prefix",
            " 7 +008 -9\n",
            "--digest",
            "one argument with spaces",
            "",
        ],
        &[
            ("SEEDBOX_ROOT", "explicit root"),
            ("VMM", "custom worker"),
            ("SEEDBOX_TIMEOUT", " .08 "),
        ],
    )
    .unwrap();
    assert_eq!(options.root, PathBuf::from("explicit root"));
    assert_eq!(options.world, "world with spaces.json");
    assert_eq!(options.prefix, "prefix");
    assert_eq!(options.seeds, ["7", "8", "-9"]);
    assert_eq!(options.extra, ["--digest", "one argument with spaces", ""]);
    assert_eq!(options.vmm, "custom worker");
    assert_eq!(options.timeout, 0.08);
    let plans = options.plan();
    assert_eq!(
        plans[1].output,
        Path::new("explicit root").join("runs/prefix-s8-r1")
    );
    assert_eq!(&plans[1].args[6..], options.extra);
}

#[test]
fn parser_errors_cover_missing_arguments_root_bad_seeds_and_timeouts() {
    for args in [vec![], vec!["world"], vec!["world", "prefix"]] {
        assert!(parse(&args, &[("SEEDBOX_ROOT", "ROOT")]).is_err());
    }
    assert!(parse(&["world", "prefix", "1"], &[])
        .err()
        .unwrap()
        .to_string()
        .contains("SEEDBOX_ROOT"));
    for seeds in ["", " \n", "not-a-seed", "1 broken 2"] {
        assert!(
            parse(&["world", "prefix", seeds], &[("SEEDBOX_ROOT", "ROOT")]).is_err(),
            "{seeds:?}"
        );
    }
    for timeout in ["0", "-1", "NaN", "inf", "not-a-timeout"] {
        assert!(
            parse(
                &["world", "prefix", "1"],
                &[("SEEDBOX_ROOT", "ROOT"), ("SEEDBOX_TIMEOUT", timeout)]
            )
            .is_err(),
            "{timeout}"
        );
    }
}

#[test]
fn empty_root_setting_preserves_pathlib_current_directory_semantics() {
    let options = parse(&["world", "prefix", "1"], &[("SEEDBOX_ROOT", "")]).unwrap();
    assert_eq!(options.root, Path::new("."));
    assert_eq!(
        options.plan()[0].output,
        Path::new(".").join("runs/prefix-s1-r0")
    );
}
