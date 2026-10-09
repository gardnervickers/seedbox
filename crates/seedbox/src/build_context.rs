//! Docker build contexts: preserve source metadata, honor the selected ignore
//! file, never follow host symlinks, and stream a bounded tar to the engine.
use crate::{Error, Result};
use std::{fs, path::Path};
struct Rule {
    matcher: globset::GlobMatcher,
    exclude: bool,
    include_prefix: std::path::PathBuf,
}
fn rules(text: &str) -> Result<Vec<Rule>> {
    let mut result = Vec::new();
    for raw in text.trim_start_matches('\u{feff}').lines() {
        if raw.starts_with('#') {
            continue;
        }
        let mut pattern = raw.trim();
        if pattern.is_empty() {
            continue;
        }
        let exclude = !pattern.starts_with('!');
        if !exclude {
            pattern = pattern[1..].trim();
        }
        pattern = pattern.trim_matches('/');
        if pattern == "." {
            continue;
        }
        if pattern.is_empty() {
            return Err(Error::Setup("empty .dockerignore exclusion".into()));
        }
        let mut components = Vec::new();
        for component in pattern.split('/') {
            match component {
                "" | "." => {}
                ".." if components.last().is_some_and(|part| *part != "..") => {
                    components.pop();
                }
                part => components.push(part),
            }
        }
        let pattern = components.join("/");
        if pattern.is_empty() {
            continue;
        }
        let matcher = globset::GlobBuilder::new(&pattern)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|error| Error::Setup(format!(".dockerignore pattern {pattern}: {error}")))?
            .compile_matcher();
        let include_prefix = pattern
            .split('/')
            .take_while(|part| !part.contains(['*', '?', '[', '\\']))
            .collect::<std::path::PathBuf>();
        result.push(Rule {
            matcher,
            exclude,
            include_prefix,
        });
    }
    Ok(result)
}
fn excluded(path: &Path, rules: &[Rule]) -> bool {
    let mut exclude = false;
    for rule in rules {
        if path
            .ancestors()
            .any(|ancestor| rule.matcher.is_match(ancestor))
        {
            exclude = rule.exclude;
        }
    }
    exclude
}
pub(crate) fn archive(
    root: &Path,
    dockerfile: &Path,
    output: &Path,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let relative = dockerfile
        .strip_prefix(root)
        .map_err(|_| Error::Setup("Dockerfile must be inside its build context".into()))?;
    let specific = dockerfile.with_file_name(format!(
        "{}.dockerignore",
        dockerfile.file_name().unwrap_or_default().to_string_lossy()
    ));
    let ignore = if specific.is_file() {
        specific
    } else {
        root.join(".dockerignore")
    };
    let patterns = match fs::read_to_string(&ignore) {
        Ok(text) => rules(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(Error::Setup(format!("{}: {error}", ignore.display()))),
    };
    let mut tar = tar::Builder::new(std::io::BufWriter::new(fs::File::create(output)?));
    tar.follow_symlinks(false);
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            let Ok(name) = entry.path().strip_prefix(root) else {
                return true;
            };
            !entry.file_type().is_dir()
                || !excluded(name, &patterns)
                || relative.starts_with(name)
                || ignore.starts_with(entry.path())
                || patterns.iter().any(|rule| {
                    !rule.exclude
                        && (rule.include_prefix.starts_with(name)
                            || name.starts_with(&rule.include_prefix))
                })
        })
    {
        check()?;
        let entry = entry
            .map_err(|error| Error::Setup(format!("build context {}: {error}", root.display())))?;
        let path = entry.path();
        let name = path
            .strip_prefix(root)
            .map_err(|error| Error::Setup(error.to_string()))?;
        if name.as_os_str().is_empty()
            || (name != relative && path != ignore && excluded(name, &patterns))
        {
            continue;
        }
        if !(entry.file_type().is_file()
            || entry.file_type().is_dir()
            || entry.file_type().is_symlink())
        {
            return Err(Error::Setup(format!(
                "build context {}: unsupported special file",
                path.display()
            )));
        }
        if entry.file_type().is_file() {
            total = total
                .checked_add(fs::symlink_metadata(path)?.len())
                .filter(|size| *size <= 4 << 30)
                .ok_or_else(|| {
                    Error::Setup(
                        "build context exceeds 4 GiB; exclude build output with .dockerignore"
                            .into(),
                    )
                })?;
        }
        tar.append_path_with_name(path, name)?;
    }
    std::io::Write::flush(&mut tar.into_inner()?)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn docker_patterns_are_root_relative_and_exceptions_override_parents() {
        let patterns = rules("# comment\ntarget\n**/*.secret\ncache/\n!cache/keep\n").unwrap();
        for path in [
            "target/file",
            "file.secret",
            "sub/file.secret",
            "cache/discard",
        ] {
            assert!(excluded(Path::new(path), &patterns), "{path}");
        }
        for path in ["sub/target/file", "cache/keep", "src/main.rs"] {
            assert!(!excluded(Path::new(path), &patterns), "{path}");
        }
    }
    #[test]
    fn ignored_unreadable_directories_are_pruned_but_exceptions_are_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("source");
        fs::create_dir_all(root.join("data")).unwrap();
        fs::create_dir_all(root.join("cache")).unwrap();
        fs::write(root.join("Dockerfile"), "FROM scratch").unwrap();
        fs::write(root.join(".dockerignore"), "data\ncache\n!cache/keep\n").unwrap();
        fs::write(root.join("cache/keep"), "keep").unwrap();
        fs::set_permissions(root.join("data"), fs::Permissions::from_mode(0o0)).unwrap();
        let output = dir.path().join("context.tar");
        let result = archive(&root, &root.join("Dockerfile"), &output, &|| Ok(()));
        fs::set_permissions(root.join("data"), fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
        let paths = tar::Archive::new(fs::File::open(output).unwrap())
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().into_owned())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"cache/keep".into()));
        assert!(!paths.contains(&"data".into()));
    }
    #[test]
    fn context_excludes_secrets_and_keeps_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("source");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("Dockerfile"), "FROM scratch").unwrap();
        fs::write(root.join(".dockerignore"), "secret\nDockerfile\n").unwrap();
        fs::write(root.join("secret"), "must not upload").unwrap();
        symlink("/outside/context", root.join("link")).unwrap();
        let output = dir.path().join("context.tar");
        archive(&root, &root.join("Dockerfile"), &output, &|| Ok(())).unwrap();
        let entries: Vec<_> = tar::Archive::new(fs::File::open(output).unwrap())
            .entries()
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.path().unwrap().into_owned(),
                    entry.header().entry_type(),
                )
            })
            .collect();
        assert!(!entries.iter().any(|(path, _)| path == Path::new("secret")));
        assert!(entries
            .iter()
            .any(|(path, kind)| path == Path::new("link") && kind.is_symlink()));
        assert!(entries
            .iter()
            .any(|(path, _)| path == Path::new("Dockerfile")));
    }
}
