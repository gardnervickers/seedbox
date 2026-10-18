//! Developer bootstrap owns only missing checkouts, never existing local work.
mod support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use support::{diagnostic, root, run, success};

fn command(program: &str, directory: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .current_dir(directory);
    command
}

fn git(directory: &Path, args: &[&str]) -> Vec<u8> {
    success(
        command("git", directory)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args),
        30,
    )
    .stdout
}

fn repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "--quiet", "-b", "fixture"]);
    fs::write(path.join("source.c"), b"original\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "--quiet", "-m", "fixture"]);
}

struct Fixture {
    directory: tempfile::TempDir,
    checkout: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let upstream = directory.path().join("upstream dr");
        repo(&upstream);
        for name in ["elfutils", "libipt space #?%", "zlib"] {
            let dependency = directory.path().join(format!("upstream {name}"));
            repo(&dependency);
            git(
                &upstream,
                &[
                    "submodule",
                    "add",
                    "--quiet",
                    dependency.to_str().unwrap(),
                    &format!("third_party/{name}"),
                ],
            );
        }
        git(&upstream, &["commit", "--quiet", "-am", "dependencies"]);
        let parent = directory.path().join("upstream seedbox");
        repo(&parent);
        fs::create_dir(parent.join("tools")).unwrap();
        fs::copy(
            root().join("tools/dev-flake.sh"),
            parent.join("tools/dev-flake.sh"),
        )
        .unwrap();
        fs::copy(root().join(".envrc"), parent.join(".envrc")).unwrap();
        git(
            &parent,
            &[
                "submodule",
                "add",
                "--quiet",
                upstream.to_str().unwrap(),
                "vendor/dynamorio",
            ],
        );
        git(&parent, &["add", "."]);
        git(&parent, &["commit", "--quiet", "-m", "translator"]);
        let checkout = directory.path().join("checkout #?% space");
        git(
            directory.path(),
            &[
                "clone",
                "--quiet",
                parent.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );
        Self {
            directory,
            checkout,
        }
    }

    fn load(&self, checkout: &Path) -> Output {
        self.load_script(checkout, "")
    }

    fn load_script(&self, checkout: &Path, prelude: &str) -> Output {
        run(
            command("bash", checkout)
                .args([
                    "-c",
                    &format!(
                        r#"{prelude}
watch_file() {{ :; }}
use() {{ printf '%s\n' "$@" > "$BOOTSTRAP_FLAKE"; }}
source .envrc
"#
                    ),
                ])
                .env("BOOTSTRAP_FLAKE", self.directory.path().join("flake-args")),
            30,
        )
    }

    fn load_successfully(&self, checkout: &Path) -> String {
        let output = self.load(checkout);
        assert!(output.status.success(), "{}", diagnostic(&output));
        let args = fs::read_to_string(self.directory.path().join("flake-args")).unwrap();
        assert!(args.starts_with("flake\ngit+file:///"), "{args}");
        assert!(args.ends_with("?submodules=1\n"), "{args}");
        args
    }
}

#[test]
fn fresh_clone_initializes_nested_gitlinks_and_encodes_direnv_reference() {
    let fixture = Fixture::new();
    let dr = fixture.checkout.join("vendor/dynamorio");
    assert!(dr.is_dir());
    assert!(!dr.join(".git").exists());
    let args = fixture.load_successfully(&fixture.checkout);
    assert!(args.contains("checkout%20%23%3F%25%20space?"), "{args}");
    assert!(git(&fixture.checkout, &["diff", "--exit-code"]).is_empty());
    assert!(git(&dr, &["diff", "--exit-code"]).is_empty());
    for name in ["elfutils", "libipt space #?%", "zlib"] {
        let dependency = dr.join("third_party").join(name);
        assert!(dependency.join(".git").is_file());
        git(&dependency, &["rev-parse", "--verify", "HEAD"]);
    }
}

fn snapshot(repo: &Path) -> Vec<Vec<u8>> {
    let index = String::from_utf8(git(repo, &["rev-parse", "--git-path", "index"])).unwrap();
    vec![
        git(repo, &["rev-parse", "HEAD"]),
        git(repo, &["rev-parse", "--symbolic-full-name", "HEAD"]),
        fs::read(repo.join(index.trim())).unwrap(),
        git(repo, &["ls-files", "--stage", "-z"]),
        fs::read(repo.join("source.c")).unwrap(),
    ]
}

fn local_work(repo: &Path) {
    git(repo, &["checkout", "--quiet", "-b", "ongoing-work"]);
    fs::write(repo.join("local.txt"), b"local commit").unwrap();
    git(repo, &["add", "local.txt"]);
    git(repo, &["commit", "--quiet", "-m", "local commit"]);
    fs::write(repo.join("source.c"), b"staged\n").unwrap();
    git(repo, &["add", "source.c"]);
    fs::write(repo.join("source.c"), b"unstaged\n").unwrap();
    fs::write(repo.join("untracked.txt"), b"untracked\n").unwrap();
}

#[test]
fn missing_nested_dependency_preserves_existing_heads_branches_and_index_bytes() {
    let fixture = Fixture::new();
    fixture.load_successfully(&fixture.checkout);
    let dr = fixture.checkout.join("vendor/dynamorio");
    let existing = dr.join("third_party/zlib");
    local_work(&dr);
    local_work(&existing);
    let missing = dr.join("third_party/elfutils");
    fs::remove_dir_all(&missing).unwrap();
    let expected_dr = snapshot(&dr);
    let expected_existing = snapshot(&existing);
    fixture.load_successfully(&fixture.checkout);
    assert!(missing.join(".git").is_file());
    assert_eq!(snapshot(&dr), expected_dr);
    assert_eq!(snapshot(&existing), expected_existing);
    assert_eq!(fs::read(dr.join("untracked.txt")).unwrap(), b"untracked\n");
    assert_eq!(
        fs::read(existing.join("untracked.txt")).unwrap(),
        b"untracked\n"
    );
}

#[test]
fn repeated_load_does_not_update_or_fetch_initialized_checkouts() {
    let fixture = Fixture::new();
    fixture.load_successfully(&fixture.checkout);
    let dr = fixture.checkout.join("vendor/dynamorio");
    // Fetching any source now fails: a second load must need none of them.
    fs::remove_dir_all(fixture.directory.path().join("upstream dr")).unwrap();
    for name in ["elfutils", "libipt space #?%", "zlib"] {
        fs::remove_dir_all(fixture.directory.path().join(format!("upstream {name}"))).unwrap();
    }
    let before = snapshot(&dr);
    let output = fixture.load_script(
        &fixture.checkout,
        r#"
git() {
  local argument
  for argument in "$@"; do
    case "$argument" in
      submodule|fetch|clone) printf 'unexpected Git mutation: %s\n' "$*" >&2; return 77 ;;
    esac
  done
  command git "$@"
}
export -f git
"#,
    );
    assert!(output.status.success(), "{}", diagnostic(&output));
    assert_eq!(snapshot(&dr), before);
}

#[test]
fn linked_worktree_initializes_missing_sources_and_loads_its_own_reference() {
    let fixture = Fixture::new();
    let worktree = fixture.directory.path().join("linked #?% worktree");
    git(
        &fixture.checkout,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            worktree.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(worktree.join(".git").is_file());
    let args = fixture.load_successfully(&worktree);
    assert!(args.contains("linked%20%23%3F%25%20worktree?"), "{args}");
    assert!(worktree
        .join("vendor/dynamorio/third_party/elfutils/.git")
        .is_file());
}

#[test]
fn initialization_failure_stops_before_nix_with_recovery_guidance() {
    let fixture = Fixture::new();
    git(
        &fixture.checkout,
        &[
            "config",
            "submodule.vendor/dynamorio.url",
            fixture
                .directory
                .path()
                .join("missing remote")
                .to_str()
                .unwrap(),
        ],
    );
    let output = fixture.load(&fixture.checkout);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("cannot initialize submodule"), "{stderr}");
    assert!(stderr.contains("rerun bash tools/dev-flake.sh"), "{stderr}");
    assert!(!fixture.directory.path().join("flake-args").exists());
}

#[test]
fn broken_existing_git_pointer_is_not_reinitialized() {
    let fixture = Fixture::new();
    fixture.load_successfully(&fixture.checkout);
    let pointer = fixture.checkout.join("vendor/dynamorio/.git");
    fs::write(&pointer, b"gitdir: /nonexistent-fixture-gitdir\n").unwrap();
    fs::remove_file(fixture.directory.path().join("flake-args")).unwrap();
    let output = fixture.load(&fixture.checkout);
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("invalid submodule checkout"));
    assert_eq!(
        fs::read(pointer).unwrap(),
        b"gitdir: /nonexistent-fixture-gitdir\n"
    );
    assert!(!fixture.directory.path().join("flake-args").exists());
}
