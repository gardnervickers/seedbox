mod source_cache;

use super::{policy::*, process::*, run::*};
use serde_json::json;
use std::{collections::VecDeque, fs, path::PathBuf, time::Duration};
#[derive(Default)]
struct Spy {
    jobs: Vec<Job>,
    replies: VecDeque<Outcome>,
}
impl Commands for Spy {
    fn execute(&mut self, job: &Job) -> std::io::Result<Outcome> {
        self.jobs.push(job.clone());
        Ok(self.replies.pop_front().unwrap_or_default())
    }
}
#[test]
fn check_runs_disable_incremental_without_mutating_caller_settings() {
    for setting in [None, Some("0"), Some("1")] {
        let mut environment = env(&[("CARGO_TARGET_DIR", "/agent-cache")]);
        if let Some(value) = setting {
            environment.insert("CARGO_INCREMENTAL".into(), value.into());
        }
        let c = Checks::new(Spy::default(), "/fixture".into(), environment.clone());
        let job = c.job("build", words(&["cargo", "build", "--locked"]));
        assert_eq!(job.environment["CARGO_INCREMENTAL"], "0");
        assert_eq!(job.environment["CARGO_TARGET_DIR"], "/agent-cache");
        assert_eq!(
            environment.get("CARGO_INCREMENTAL").map(String::as_str),
            setting
        );
    }
}
fn strings(values: &[&str]) -> Vec<String> {
    words(values)
}
fn env(values: &[(&str, &str)]) -> Environment {
    values
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}
fn fixture() -> Temporary {
    Temporary::in_directory(&std::env::temp_dir(), "seedbox-check-unit-").unwrap()
}
fn host() -> Host {
    let mut h = Host::local();
    h.system = "Linux".into();
    h.machine = "x86_64".into();
    h.kvm = Some(String::new());
    h.tools.extend(strings(&["nix", "podman"]));
    h.root_prefix = Some(vec![]);
    h
}
fn checks(root: &std::path::Path) -> Checks<Spy> {
    Checks::new(Spy::default(), root.into(), Environment::new())
}
fn suites(names: &[&str]) -> Vec<Suite> {
    expand(&strings(names)).unwrap()
}
// A cross-process readiness file is a publication event, not merely an inode.
// Keep incomplete contents private until same-directory rename publishes them.
fn publish_ready_file(
    path: &std::path::Path,
    contents: &[u8],
    pending_complete: impl FnOnce(&std::path::Path),
) {
    let pending = path.with_extension("pending");
    fs::write(&pending, contents).unwrap();
    pending_complete(&pending);
    fs::rename(pending, path).unwrap();
}
#[test]
fn readiness_is_published_only_after_complete_contents_are_written() {
    let t = fixture();
    let ready = t.0.join("ready");
    publish_ready_file(&ready, b"12345", |pending| {
        assert!(
            !ready.exists(),
            "a reader must not see the unpublished file"
        );
        assert_eq!(fs::read(pending).unwrap(), b"12345");
    });
    assert_eq!(
        fs::read_to_string(&ready).unwrap().parse::<u32>().unwrap(),
        12345
    );
    assert!(!ready.with_extension("pending").exists());
}
fn artifact(path: &std::path::Path, name: &str, test: bool) -> String {
    json!({"reason":"compiler-artifact","executable":path,"target":{"name":name},"profile":{"test":test}}).to_string()
}

#[test]
fn test_machine_suites_are_explicit_and_require_inputs() {
    let temp = fixture();
    let root = &temp.0;
    let h = host();
    assert_eq!(
        suites(&["machine-tcg"]),
        [Suite::Portable, Suite::MachineTcg]
    );
    assert!(!suites(&["linux"]).contains(&Suite::MachineTcg));
    let missing = requirements(Suite::MachineTcg, &h, &Environment::new(), root);
    for key in machine_inputs(Suite::MachineTcg) {
        assert!(missing.iter().any(|r| r.contains(&key)), "{key}");
    }
    assert!(!missing.iter().any(|r| r.contains("/dev/kvm")));
    for key in machine_inputs(Suite::MachinePv) {
        assert!(
            requirements(Suite::MachinePv, &h, &Environment::new(), root)
                .iter()
                .any(|r| r.contains(&key))
        );
    }
    let mut arm = h.clone();
    arm.machine = "aarch64".into();
    assert!(
        requirements(Suite::MachineTcg, &arm, &Environment::new(), root)
            .contains(&"requires Linux x86-64".into())
    );
    let mut denied = h.clone();
    denied.kvm = Some("denied".into());
    assert!(
        requirements(Suite::MachinePv, &denied, &Environment::new(), root)
            .iter()
            .any(|r| r.contains("/dev/kvm"))
    );
    let executable = root.join("input");
    install_executable(&executable, "fixture");
    let mut inputs = Environment::new();
    for key in machine_inputs(Suite::MachineTcg) {
        inputs.insert(key, executable.to_string_lossy().into());
    }
    inputs.insert("SEEDBOX_TCG_ASSETS".into(), root.to_string_lossy().into());
    // The public-service tests need the normal runtime package as well.
    assert_eq!(
        requirements(Suite::MachineTcg, &denied, &inputs, root),
        [format!(
            "{SERVICE_RUNTIME} is missing; run cargo xtask build-assets"
        )]
    );
    fs::create_dir_all(root.join("build/assets")).unwrap();
    fs::write(root.join(SERVICE_RUNTIME), "runtime").unwrap();
    assert!(requirements(Suite::MachineTcg, &denied, &inputs, root).is_empty());
    // Both suites need the explicit backend-neutral cpu-loop input; TCG
    // still needs no KVM to use it.
    assert!(machine_inputs(Suite::MachinePv).contains(&CPU_LOOP_FIXTURE.to_string()));
    let value = inputs.remove(CPU_LOOP_FIXTURE).unwrap();
    assert_eq!(
        requirements(Suite::MachineTcg, &denied, &inputs, root),
        [format!("set {CPU_LOOP_FIXTURE} to an existing file")]
    );
    inputs.insert(CPU_LOOP_FIXTURE.into(), value);
    for key in [
        "SEEDBOX_TCG_CLOCK_APPLICATION",
        "SEEDBOX_TCG_NETWORK_APPLICATION",
        "SEEDBOX_TCG_TERMINAL_APPLICATION",
    ] {
        let value = inputs.remove(key).unwrap();
        assert_eq!(
            requirements(Suite::MachineTcg, &denied, &inputs, root),
            [format!("set {key} to an existing file")]
        );
        inputs.insert(key.into(), root.to_string_lossy().into());
        assert_eq!(
            requirements(Suite::MachineTcg, &denied, &inputs, root),
            [format!("set {key} to an existing file")]
        );
        inputs.insert(key.into(), value);
    }
}
#[test]
fn test_raw_machine_requirements_do_not_acquire_etcd_oci_or_cli_inputs() {
    let temp = fixture();
    let runtime = temp.0.join(SERVICE_RUNTIME);
    fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    fs::write(runtime, "runtime fixture").unwrap();
    let binary = temp.0.join("native-fixture");
    install_executable(&binary, "fixture");
    let mut no_engine = host();
    no_engine.tools.remove("podman");
    assert!(!no_engine.engine(&Environment::new()));
    for suite in [Suite::MachinePv, Suite::MachineTcg] {
        let inputs: Environment = machine_inputs(suite)
            .into_iter()
            .map(|key| {
                assert_ne!(key, "SEEDBOX_ETCD_CLI");
                let path = if key.ends_with("_ASSETS") {
                    &temp.0
                } else {
                    &binary
                };
                (key, path.to_string_lossy().into_owned())
            })
            .collect();
        assert!(requirements(suite, &no_engine, &inputs, &temp.0).is_empty());
    }
}

#[test]
fn test_etcd_smoke_target_is_discovered_only_by_oci_linux_apps_feature() {
    let metadata = json!({"packages": [{"name":"renamed-cli",
        "metadata":{"seedbox-check":{"suites":{"linux-apps":["etcd-prep-tests"]}}},
        "features":{"etcd-prep-tests":[]}, "targets":[
        {"name":"etcd_service_native", "kind":["test"], "required-features":["etcd-prep-tests"]}
    ]}]});
    for (names, expected) in [
        (vec!["linux-apps"], vec!["etcd_service_native"]),
        (vec!["machine-pv-x86", "machine-tcg"], vec![]),
        (vec!["mac-apps"], vec![]),
    ] {
        let discovered = suite_test_targets(&metadata, &suites(&names)).unwrap();
        let targets: Vec<_> = discovered
            .into_iter()
            .flat_map(|tests| tests.targets)
            .collect();
        assert_eq!(targets, strings(&expected));
    }
}

#[test]
fn test_missing_tcg_list_exits_two_without_probing_kvm_or_building() {
    let temp = fixture();
    let mut c = checks(&temp.0);
    let o = options(strings(&["machine-tcg", "--list"])).unwrap();
    let observed = observe(&mut c, &o.suites).unwrap();
    assert!(c.commands.jobs.is_empty());
    assert_eq!(coordinate(&mut c, &observed, &o).unwrap(), 2);
    assert!(c.commands.jobs.is_empty());
}
#[test]
fn dynamorio_suite_is_unprivileged_native_and_requires_prepared_inputs() {
    let temp = fixture();
    let mut c = checks(&temp.0);
    let options = options(strings(&["dynamorio", "--list"])).unwrap();
    assert_eq!(options.suites, [Suite::Portable, Suite::Dynamorio]);
    let observed = observe(&mut c, &options.suites).unwrap();
    assert!(c.commands.jobs.is_empty(), "must not probe KVM or sudo");
    assert_eq!(coordinate(&mut c, &observed, &options).unwrap(), 2);
    assert!(
        c.commands.jobs.is_empty(),
        "missing fixtures fail before builds"
    );
    let mut h = host();
    h.kvm = None;
    h.tools.clear();
    h.root_prefix = None;
    let release_only = env(&[("SEEDBOX_DYNAMORIO_FIXTURES", temp.0.to_str().unwrap())]);
    fs::create_dir_all(temp.0.join("bin")).unwrap();
    fs::create_dir_all(temp.0.join("runtime/bin64")).unwrap();
    fs::create_dir_all(temp.0.join("lib")).unwrap();
    install_executable(&temp.0.join("runtime/bin64/drrun"), "fixture");
    install_executable(&temp.0.join("bin/application"), "fixture");
    fs::write(temp.0.join("lib/libpreserve.so"), "fixture").unwrap();
    assert!(requirements(Suite::Dynamorio, &h, &release_only, &temp.0)
        .iter()
        .any(|reason| reason.contains("SEEDBOX_DYNAMORIO_DEBUG_FIXTURES")));
    fs::create_dir_all(temp.0.join("runtime/lib64/debug")).unwrap();
    fs::write(
        temp.0.join("runtime/lib64/debug/libdynamorio.so"),
        "fixture",
    )
    .unwrap();
    install_executable(&temp.0.join("bin/unix-supervisor"), "fixture");
    install_executable(&temp.0.join("bin/unix-signal-models-itimer"), "fixture");
    assert!(
        requirements(Suite::Dynamorio, &h, &release_only, &temp.0)
            .iter()
            .any(|reason| reason.contains("SEEDBOX_DYNAMORIO_DEBUG_FIXTURES")),
        "release input never implicitly supplies debug fixtures"
    );
    let env = env(&[
        ("SEEDBOX_DYNAMORIO_FIXTURES", temp.0.to_str().unwrap()),
        ("SEEDBOX_DYNAMORIO_DEBUG_FIXTURES", temp.0.to_str().unwrap()),
    ]);
    for arch in ["x86_64", "aarch64"] {
        h.machine = arch.into();
        assert!(requirements(Suite::Dynamorio, &h, &env, &temp.0).is_empty());
    }
    h.system = "Darwin".into();
    assert!(requirements(Suite::Dynamorio, &h, &env, &temp.0)
        .contains(&"requires Linux x86-64 or ARM64".into()));
}

#[test]
fn test_default_is_portable_only() {
    assert_eq!(options(vec![]).unwrap().suites, [Suite::Portable]);
    assert_eq!(suites(&["portable"]), [Suite::Portable]);
}
#[test]
fn test_groups_and_implied_suites_expand_in_documented_order() {
    for (names, expected) in [
        (
            &["linux"][..],
            vec![Suite::Portable, Suite::LinuxOci, Suite::LinuxApps],
        ),
        (&["mac"][..], vec![Suite::Portable, Suite::MacApps]),
        (&["mac-apps"][..], vec![Suite::Portable, Suite::MacApps]),
    ] {
        assert_eq!(suites(names), expected);
    }
    assert_eq!(
        suites(&["linux-apps", "portable", "linux-oci", "linux-apps"]),
        suites(&["linux-oci", "linux-apps"])
    );
    for retired in ["linux-kvm", "linux-guests", "mac-hvf", "mac-guests"] {
        assert!(expand(&strings(&[retired])).is_err(), "{retired}");
    }
}
#[test]
fn test_workspace_suite_targets_preserve_portable_linux_mac_and_pause_selection() {
    use seedbox_exec::Guarded;
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .args([
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--locked",
            "--offline",
        ])
        .guarded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata = serde_json::from_slice(&output.stdout).unwrap();
    for names in [vec!["portable"], vec!["machine-pv-x86", "machine-tcg"]] {
        assert!(suite_test_targets(&metadata, &suites(&names))
            .unwrap()
            .is_empty());
    }
    for (names, expected) in [
        (
            vec!["dynamorio"],
            vec![SuiteTests {
                package: "seedbox-machine".into(),
                features: strings(&["dynamorio-tests"]),
                targets: strings(&["dynamorio"]),
            }],
        ),
        (
            vec!["dynamorio-isolated"],
            vec![SuiteTests {
                package: "seedbox-machine".into(),
                features: strings(&["dynamorio-isolated-tests"]),
                targets: strings(&["dynamorio_isolated"]),
            }],
        ),
        (
            vec!["linux"],
            vec![
                SuiteTests {
                    package: "seedbox".into(),
                    features: strings(&["application-tests", "etcd-prep-tests"]),
                    targets: strings(&["applications", "etcd_service_native"]),
                },
                SuiteTests {
                    package: "seedbox-application".into(),
                    features: strings(&["oci-tests"]),
                    targets: strings(&["oci_engine"]),
                },
            ],
        ),
        (
            vec!["mac"],
            vec![SuiteTests {
                package: "seedbox".into(),
                features: strings(&["application-tests"]),
                targets: strings(&["applications"]),
            }],
        ),
        (
            vec!["linux-pause"],
            vec![SuiteTests {
                package: "seedbox".into(),
                features: strings(&["retained-run-tests"]),
                targets: strings(&["host_pause"]),
            }],
        ),
    ] {
        assert_eq!(
            suite_test_targets(&metadata, &suites(&names)).unwrap(),
            expected
        );
    }
}

#[test]
fn test_suite_discovery_follows_new_packages_features_and_required_features() {
    let metadata = json!({"packages":[
        {"name":"unregistered", "targets":[{"name":"not-selected", "kind":["test"], "required-features":["native"]}]},
        {"name":"new-package", "metadata":{"seedbox-check":{"suites":{
            "linux-oci":["native"], "linux-apps":["native", "extra"], "mac-apps":["extra"]
        }}}, "features":{"native":[], "extra":[]}, "targets":[
            {"name":"ordinary", "kind":["test"]},
            {"name":"native-test", "kind":["test"], "required-features":["native"]},
            {"name":"combined", "kind":["test"], "required-features":["native", "extra"]},
            {"name":"helper", "kind":["bin"], "required-features":["native"]}
        ]}
    ]});
    assert_eq!(
        suite_test_targets(&metadata, &suites(&["linux-oci"])).unwrap(),
        vec![SuiteTests {
            package: "new-package".into(),
            features: strings(&["native"]),
            targets: strings(&["native-test"])
        }]
    );
    assert_eq!(
        suite_test_targets(&metadata, &suites(&["linux"])).unwrap(),
        vec![SuiteTests {
            package: "new-package".into(),
            features: strings(&["extra", "native"]),
            targets: strings(&["combined", "native-test"])
        }]
    );
}

#[test]
fn test_invalid_suite_declarations_fail_instead_of_silently_skipping_tests() {
    assert!(suite_test_targets(&json!({}), &suites(&["linux"])).is_err());
    for (declarations, error) in [
        (json!([]), "must map suites"),
        (
            json!({"typo":["native"]}),
            "unknown seedbox-check suite typo",
        ),
        (json!({"linux-oci":"native"}), "must contain a feature list"),
        (json!({"linux-oci":[false]}), "feature must be a string"),
        (json!({"linux-oci":["typo"]}), "unknown feature typo"),
        (
            json!({"linux-oci":["native"]}),
            "no integration targets require",
        ),
    ] {
        let metadata = json!({"packages":[{"name":"new-package", "metadata":{"seedbox-check":{"suites":declarations}}, "features":{"native":[]}, "targets":[]}]});
        assert!(
            suite_test_targets(&metadata, &suites(&["linux"]))
                .unwrap_err()
                .contains(error),
            "{error}"
        );
    }
}
#[test]
fn test_hardware_targets_follow_cargo_declarations() {
    let metadata = json!({"packages":[{"name":"seedbox","targets":[{"name":"portable","kind":["test"]},{"name":"image_engine","kind":["test"],"required-features":["oci-tests"]},{"name":"combined","kind":["test"],"required-features":["oci-tests","application-tests"]},{"name":"library","kind":["lib"],"required-features":["oci-tests"]}]}]});
    assert_eq!(
        feature_test_targets(&metadata, "seedbox", &strings(&["oci-tests"])).unwrap(),
        strings(&["image_engine"])
    );
    assert_eq!(
        feature_test_targets(
            &metadata,
            "seedbox",
            &strings(&["application-tests", "oci-tests"])
        )
        .unwrap(),
        strings(&["combined", "image_engine"])
    );
    assert!(feature_test_targets(&metadata, "seedbox", &[])
        .unwrap()
        .is_empty());
}
#[test]
fn test_apps_suite_reports_missing_root() {
    let t = fixture();
    let mut h = host();
    h.root_prefix = None;
    assert!(
        requirements(Suite::LinuxApps, &h, &Environment::new(), &t.0)
            .contains(&"requires root or passwordless sudo".into())
    );
}
#[test]
fn test_oci_suite_needs_an_engine_but_no_kvm_or_root() {
    let t = fixture();
    let mut h = host();
    h.kvm = Some("no such device".into());
    h.root_prefix = None;
    assert!(requirements(Suite::LinuxOci, &h, &Environment::new(), &t.0).is_empty());
    h.tools.clear();
    assert!(requirements(
        Suite::LinuxOci,
        &h,
        &env(&[("DOCKER_HOST", "unix:///fixture")]),
        &t.0
    )
    .is_empty());
    let m = requirements(Suite::LinuxOci, &h, &Environment::new(), &t.0);
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("no Docker-compatible engine"));
}
#[test]
fn test_an_engine_socket_counts_without_binaries() {
    let t = fixture();
    let path = t.0.join("socket");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let mut h = host();
    h.tools.clear();
    h.sockets = vec![(path, true)];
    assert!(h.engine(&Environment::new()));
    let paths = socket_candidates(&env(&[
        ("HOME", "/home/fixture"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
    ]));
    assert!(paths.contains(&PathBuf::from("/home/fixture/.docker/run/docker.sock")));
    assert!(paths.contains(&PathBuf::from("/run/user/1000/podman/podman.sock")));
}
#[test]
fn test_a_root_only_engine_socket_needs_root_for_oci() {
    let t = fixture();
    let p = t.0.join("podman.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&p).unwrap();
    let e = env(&[("DOCKER_HOST", &format!("unix://{}", p.display()))]);
    let mut h = host();
    h.sockets = vec![(p.clone(), true)];
    assert!(!h.engine_needs_root(&e));
    h.sockets = vec![(p, false)];
    assert!(h.engine_needs_root(&e));
    assert!(requirements(Suite::LinuxOci, &h, &e, &t.0).is_empty());
    h.root_prefix = None;
    let m = requirements(Suite::LinuxOci, &h, &e, &t.0);
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("not writable by this user"));
    assert!(!h.engine_needs_root(&env(&[("DOCKER_HOST", "tcp://127.0.0.1:2375")])));
}
#[test]
fn test_x86_only_suites_refuse_arm_linux() {
    let t = fixture();
    let mut h = host();
    h.machine = "aarch64".into();
    assert!(requirements(Suite::LinuxOci, &h, &Environment::new(), &t.0)
        .contains(&"requires Linux x86-64".into()));
}
#[test]
fn test_platform_mismatch_is_unavailable() {
    let t = fixture();
    let h = host();
    assert_eq!(
        requirements(Suite::MacApps, &h, &Environment::new(), &t.0),
        ["requires macOS on Apple Silicon"]
    );
    let mut mac = h;
    mac.system = "Darwin".into();
    mac.machine = "arm64".into();
    assert_eq!(
        requirements(Suite::LinuxApps, &mac, &Environment::new(), &t.0),
        ["requires Linux"]
    );
    assert!(requirements(Suite::Portable, &mac, &Environment::new(), &t.0).is_empty());
}
#[test]
fn test_mac_apps_requirements() {
    let t = fixture();
    let mut h = host();
    h.system = "Darwin".into();
    h.machine = "arm64".into();
    h.mac_major = 15;
    h.hvf = true;
    h.tools.insert("codesign".into());
    assert!(requirements(Suite::MacApps, &h, &Environment::new(), &t.0).is_empty());
    h.mac_major = 14;
    assert_eq!(
        requirements(Suite::MacApps, &h, &Environment::new(), &t.0),
        ["requires macOS 15+"]
    );
}
#[test]
fn test_build_admission_needs_tools_not_cached_assets() {
    let mut h = host();
    let e = env(&[("SEEDBOX_ASSET_DIR", "/stale")]);
    assert!(build_requirements(&h, &e).is_empty());
    h.system = "Darwin".into();
    assert!(build_requirements(&h, &e).is_empty());
    assert!(build_requirements(&h, &env(&[("SEEDBOX_ASSET_BUILDER", "")])).is_empty());
    h.tools.clear();
    assert!(build_requirements(&h, &env(&[("SEEDBOX_ASSET_BUILDER", "")]))[0].contains("CLI"));
    assert!(build_requirements(&h, &e)[0].contains("CLI"));
    h.system = "Linux".into();
    assert!(build_requirements(&h, &e)[0].contains("nix"));
}
#[test]
fn test_complete_build_replaces_ambient_runtime_overrides() {
    let t = fixture();
    let mut e = Environment::new();
    for key in ["SEEDBOX_ASSET_DIR", "SEEDBOX_MKE2FS"] {
        e.insert(key.into(), "/stale".into());
    }
    assert!(select_build(&mut e, &t.0).is_err());
    for name in ["seedbox", "driver-runtime.tar.gz"] {
        fs::write(t.0.join(name), "").unwrap();
    }
    select_build(&mut e, &t.0).unwrap();
    assert_eq!(e.len(), 1);
    assert_eq!(e["SEEDBOX_TEST_CLI"], t.0.join("seedbox").to_str().unwrap());
}
#[test]
fn test_privileged_execution_forwards_asset_directory_and_engine() {
    let e = env(&[
        ("SEEDBOX_MKE2FS", "/formatter"),
        ("SEEDBOX_ASSET_DIR", "/slot"),
        ("DOCKER_HOST", "unix:///engine"),
        ("GH_TOKEN", "fixture"),
    ]);
    let command = privileged(
        Some(&strings(&["sudo", "-n", "env"])),
        strings(&["/test"]),
        &e,
    );
    assert_eq!(
        command,
        strings(&[
            "sudo",
            "-n",
            "env",
            "DOCKER_HOST=unix:///engine",
            "SEEDBOX_ASSET_DIR=/slot",
            "SEEDBOX_MKE2FS=/formatter",
            "/test"
        ])
    );
}
#[test]
fn test_unmet_selected_suite_fails_before_any_build() {
    let t = fixture();
    let mut h = host();
    h.system = "Darwin".into();
    h.machine = "arm64".into();
    for args in [strings(&["--list", "mac-apps"]), strings(&["mac-apps"])] {
        let mut c = checks(&t.0);
        assert_eq!(coordinate(&mut c, &h, &options(args).unwrap()).unwrap(), 2);
        assert!(c.commands.jobs.is_empty());
    }
}
#[test]
fn test_unknown_suite_is_a_usage_error() {
    assert!(options(strings(&["linux-everything"]))
        .unwrap_err()
        .contains("unknown suite"));
}
#[test]
fn test_discovers_all_test_artifacts_and_ignores_build_outputs() {
    let a = artifact(std::path::Path::new("/new/test-a"), "new-a", true);
    let output = [
        a.clone(),
        artifact(std::path::Path::new("/new/test-b"), "new-b", true),
        artifact(std::path::Path::new("/cli"), "cli", false),
        a,
    ]
    .join("\n");
    assert_eq!(
        test_artifacts(&output).unwrap(),
        vec![
            ("/new/test-a".into(), "new-a".into()),
            ("/new/test-b".into(), "new-b".into())
        ]
    );
}
#[test]
fn native_tests_own_short_scratch_through_privileged_and_direct_execution() {
    struct ScratchSpy {
        scratch: Vec<PathBuf>,
        privileged: bool,
    }
    impl Commands for ScratchSpy {
        fn execute(&mut self, job: &Job) -> std::io::Result<Outcome> {
            if job.capture {
                assert_eq!(job.environment["TMPDIR"], "/caller/long/build/scratch");
                return Ok(Outcome {
                    code: 0,
                    stdout: [
                        artifact(std::path::Path::new("/fixture/first"), "first", true),
                        artifact(std::path::Path::new("/fixture/second"), "second", true),
                    ]
                    .join("\n"),
                });
            }
            let scratch = PathBuf::from(&job.environment["TMPDIR"]);
            assert!(scratch.is_dir());
            assert_eq!(scratch.parent(), Some(std::path::Path::new("/tmp")));
            assert!(scratch.as_os_str().len() < 32);
            assert!(!self.scratch.contains(&scratch));
            if self.privileged {
                assert_eq!(&job.args[..3], &["sudo", "-n", "env"]);
                assert!(job.args.contains(&format!("TMPDIR={}", scratch.display())));
            } else {
                assert!(job.args[0].starts_with("/fixture/"));
            }
            fs::write(scratch.join("owned-file"), b"fixture")?;
            self.scratch.push(scratch);
            Ok(Outcome::default())
        }
    }
    for prefix in [
        None,
        Some(Vec::new()),
        Some(strings(&["sudo", "-n", "env"])),
    ] {
        let t = fixture();
        let mut c = Checks::new(
            ScratchSpy {
                scratch: Vec::new(),
                privileged: prefix.as_ref().is_some_and(|p| !p.is_empty()),
            },
            t.0.clone(),
            env(&[("TMPDIR", "/caller/long/build/scratch")]),
        );
        cargo_tests(
            &mut c,
            "seedbox",
            strings(&["cargo", "test", "-p", "seedbox"]),
            &strings(&["application-tests"]),
            prefix.as_deref(),
            false,
        )
        .unwrap();
        assert_eq!(c.commands.scratch.len(), 2);
        assert!(c.commands.scratch.iter().all(|p| !p.exists()));
        assert_eq!(c.environment["TMPDIR"], "/caller/long/build/scratch");
    }
}

#[test]
fn test_machine_suites_default_to_two_test_threads_in_their_child_only() {
    let e = env(&[("PATH", "/bin")]);
    let runs = machine_suite_runs(
        &suites(&["machine-tcg", "machine-pv-x86"]),
        &e,
        std::path::Path::new("/fixture"),
    );
    assert!(!runs.is_empty());
    for job in runs {
        assert_eq!(
            job.environment,
            env(&[("PATH", "/bin"), ("RUST_TEST_THREADS", "2")])
        );
    }
    assert_eq!(e, env(&[("PATH", "/bin")]));
}
#[test]
fn test_machine_suites_keep_an_explicit_test_thread_count() {
    for value in ["1", "8"] {
        let e = env(&[("RUST_TEST_THREADS", value)]);
        for job in machine_suite_runs(
            &suites(&["machine-tcg", "machine-pv-x86"]),
            &e,
            std::path::Path::new("/fixture"),
        ) {
            assert_eq!(job.environment["RUST_TEST_THREADS"], value);
        }
    }
}
#[test]
fn test_machine_suites_do_not_force_serial_execution() {
    let runs = machine_suite_runs(
        &suites(&["machine-tcg", "machine-pv-x86"]),
        &Environment::new(),
        std::path::Path::new("/fixture"),
    );
    let expected = [
        "cargo test -p seedbox-machine-tcg --locked --lib -- --ignored",
        "cargo test -p seedbox-machine-tcg --locked --test native_machine -- --ignored",
        "cargo test -p seedbox-driver --locked --test native_driver -- --ignored tcg",
        "cargo test -p seedbox-driver --locked --test tcg_shutdown -- --ignored",
        "cargo test -p seedbox-driver --locked --test driver_guest_services -- --ignored tcg",
        "cargo test -p seedbox-machine-tcg --locked --test guest_services -- --ignored tcg",
        "cargo test -p seedbox-machine-tcg --locked --test prepared_application -- --ignored tcg",
        "cargo test -p seedbox-machine-tcg --locked --test simulation_control -- --ignored tcg",
        "cargo test -p seedbox-server --locked --features tcg-tests --test native_tcg_service --",
        "cargo test -p seedbox-server --locked --features tcg-tests --test native_controller_service --",
        "cargo test -p seedbox-machine-pv --locked --lib -- --ignored",
        "cargo test -p seedbox-machine-pv --locked --test native_machine -- --ignored",
        "cargo test -p seedbox-driver --locked --test native_driver -- --ignored pv",
        "cargo test -p seedbox-machine-pv --locked --test pv_progress -- --ignored",
        "cargo test -p seedbox-driver --locked --test native_pv_driver -- --ignored",
        "cargo test -p seedbox-driver --locked --test pv_instructions -- --ignored",
        "cargo test -p seedbox-driver --locked --test driver_guest_services -- --ignored pv",
        "cargo test -p seedbox-machine-pv --locked --test guest_services -- --ignored pv",
        "cargo test -p seedbox-machine-pv --locked --test prepared_application -- --ignored pv",
        "cargo test -p seedbox-machine-pv --locked --test simulation_control -- --ignored pv",
        "cargo test -p seedbox-server --locked --features kvm-tests --test native_pv_service --",
        "cargo test -p seedbox-server --locked --features kvm-tests --test native_controller_service --",
    ]
    .map(|line| {
        line.split_whitespace()
            .map(String::from)
            .collect::<Vec<_>>()
    });
    assert_eq!(
        runs.iter().map(|job| job.args.clone()).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        runs.iter()
            .map(|job| job.args.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        22
    );
    assert_eq!(
        runs.iter().map(|job| job.name.as_str()).collect::<Vec<_>>(),
        [
            "machine-tcg/seedbox-machine-tcg/--lib/",
            "machine-tcg/seedbox-machine-tcg/--test/native_machine",
            "machine-tcg/seedbox-driver/--test/native_driver",
            "machine-tcg/seedbox-driver/--test/tcg_shutdown",
            "machine-tcg/seedbox-driver/--test/driver_guest_services",
            "machine-tcg/seedbox-machine-tcg/--test/guest_services",
            "machine-tcg/seedbox-machine-tcg/--test/prepared_application",
            "machine-tcg/seedbox-machine-tcg/--test/simulation_control",
            "machine-tcg/seedbox-server/--test/native_tcg_service",
            "machine-tcg/seedbox-server/--test/native_controller_service",
            "machine-pv-x86/seedbox-machine-pv/--lib/",
            "machine-pv-x86/seedbox-machine-pv/--test/native_machine",
            "machine-pv-x86/seedbox-driver/--test/native_driver",
            "machine-pv-x86/seedbox-machine-pv/--test/pv_progress",
            "machine-pv-x86/seedbox-driver/--test/native_pv_driver",
            "machine-pv-x86/seedbox-driver/--test/pv_instructions",
            "machine-pv-x86/seedbox-driver/--test/driver_guest_services",
            "machine-pv-x86/seedbox-machine-pv/--test/guest_services",
            "machine-pv-x86/seedbox-machine-pv/--test/prepared_application",
            "machine-pv-x86/seedbox-machine-pv/--test/simulation_control",
            "machine-pv-x86/seedbox-server/--test/native_pv_service",
            "machine-pv-x86/seedbox-server/--test/native_controller_service"
        ]
    );
    for job in runs {
        assert!(!job.args.iter().any(|arg| arg.starts_with("--test-threads")));
    }
}
#[test]
fn test_machine_suites_schedule_feature_gated_public_service_targets_unfiltered() {
    for (suite, feature, targets) in [
        (
            "machine-tcg",
            "tcg-tests",
            &["native_tcg_service", "native_controller_service"][..],
        ),
        (
            "machine-pv-x86",
            "kvm-tests",
            &["native_pv_service", "native_controller_service"][..],
        ),
    ] {
        let runs = machine_suite_runs(
            &suites(&[suite]),
            &Environment::new(),
            std::path::Path::new("/fixture"),
        );
        let service: Vec<_> = runs
            .iter()
            .filter(|job| job.args.iter().any(|a| a == "seedbox-server"))
            .collect();
        assert_eq!(service.len(), targets.len(), "{suite}");
        for (job, target) in service.iter().zip(targets) {
            assert_eq!(
                job.args,
                strings(&[
                    "cargo",
                    "test",
                    "-p",
                    "seedbox-server",
                    "--locked",
                    "--features",
                    feature,
                    "--test",
                    target,
                    "--",
                ])
            );
            // Feature-gated, not ignored: no --ignored and no libtest filter,
            // so run_machine's --list counts and then runs the whole target.
            assert!(!job.args.iter().any(|a| a == "--ignored"));
            assert_eq!(job.args.last().unwrap(), "--");
            assert_eq!(job.name, format!("{suite}/seedbox-server/--test/{target}"));
        }
    }
}
#[test]
fn test_service_release_build_uses_the_target_the_service_tests_read() {
    // The service tests resolve release/seedbox under CARGO_TARGET_DIR, so the
    // release build must keep it (and the checkout target when it is unset).
    for environment in [
        env(&[("PATH", "/bin"), ("CARGO_TARGET_DIR", "/agent-cache")]),
        env(&[("PATH", "/bin")]),
    ] {
        let job = service_release_build(&environment, std::path::Path::new("/fixture"));
        assert_eq!(
            job.args,
            strings(&["cargo", "build", "--release", "--locked", "-p", "seedbox"])
        );
        assert_eq!(job.environment, environment);
    }
}
#[test]
fn test_unselected_machine_suites_have_no_runs() {
    assert!(machine_suite_runs(
        &suites(&["linux-oci"]),
        &Environment::new(),
        std::path::Path::new("/fixture")
    )
    .is_empty());
}
#[test]
fn test_privileged_execution_forwards_only_explicit_test_inputs() {
    assert_eq!(
        privileged(
            Some(&strings(&["sudo", "-n", "env"])),
            strings(&["/test"]),
            &env(&[
                ("SEEDBOX_TEST_KERNEL", "/kernel"),
                ("TMPDIR", "/tmp/owned"),
                ("UNRELATED_SECRET", "fixture")
            ])
        ),
        strings(&[
            "sudo",
            "-n",
            "env",
            "SEEDBOX_TEST_KERNEL=/kernel",
            "TMPDIR=/tmp/owned",
            "/test"
        ])
    );
}
#[test]
fn test_a_failed_step_does_not_hide_later_checks() {
    let t = fixture();
    let mut c = Checks::new(Real, t.0.clone(), std::env::vars().collect());
    assert!(!c.step("failed", strings(&["/bin/sh", "-c", "exit 7"])));
    assert!(c.step(
        "later",
        strings(&["/bin/sh", "-c", "printf later > marker"])
    ));
    assert!(c.failed);
    assert!(t.0.join("marker").exists());
}

#[test]
fn portable_list_observes_and_executes_nothing() {
    let t = fixture();
    let mut c = checks(&t.0);
    let o = options(strings(&["--list"])).unwrap();
    let h = observe(&mut c, &o.suites).unwrap();
    assert_eq!(coordinate(&mut c, &h, &o).unwrap(), 0);
    assert!(c.commands.jobs.is_empty());
}
#[test]
fn portable_failure_blocks_all_native_work_but_not_outer_prerequisites() {
    let t = fixture();
    let mut c = checks(&t.0);
    c.commands.replies.extend([
        Outcome {
            code: 1,
            stdout: String::new(),
        },
        Outcome::default(),
        Outcome {
            code: 1,
            stdout: String::new(),
        },
    ]);
    let o = options(strings(&["linux-oci"])).unwrap();
    assert_eq!(coordinate(&mut c, &host(), &o).unwrap(), 1);
    assert_eq!(
        c.commands
            .jobs
            .iter()
            .map(|j| j.name.as_str())
            .collect::<Vec<_>>(),
        ["fmt", "clippy", "shell-discovery"]
    );
}
#[test]
fn portable_stage_keeps_outer_fmt_and_clippy_out() {
    let t = fixture();
    let mut c = checks(&t.0);
    let o = options(strings(&["--portable-stage"])).unwrap();
    assert_eq!(coordinate(&mut c, &host(), &o).unwrap(), 0);
    assert!(c
        .commands
        .jobs
        .iter()
        .all(|job| job.name != "fmt" && !job.name.contains("clippy")));
    assert!(c
        .commands
        .jobs
        .iter()
        .any(|job| job.name == "workspace-feature-build"));
}

#[test]
fn empty_native_selection_and_empty_artifacts_are_errors() {
    let t = fixture();
    let mut c = checks(&t.0);
    let job = machine_suite_runs(&suites(&["machine-pv-x86"]), &Environment::new(), &t.0).remove(0);
    assert!(run_machine(&mut c, job)
        .unwrap_err()
        .contains("no native tests"));
    assert_eq!(c.commands.jobs.len(), 1);
    assert!(cargo_tests(
        &mut c,
        "seedbox",
        strings(&["cargo", "test"]),
        &[],
        None,
        false
    )
    .unwrap_err()
    .contains("no test executables"));
}
#[test]
fn test_time_policy_failure_blocks_backend_execution() {
    let t = fixture();
    let file = t.0.join("crates/example/src/lib.rs");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        file,
        "#[cfg(test)] fn not_a_signal(){std::thread::sleep(delay);}",
    )
    .unwrap();
    let mut c = checks(&t.0);
    portable(&mut c).unwrap();
    assert!(c.failed);
    assert_eq!(
        c.commands
            .jobs
            .iter()
            .map(|job| job.name.as_str())
            .collect::<Vec<_>>(),
        ["shell-discovery"]
    );
    assert_eq!(c.timings.len(), 2);
}
#[test]
fn portable_order_is_single_owned() {
    let t = fixture();
    let mut c = checks(&t.0);
    c.environment
        .insert("CARGO_TARGET_DIR".into(), "/workspace".into());
    portable(&mut c).unwrap();
    let jobs = &c.commands.jobs;
    assert_eq!(
        jobs.iter().map(|j| j.name.as_str()).collect::<Vec<_>>(),
        [
            "shell-discovery",
            "workspace-feature-build",
            "workspace-tests"
        ]
    );
    assert!(jobs
        .iter()
        .filter(|j| j.name.starts_with("workspace"))
        .all(|j| j.environment.contains_key("CARGO_TARGET_DIR")));
    assert_eq!(
        jobs[1].args,
        strings(&[
            "cargo",
            "test",
            "--workspace",
            "--locked",
            "--all-features",
            "--no-run"
        ])
    );
    assert_eq!(
        jobs[2].args,
        strings(&["cargo", "test", "--workspace", "--locked"])
    );
    let labels = [
        "shell-syntax",
        "test-time-policy",
        "workspace-feature-build",
        "workspace-tests",
    ];
    assert_eq!(c.timings.len(), labels.len());
    for (row, label) in c.timings.iter().zip(labels) {
        assert!(row.starts_with(&format!("timing: portable/{label}: ")));
        assert!(row.contains(" seconds (user "));
        assert!(row.contains(", sys "));
    }
}
#[test]
fn process_timeout_reaps_owned_descendants() {
    if let Some(root) = std::env::var_os("SEEDBOX_CHECK_TIMEOUT_CHILD") {
        let root = PathBuf::from(root);
        let mut real = Real::install_signals().unwrap();
        let mut job = Job::new(
            "timeout",
            strings(&["/bin/sh", "-c", "sleep 20 & echo $! > descendant; wait"]),
            &std::env::vars().collect(),
            &root,
        );
        job.timeout = Some(Duration::from_millis(150));
        assert_eq!(
            real.execute(&job).unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        // Parsing proves the descendant started; only Linux checks it was reaped.
        #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
        let pid: libc::pid_t = fs::read_to_string(root.join("descendant"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        #[cfg(target_os = "linux")]
        {
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        return;
    }
    let t = fixture();
    let mut e: Environment = std::env::vars().collect();
    e.insert(
        "SEEDBOX_CHECK_TIMEOUT_CHILD".into(),
        t.0.to_string_lossy().into(),
    );
    let mut job = Job::new(
        "isolated-timeout",
        vec![
            std::env::current_exe().unwrap().to_string_lossy().into(),
            "--exact".into(),
            "tests::process_timeout_reaps_owned_descendants".into(),
        ],
        &e,
        &t.0,
    );
    job.capture = true;
    job.timeout = Some(Duration::from_secs(5));
    let result = Real.execute(&job).unwrap();
    assert_eq!(result.code, 0);
    assert!(
        result.stdout.contains("1 passed; 0 failed"),
        "timeout child test did not execute: {}",
        result.stdout
    );
}
#[test]
fn process_interruption_cleans_group_and_does_not_start_later_commands() {
    if let Some(root) = std::env::var_os("SEEDBOX_CHECK_SIGNAL_CHILD") {
        let root = PathBuf::from(root);
        let ready = root.clone();
        let pid = std::process::id();
        let waiter = std::thread::spawn(move || {
            seedbox_test_time::HangGuard::new(
                "positive worker publication before signal readiness; retain 2s for contended shell startup",
                Duration::from_secs(2),
            ).wait_for(|| ready.join("worker").is_file().then_some(())).unwrap();
            publish_ready_file(&ready.join("ready"), pid.to_string().as_bytes(), |_| {});
        });
        let mut real = Real::install_signals().unwrap();
        let mut job = Job::new(
            "interrupt",
            strings(&[
                "/bin/sh",
                "-c",
                "sleep 20 & echo $! > descendant; echo $$ > worker; wait",
            ]),
            &std::env::vars().collect(),
            &root,
        );
        job.timeout = Some(Duration::from_secs(3));
        assert_eq!(
            real.execute(&job).unwrap_err().kind(),
            std::io::ErrorKind::Interrupted
        );
        waiter.join().unwrap();
        let later = Job::new(
            "later",
            strings(&["/bin/sh", "-c", "touch should-not-exist"]),
            &std::env::vars().collect(),
            &root,
        );
        assert_eq!(
            real.execute(&later).unwrap_err().kind(),
            std::io::ErrorKind::Interrupted
        );
        assert!(!root.join("should-not-exist").exists());
        #[cfg(target_os = "linux")]
        for name in ["worker", "descendant"] {
            let pid: libc::pid_t = fs::read_to_string(root.join(name))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        return;
    }
    let t = fixture();
    let ready = t.0.clone();
    let sender =
        std::thread::spawn(move || {
            seedbox_test_time::HangGuard::new(
            "positive atomic checker readiness before SIGTERM; retain 3s loaded-host startup room",
            Duration::from_secs(3),
        ).wait_for(|| ready.join("ready").is_file().then_some(())).unwrap();
            let pid: libc::pid_t = fs::read_to_string(ready.join("ready"))
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        });
    let mut e: Environment = std::env::vars().collect();
    e.insert(
        "SEEDBOX_CHECK_SIGNAL_CHILD".into(),
        t.0.to_string_lossy().into(),
    );
    let mut job = Job::new(
        "isolated-interrupt",
        vec![
            std::env::current_exe().unwrap().to_string_lossy().into(),
            "--exact".into(),
            "tests::process_interruption_cleans_group_and_does_not_start_later_commands".into(),
        ],
        &e,
        &t.0,
    );
    job.capture = true;
    job.timeout = Some(Duration::from_secs(5));
    let result = Real.execute(&job).unwrap();
    assert_eq!(result.code, 0);
    assert!(
        result.stdout.contains("1 passed; 0 failed"),
        "interruption child test did not execute: {}",
        result.stdout
    );
    sender.join().unwrap();
}

#[test]
fn release_bundle_uses_cargos_exact_current_cli_not_an_old_target_path() {
    let current = artifact(
        std::path::Path::new("/custom-cache/release/seedbox"),
        "seedbox",
        false,
    );
    let test = artifact(std::path::Path::new("/tests/seedbox"), "seedbox", true);
    let other = artifact(std::path::Path::new("/other-bin"), "other", false);
    assert_eq!(
        release_cli_artifact(&[test, other, current.clone(), current.clone()].join("\n")).unwrap(),
        PathBuf::from("/custom-cache/release/seedbox")
    );
    assert!(release_cli_artifact("").is_err());
    assert!(release_cli_artifact(
        &[
            current,
            artifact(std::path::Path::new("/second/seedbox"), "seedbox", false)
        ]
        .join("\n")
    )
    .is_err());
    assert!(release_cli_artifact(&artifact(
        std::path::Path::new("relative"),
        "seedbox",
        false
    ))
    .is_err());
}

#[test]
fn timing_uses_self_plus_reaped_children_and_preserves_units() {
    let before = CpuSample {
        own: Cpu {
            user: 1.25,
            system: 0.5,
        },
        children: Cpu {
            user: 2.5,
            system: 0.25,
        },
    };
    let after = CpuSample {
        own: Cpu {
            user: 1.75,
            system: 0.75,
        },
        children: Cpu {
            user: 4.5,
            system: 1.0,
        },
    };
    assert_eq!(
        timing_row("shell-syntax", 1.125, before, after),
        "timing: portable/shell-syntax: 1.125 seconds (user 2.500, sys 1.000)"
    );
    assert_eq!(
        timing_row("workspace-tests", 0.0, before, before),
        "timing: portable/workspace-tests: 0.000 seconds (user 0.000, sys 0.000)"
    );
}
#[test]
fn shell_timing_covers_discovery_and_all_checks_including_failure() {
    let t = fixture();
    for name in ["a.sh", "b.sh"] {
        fs::write(t.0.join(name), "").unwrap();
    }
    let mut c = checks(&t.0);
    c.commands.replies.extend([
        Outcome {
            code: 0,
            stdout: "a.sh\0b.sh\0".into(),
        },
        Outcome::default(),
        Outcome {
            code: 1,
            stdout: String::new(),
        },
    ]);
    portable(&mut c).unwrap();
    assert!(c.failed);
    assert_eq!(
        c.commands
            .jobs
            .iter()
            .map(|j| j.name.as_str())
            .collect::<Vec<_>>(),
        ["shell-discovery", "shell-syntax", "shell-syntax"]
    );
    assert_eq!(c.timings.len(), 1);
    assert!(c.timings[0].starts_with("timing: portable/shell-syntax: "));
    assert!(c.timings[0].contains(" seconds (user "));
    assert!(c.timings[0].contains(", sys "));
    let mut failed = checks(&t.0);
    failed.commands.replies.push_back(Outcome {
        code: 1,
        stdout: String::new(),
    });
    portable(&mut failed).unwrap();
    assert_eq!(failed.commands.jobs.len(), 1);
    assert_eq!(failed.timings.len(), 1);
}
#[test]
fn timing_error_rows_do_not_hide_the_underlying_failure() {
    let t = fixture();
    let mut c = checks(&t.0);
    let result: Result<(), String> =
        timed(&mut c, "shell-syntax", |_| Err("discovery failed".into()));
    assert_eq!(result.unwrap_err(), "discovery failed");
    assert_eq!(c.timings.len(), 1);
    assert!(c.timings[0].starts_with("timing: portable/shell-syntax: "));
    assert!(c.commands.jobs.is_empty());
}

fn wrapper_job(
    root: &std::path::Path,
    wrapper: &str,
    args: &[&str],
    environment: &Environment,
) -> Job {
    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let bash = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("bash"))
        .find(|path| executable(path))
        .expect("run checker tests inside nix develop, which supplies bash");
    let mut command = vec![
        bash.to_string_lossy().into(),
        "-c".into(),
        "exec \"$@\" 2>&1".into(),
        "wrapper-test".into(),
        bash.to_string_lossy().into(),
    ];
    command.push(
        checkout
            .join("harness")
            .join(wrapper)
            .to_string_lossy()
            .into(),
    );
    command.extend(strings(args));
    let mut job = Job::new("wrapper", command, environment, root);
    job.capture = true;
    job.timeout = Some(Duration::from_secs(3));
    job
}
#[test]
fn compatibility_wrappers_only_exec_the_supplied_tool_and_preserve_arguments() {
    let t = fixture();
    let tool = t.0.join("coordinator with spaces");
    install_executable(
        &tool,
        "#!/bin/sh\nprintf 'arg:%s\\n' \"$@\"\nprintf 'cwd:%s\\n' \"$PWD\"\nprintf 'incremental:%s\\n' \"${CARGO_INCREMENTAL-unset}\"\n",
    );
    let e = env(&[
        ("PATH", "/no-tools"),
        ("SEEDBOX_CHECK", tool.to_str().unwrap()),
        ("CARGO_INCREMENTAL", "1"),
    ]);
    let result = Real
        .execute(&wrapper_job(&t.0, "check.sh", &["portable", "--list"], &e))
        .unwrap();
    assert_eq!(result.code, 0);
    assert!(result.stdout.starts_with("arg:portable\narg:--list\n"));
    assert!(result.stdout.contains("incremental:0\n"));
    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    assert!(result
        .stdout
        .contains(&format!("cwd:{}\n", checkout.display())));
    for args in [vec![], vec!["portable"]] {
        let result = Real
            .execute(&wrapper_job(&t.0, "acceptance.sh", &args, &e))
            .unwrap();
        assert_eq!(result.code, 0);
        assert!(result
            .stdout
            .starts_with("arg:--portable-stage\narg:portable\n"));
        assert!(result.stdout.contains("incremental:0\n"));
    }
}
#[test]
fn missing_provisioned_tool_is_actionable_and_has_no_build_fallback() {
    let t = fixture();
    for environment in [
        env(&[("PATH", "/no-tools")]),
        env(&[("PATH", "/no-tools"), ("SEEDBOX_CHECK", "/missing/checker")]),
    ] {
        for (wrapper, args) in [
            ("check.sh", vec!["--list"]),
            ("acceptance.sh", vec!["portable"]),
        ] {
            let result = Real
                .execute(&wrapper_job(&t.0, wrapper, &args, &environment))
                .unwrap();
            assert_eq!(result.code, 2);
            assert!(result.stdout.contains("nix develop"));
            assert!(result.stdout.contains("never builds or downloads"));
        }
    }
}
#[test]
fn acceptance_compatibility_rejects_nonportable_and_extra_arguments() {
    let t = fixture();
    let e = env(&[("PATH", "/no-tools")]);
    for args in [vec!["linux"], vec!["portable", "extra"], vec!["--list"]] {
        let result = Real
            .execute(&wrapper_job(&t.0, "acceptance.sh", &args, &e))
            .unwrap();
        assert_eq!(result.code, 2);
        assert!(result
            .stdout
            .starts_with("Usage: harness/acceptance.sh portable;"));
        assert!(!result.stdout.contains("Rust coordinator unavailable"));
    }
}

#[test]
fn malformed_unix_arguments_are_rejected_without_lossy_suite_acceptance() {
    use std::os::unix::ffi::OsStringExt;
    let bad = std::ffi::OsString::from_vec(b"portable\xff".to_vec());
    let error = arguments(vec![bad]).unwrap_err();
    assert!(error.contains("argument 1"));
    assert!(error.contains("non-UTF-8"));
    assert!(error.contains("--help"));
    assert_eq!(
        arguments(vec!["portable".into(), "--list".into()]).unwrap(),
        strings(&["portable", "--list"])
    );
    assert!(options(arguments(vec!["λ".into()]).unwrap())
        .unwrap_err()
        .contains("unknown suite"));
}

#[test]
fn oci_artifacts_follow_actual_package_feature_and_socket_permissions() {
    let t = fixture();
    let socket = t.0.join("engine.sock");
    let binary = t.0.join("oci-test");
    for target in ["oci_engine", "newly_discovered_oci_target"] {
        for accessible in [false, true] {
            let mut h = host();
            h.sockets = vec![(socket.clone(), accessible)];
            h.root_prefix = Some(strings(&["sudo", "-n", "env", "PATH=/fixture/shell"]));
            let e = env(&[
                ("DOCKER_HOST", &format!("unix://{}", socket.display())),
                ("CARGO_TARGET_DIR", "/workspace-cache"),
                ("GH_TOKEN", "not-forwarded"),
            ]);
            assert!(requirements(Suite::LinuxOci, &h, &e, &t.0).is_empty());
            let mut c = Checks::new(Spy::default(), t.0.clone(), e);
            c.commands.replies.push_back(Outcome {
                code: 0,
                stdout: artifact(&binary, target, true),
            });
            let requires_root = h.engine_needs_root(&c.environment);
            assert_eq!(requires_root, !accessible);
            cargo_tests(
                &mut c,
                "seedbox-application",
                strings(&[
                    "cargo",
                    "test",
                    "-p",
                    "seedbox-application",
                    "--locked",
                    "--test",
                    target,
                ]),
                &strings(&["oci-tests"]),
                h.root_prefix.as_deref(),
                requires_root,
            )
            .unwrap();
            assert_eq!(c.commands.jobs.len(), 2);
            assert_eq!(
                c.commands.jobs[0].args,
                strings(&[
                    "cargo",
                    "test",
                    "-p",
                    "seedbox-application",
                    "--locked",
                    "--test",
                    target,
                    "--features",
                    "oci-tests",
                    "--no-run",
                    "--message-format=json"
                ])
            );
            assert_eq!(
                c.commands.jobs[0].environment["CARGO_TARGET_DIR"],
                "/workspace-cache"
            );
            let mut expected = Vec::new();
            if !accessible {
                expected = strings(&["sudo", "-n", "env", "PATH=/fixture/shell"]);
                expected.push(format!("DOCKER_HOST=unix://{}", socket.display()));
                expected.push(format!(
                    "TMPDIR={}",
                    c.commands.jobs[1].environment["TMPDIR"]
                ));
            }
            expected.extend([binary.to_string_lossy().into(), "--test-threads=1".into()]);
            assert_eq!(c.commands.jobs[1].args, expected);
        }
    }
    // Root-only sockets do not elevate unrelated application feature artifacts.
    let mut c = checks(&t.0);
    c.commands.replies.push_back(Outcome {
        code: 0,
        stdout: artifact(&binary, "unrelated", true),
    });
    cargo_tests(
        &mut c,
        "seedbox-application",
        strings(&["cargo", "test"]),
        &strings(&["unrelated-feature"]),
        Some(&strings(&["sudo", "-n", "env"])),
        true,
    )
    .unwrap();
    assert_eq!(
        c.commands.jobs[1].args,
        vec![
            binary.to_string_lossy().into_owned(),
            "--test-threads=1".to_string()
        ]
    );
}
#[test]
fn malformed_environment_is_fallible_without_lossy_paths_or_value_disclosure() {
    use std::os::unix::ffi::OsStringExt;
    let error = environment_values([(
        std::ffi::OsString::from_vec(b"NAME\xff".to_vec()),
        "secret-value".into(),
    )])
    .unwrap_err();
    assert!(error.contains("variable name"));
    assert!(error.contains("non-UTF-8"));
    assert!(!error.contains("secret"));
    let error = environment_values([(
        "SEEDBOX_TEST_KERNEL".into(),
        std::ffi::OsString::from_vec(b"secret-path\xff".to_vec()),
    )])
    .unwrap_err();
    assert!(error.contains("SEEDBOX_TEST_KERNEL"));
    assert!(error.contains("non-UTF-8"));
    assert!(error.contains("UTF-8 value"));
    assert!(!error.contains("secret-path"));
    assert_eq!(
        environment_values([("SEEDBOX_TEST_KERNEL".into(), "/valid/λ".into())]).unwrap(),
        env(&[("SEEDBOX_TEST_KERNEL", "/valid/λ")])
    );
}

#[test]
fn invalid_captured_process_stdout_is_invalid_data_and_fails_the_caller() {
    let t = fixture();
    let mut c = Checks::new(Real, t.0.clone(), std::env::vars().collect());
    let mut job = c.job(
        "shell-discovery",
        strings(&[
            "/bin/sh",
            "-c",
            "printf 'private-prefix\\377private-suffix.sh\\000'",
        ]),
    );
    job.capture = true;
    let error = c.commands.execute(&job).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    let message = error.to_string();
    assert!(message.contains("shell-discovery"));
    assert!(message.contains("UTF-8"));
    assert!(!message.contains("private-prefix"));
    assert!(!message.contains("private-suffix"));
    // A second independent invocation exercises the Checks failure accumulator,
    // not a retry accepting the first operation's invalid output.
    assert!(c.run(job).is_none());
    assert!(c.failed);
}
#[test]
fn valid_unicode_captured_process_stdout_is_preserved() {
    let t = fixture();
    let mut c = Checks::new(Real, t.0.clone(), std::env::vars().collect());
    let mut job = c.job(
        "unicode-output",
        strings(&["/bin/sh", "-c", "printf 'λ雪\\n'"]),
    );
    job.capture = true;
    assert_eq!(c.run(job).unwrap(), "λ雪\n");
    assert!(!c.failed);
}

/// Write an executable fixture with forks excluded until it is closed: sibling
/// tests in this binary spawn through `seedbox_exec` and must not copy its
/// writable descriptor (ETXTBSY). The checker cannot dev-depend on
/// seedbox-test, which it checks, so it uses the lock directly.
fn install_executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _writing = seedbox_exec::writing();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn scan_only_policy_mode_is_explicit_and_cannot_select_other_work() {
    assert!(
        options(strings(&["--test-time-policy"]))
            .unwrap()
            .test_time_policy
    );
    assert!(!options(vec![]).unwrap().test_time_policy);
    for other in [
        "portable",
        "linux-apps",
        "--list",
        "--portable-stage",
        "--probe-linux",
    ] {
        assert_eq!(
            options(strings(&["--test-time-policy", other])).unwrap_err(),
            "--test-time-policy must be used without suites or other modes"
        );
    }
}

#[test]
fn scan_only_stage_reports_success_findings_and_parse_failure() {
    let t = fixture();
    let file = t.0.join("crates/example/tests/fixture.rs");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        "fn f(){activity::Controlled::sleep_until(&timer,deadline);}",
    )
    .unwrap();
    assert_eq!(test_time_policy_stage(&t.0).unwrap(), 0);
    fs::write(&file, "fn f(){tokio::time::sleep(delay);}").unwrap();
    assert_eq!(test_time_policy_stage(&t.0).unwrap(), 1);
    fs::write(&file, "not Rust").unwrap();
    assert!(test_time_policy_stage(&t.0)
        .unwrap_err()
        .contains("fixture.rs"));
}

#[cfg(target_os = "linux")]
#[test]
fn isolated_dr_requires_positive_preflights_without_root_or_kvm_probes() {
    let temp = fixture();
    for name in ["bin", "lib", "runtime/bin64", "cgroup"] {
        fs::create_dir_all(temp.0.join(name)).unwrap();
    }
    for name in [
        "bin/seedbox-dr-isolate",
        "bin/unix-pid1",
        "bin/unix-eagain",
        "bin/unix-supervisor",
        "bin/application",
        "runtime/bin64/drrun",
    ] {
        install_executable(&temp.0.join(name), "fixture");
    }
    fs::write(temp.0.join("lib/libpreserve.so"), "fixture").unwrap();
    let mut c = checks(&temp.0);
    c.environment = env(&[
        ("SEEDBOX_DYNAMORIO_FIXTURES", temp.0.to_str().unwrap()),
        (
            "SEEDBOX_DYNAMORIO_CGROUP_ROOT",
            temp.0.join("cgroup").to_str().unwrap(),
        ),
    ]);
    c.commands.replies.push_back(Outcome {
        code: 2,
        ..Outcome::default()
    });
    c.commands.replies.push_back(Outcome::default());
    let options = options(strings(&["dynamorio-isolated", "--list"])).unwrap();
    let h = observe(&mut c, &options.suites).unwrap();
    assert_eq!(coordinate(&mut c, &h, &options).unwrap(), 2);
    assert_eq!(c.commands.jobs.len(), 2);
    assert!(c.commands.jobs.iter().all(|j| j.args[1] == "preflight"));
    assert!(
        requirements(Suite::DynamorioIsolated, &h, &c.environment, &temp.0)
            .iter()
            .any(|s| s.contains("pid1 capability preflight"))
    );
    let h = observe(&mut c, &options.suites).unwrap();
    assert_eq!(coordinate(&mut c, &h, &options).unwrap(), 0);
}
