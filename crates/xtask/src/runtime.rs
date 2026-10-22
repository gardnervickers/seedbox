//! Offline, host-native runtime packaging. Never called by runtime libraries.
use super::{capture, command, run, Result};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

pub(super) fn nix_output(root: &Path, name: &str) -> Result<PathBuf> {
    // Asset builds include live submodule edits on the initial fetch. The
    // development shell deliberately uses a reference without submodules.
    let mut path = String::new();
    for &byte in root.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            path.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(path, "%{byte:02X}")?;
        }
    }
    let output = capture(command("nix", root).args([
        "build",
        "--no-link",
        "--print-out-paths",
        &format!("git+file://{path}?submodules=1#{name}"),
    ]))?;
    let paths: Vec<_> = output.lines().collect();
    if paths.len() != 1 || !Path::new(paths[0]).is_absolute() {
        return Err(format!("{name}: expected one absolute Nix output, got {output:?}").into());
    }
    Ok(paths[0].into())
}

fn copy(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_file() {
        return Err(format!("runtime input {}: not a regular file", source.display()).into());
    }
    fs::create_dir_all(destination.parent().ok_or("missing destination parent")?)?;
    // Closure members (QEMU, its loader) are executed later: no fork may copy
    // a writable descriptor while the copy is open.
    let _writing = seedbox_exec::writing();
    fs::copy(source, destination)?;
    let mode = if fs::metadata(source)?.permissions().mode() & 0o111 != 0 {
        0o755
    } else {
        0o644
    };
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
    Ok(())
}

pub(super) fn guest(root: &Path, output: &Path, arch: &str) -> Result<()> {
    for (backend, attribute, required) in [
        ("pv", "machinePvAssets", "progress.tar"),
        ("tcg", "machineTcgAssets", "firmware"),
    ] {
        let input = nix_output(root, attribute)?;
        let guest_arch = if backend == "tcg" { "x86_64" } else { arch };
        let destination = output.join(backend).join(guest_arch);
        for name in ["kernel", "init", "command-line", required] {
            copy(&input.join(name), &destination.join(name))?;
        }
    }
    for (attribute, guest_arch) in [
        ("machineGuestFormatter", arch),
        ("machineGuestFormatterX86", "x86_64"),
    ] {
        let input = nix_output(root, attribute)?.join("bin/mke2fs");
        for backend in if guest_arch == arch {
            vec!["pv", "tcg"]
        } else {
            vec!["tcg"]
        } {
            if backend == "tcg" && guest_arch != "x86_64" {
                continue;
            }
            copy(
                &input,
                &output.join(backend).join(guest_arch).join("mke2fs"),
            )?;
        }
    }
    Ok(())
}

/// Assemble a relocatable QEMU dependency closure using native binary metadata.
/// Linux carries its own ELF loader; Darwin preserves only OS-owned /usr/lib and
/// /System dependencies. Executable/dependency lookup needs no Nix library paths.
/// The generated launcher preserves inherited fd0/fd1 as bidirectional QMP/GDB
/// sockets: it neither reads nor writes either transport before exec. Its root
/// calculation captures stdout privately; diagnostics use only stderr.
// TODO: closure discovery follows only the QEMU binary's linked libraries, so
// runtime-loadable QEMU modules (dlopen'd from its module directory) would be
// silently missing from the relocated closure. Before enabling modules in the
// tcgQemu build, extend discovery to copy them and their dependencies.
pub(super) fn host(root: &Path, output: &Path) -> Result<()> {
    let input = nix_output(root, "tcgQemu")?;
    let system = format!(
        "{}-{}",
        std::env::consts::ARCH,
        if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        }
    );
    let destination = output.join("host").join(system);
    let qemu = input.join("bin/qemu-system-x86_64");
    if cfg!(target_os = "linux") {
        linux_closure(&qemu, &destination)?;
    } else if cfg!(target_os = "macos") {
        darwin_closure(&qemu, &destination)?;
    } else {
        return Err("unsupported QEMU host".into());
    }
    // QEMU's owned data lookup is independent of its compiled-in store prefix.
    // The microvm backend supplies its own firmware and needs no option ROMs.
    fs::create_dir_all(destination.join("share/qemu"))?;
    copy(
        &input.join("share/qemu/qboot.rom"),
        &destination.join("share/qemu/qboot.rom"),
    )?;
    Ok(())
}

fn tool(name: &str, args: &[&std::ffi::OsStr]) -> Result<String> {
    capture(Command::new(name).args(args))
}
fn print_elf(flag: &str, file: &Path) -> Result<String> {
    tool("patchelf", &[flag.as_ref(), file.as_os_str()])
}
/// Retain `source` once in the flat `lib/` directory and return its retained
/// name; identical bytes under one name are shared. A different library whose
/// name is taken is an error unless `qualify` allows `<package>-<name>`, where
/// `<package>` is the directory containing its `lib/`. Mach-O references and
/// identities are rewritten to the retained name, so Darwin can keep both.
/// ELF loaders also deduplicate by SONAME, so Linux rejects such conflicts.
fn add_library(
    source: PathBuf,
    destination: &Path,
    seen: &mut BTreeMap<String, PathBuf>,
    pending: &mut VecDeque<(PathBuf, String)>,
    qualify: bool,
) -> Result<String> {
    let name = source
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("non-UTF-8 runtime library")?
        .to_owned();
    let mut candidates = vec![name.clone()];
    if qualify {
        let package = source
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .ok_or("runtime library package name")?;
        // Nix store entries are `<32-character hash>-<name>`.
        let package = match package.split_once('-') {
            Some((hash, rest)) if hash.len() == 32 => rest,
            _ => package,
        };
        candidates.push(format!("{package}-{name}"));
    }
    for candidate in candidates {
        match seen.get(&candidate) {
            Some(previous) if fs::read(previous)? == fs::read(&source)? => return Ok(candidate),
            Some(_) => {}
            None => {
                copy(&source, &destination.join("lib").join(&candidate))?;
                seen.insert(candidate.clone(), source.clone());
                pending.push_back((source, candidate.clone()));
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "conflicting runtime library {name}: {} and {}",
        seen[&name].display(),
        source.display()
    )
    .into())
}
fn linux_closure(qemu: &Path, destination: &Path) -> Result<()> {
    let real = destination.join("libexec/qemu-system-x86_64");
    copy(qemu, &real)?;
    let loader = PathBuf::from(print_elf("--print-interpreter", qemu)?);
    // Nix's libc/libgcc may use the interpreter's default library directory
    // without their own RUNPATH. Resolve that authority explicitly from the
    // executable, never from the packaging host's loader cache or PATH.
    let executable_search = format!(
        "{}:{}",
        print_elf("--print-rpath", qemu)?,
        loader.parent().ok_or("loader parent")?.display()
    );
    let mut seen = BTreeMap::new();
    let mut pending = VecDeque::from([(qemu.to_owned(), String::new())]);
    let loader_name = add_library(loader.clone(), destination, &mut seen, &mut pending, false)?;
    while let Some((source, packaged)) = pending.pop_front() {
        let target = if source == qemu {
            real.clone()
        } else {
            destination.join("lib").join(&packaged)
        };
        let search = print_elf("--print-rpath", &source)?;
        let needed = print_elf("--print-needed", &source)?;
        if source == loader {
            // The ELF interpreter relocates itself before ordinary dynamic
            // linking. Rewriting its layout with patchelf can break startup;
            // retain this self-contained object byte-for-byte instead.
            if !search.is_empty() || !needed.is_empty() {
                return Err(format!("ELF interpreter {} has external library authority; cannot package it as a self-contained loader", source.display()).into());
            }
            continue;
        }
        for name in needed.lines() {
            let dependency = resolve_elf(&source, &search, name)
                .or_else(|_| resolve_elf(qemu, &executable_search, name))?;
            let retained = add_library(dependency, destination, &mut seen, &mut pending, false)?;
            if retained != name {
                run(Command::new("patchelf")
                    .args(["--replace-needed", name, &retained])
                    .arg(&target))?;
            }
        }
        // The explicit loader invocation is independent of PT_INTERP. Remove
        // its stale store authority too, so inspection cannot mistake it for a
        // deployable executable independent of the adjacent launcher.
        if source == qemu {
            run(Command::new("patchelf")
                .args([
                    "--set-interpreter",
                    "/seedbox-runtime-use-adjacent-launcher",
                ])
                .arg(&target))?;
        }
        run(Command::new("patchelf")
            .args(["--set-rpath", "$ORIGIN"])
            .arg(&target))?;
    }
    // Leave ORIGIN literal for the loader: path-list splitting must precede
    // directory-token expansion, otherwise a ':' in caller scratch is a separator.
    let script = format!("#!/bin/sh\nset -eu\nroot=$(CDPATH= cd -- \"${{0%/*}}/..\" && pwd)\nexec \"$root/lib/{loader_name}\" --library-path '$ORIGIN/../lib' \"$root/libexec/qemu-system-x86_64\" -L \"$root/share/qemu\" \"$@\"\n");
    let launcher = destination.join("bin/qemu-system-x86_64");
    fs::create_dir_all(launcher.parent().ok_or("launcher parent")?)?;
    let _writing = seedbox_exec::writing();
    fs::write(&launcher, script)?;
    fs::set_permissions(launcher, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
fn resolve_elf(source: &Path, search: &str, name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() && path.is_file() {
        return Ok(path.into());
    }
    let origin = source
        .parent()
        .ok_or("ELF parent")?
        .to_str()
        .ok_or("non-UTF-8 ELF path")?;
    for root in search.split(':').filter(|root| !root.is_empty()) {
        let path =
            PathBuf::from(root.replace("${ORIGIN}", origin).replace("$ORIGIN", origin)).join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    // Nix's glibc DSOs refer to their sibling loader without a RUNPATH.
    let sibling = source.parent().ok_or("ELF parent")?.join(name);
    if sibling.is_file() {
        return Ok(sibling);
    }
    Err(format!("{}: cannot resolve runtime dependency {name} in {search:?}; package must not use host library search", source.display()).into())
}

fn darwin_dependencies(path: &Path) -> Result<Vec<String>> {
    let output = tool("/usr/bin/otool", &["-L".as_ref(), path.as_os_str()])?;
    Ok(output
        .lines()
        .skip(1)
        .filter_map(|line| line.trim().split(" (compatibility version").next())
        .map(str::to_owned)
        .collect())
}
fn darwin_closure(qemu: &Path, destination: &Path) -> Result<()> {
    let real = destination.join("libexec/qemu-system-x86_64");
    copy(qemu, &real)?;
    let executable_commands = tool("/usr/bin/otool", &["-l".as_ref(), qemu.as_os_str()])?;
    let mut pending = VecDeque::from([(qemu.to_owned(), String::new())]);
    let mut seen = BTreeMap::new();
    while let Some((source, packaged)) = pending.pop_front() {
        let target = if source == qemu {
            real.clone()
        } else {
            destination.join("lib").join(&packaged)
        };
        // The second pass resolves loader-relative references emitted by Nix's
        // Darwin fixups; all other unresolved/non-system dependencies fail.
        let load_commands = tool("/usr/bin/otool", &["-l".as_ref(), source.as_os_str()])?;
        // otool -L lists LC_ID_DYLIB first for a dylib; it is identity, not
        // a dependency to resolve through an executable's inherited LC_RPATH.
        for name in darwin_dependencies(&source)?
            .into_iter()
            .skip(usize::from(source != qemu))
        {
            if name.starts_with("/usr/lib/") || name.starts_with("/System/") {
                continue;
            }
            let dependency =
                resolve_macho(&source, qemu, &load_commands, &name).or_else(|error| {
                    if name.starts_with("@rpath/") {
                        resolve_macho(qemu, qemu, &executable_commands, &name)
                    } else {
                        Err(error)
                    }
                })?;
            let retained = add_library(dependency, destination, &mut seen, &mut pending, true)?;
            let replacement = format!(
                "@loader_path/{}{retained}",
                if source == qemu { "../lib/" } else { "" }
            );
            run(Command::new("/usr/bin/install_name_tool")
                .args(["-change", &name, &replacement])
                .arg(&target))?;
        }
        if source != qemu {
            run(Command::new("/usr/bin/install_name_tool")
                .arg("-id")
                .arg(format!("@rpath/{packaged}"))
                .arg(&target))?;
        }
        for rpath in macho_rpaths(&load_commands) {
            run(Command::new("/usr/bin/install_name_tool")
                .args(["-delete_rpath", rpath])
                .arg(&target))?;
        }
        // Editing Mach-O invalidates its original signature. Re-sign every
        // modified object; QEMU uses software emulation, not HVF entitlements.
        run(Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&target))?;
    }
    let launcher = destination.join("bin/qemu-system-x86_64");
    fs::create_dir_all(launcher.parent().ok_or("launcher parent")?)?;
    let _writing = seedbox_exec::writing();
    fs::write(&launcher, "#!/bin/sh\nset -eu\nroot=$(CDPATH= cd -- \"${0%/*}/..\" && pwd)\nexec \"$root/libexec/qemu-system-x86_64\" -L \"$root/share/qemu\" \"$@\"\n")?;
    fs::set_permissions(launcher, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
fn resolve_macho(source: &Path, executable: &Path, commands: &str, name: &str) -> Result<PathBuf> {
    let origin = source.parent().ok_or("Mach-O parent")?;
    let executable_origin = executable.parent().ok_or("Mach-O executable parent")?;
    let expand = |value: &str| {
        if let Some(suffix) = value.strip_prefix("@loader_path/") {
            origin.join(suffix)
        } else if let Some(suffix) = value.strip_prefix("@executable_path/") {
            executable_origin.join(suffix)
        } else {
            PathBuf::from(value)
        }
    };
    let path = expand(name);
    if path.is_absolute() && path.is_file() {
        return Ok(path);
    }
    if let Some(suffix) = name.strip_prefix("@rpath/") {
        for path in macho_rpaths(commands) {
            let candidate = expand(path).join(suffix);
            if candidate.is_absolute() && candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "{}: cannot resolve Mach-O dependency {name}; no ambient fallback",
        source.display()
    )
    .into())
}

fn macho_rpaths(commands: &str) -> Vec<&str> {
    let lines: Vec<_> = commands.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            (line.trim() == "cmd LC_RPATH")
                .then(|| {
                    lines
                        .get(i + 2)
                        .and_then(|line| line.trim().strip_prefix("path "))
                        .and_then(|line| line.split(" (offset").next())
                })
                .flatten()
        })
        .collect()
}

pub(super) fn archive(root: &Path, input: &Path, output: &Path) -> Result<()> {
    // Directory entries are omitted. All links have already been dereferenced
    // while constructing the package, so the runtime reader accepts files only.
    // POSIX pax is supported by GNU tar and BSD tar, including long library
    // names. Suppress Darwin AppleDouble sidecars, not ordinary file bytes.
    let mut files = Vec::new();
    collect(input, input, &mut files)?;
    files.sort();
    let mut list = Vec::new();
    use std::os::unix::ffi::OsStrExt;
    for file in files {
        list.extend_from_slice(file.as_os_str().as_bytes());
        list.push(0);
    }
    let mut command = command("tar", root);
    command
        .args(["--format=pax", "-czf"])
        .env("COPYFILE_DISABLE", "1")
        .arg(output)
        .arg("-C")
        .arg(input)
        .args(["--null", "-T", "-"]);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    let mut child = seedbox_exec::spawn(&mut command)?;
    use std::io::Write;
    let result = child.stdin.take().ok_or("tar stdin")?.write_all(&list);
    let status = child.wait()?;
    result?;
    if !status.success() {
        return Err(format!("runtime tar: {status}").into());
    }
    Ok(())
}
fn collect(root: &Path, directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(root, &entry.path(), output)?;
        } else if kind.is_file() {
            output.push(entry.path().strip_prefix(root)?.to_owned());
        } else {
            return Err(format!(
                "runtime package contains link or special file {}",
                entry.path().display()
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Metadata-only packaging of the real project-provisioned host executable.
    /// This test never starts QEMU or executes any guest instructions.
    #[test]
    #[cfg(target_os = "linux")]
    fn real_qemu_elf_closure_is_relocated_without_executing_it() {
        let qemu = std::env::var_os("SEEDBOX_TCG_QEMU")
            .expect("run host packaging checks inside nix develop");
        let directory = tempfile::tempdir().unwrap();
        let interpreter =
            PathBuf::from(print_elf("--print-interpreter", Path::new(&qemu)).unwrap());
        let original = directory.path().join("original");
        linux_closure(Path::new(&qemu), &original).unwrap();
        let relocated = directory.path().join("relocated");
        fs::rename(original, &relocated).unwrap();
        let real = relocated.join("libexec/qemu-system-x86_64");
        assert_eq!(
            print_elf("--print-interpreter", &real).unwrap(),
            "/seedbox-runtime-use-adjacent-launcher"
        );
        let mut native = vec![real];
        native.extend(
            fs::read_dir(relocated.join("lib"))
                .unwrap()
                .map(|entry| entry.unwrap().path()),
        );
        assert!(native.len() > 2, "must include loader and dependencies");
        eprintln!("metadata-only QEMU closure: {} ELF files (including executable and loader); no QEMU execution", native.len());
        for path in native {
            let rpath = print_elf("--print-rpath", &path).unwrap();
            if path.file_name() == interpreter.file_name() {
                assert_eq!(fs::read(&path).unwrap(), fs::read(&interpreter).unwrap());
                assert!(rpath.is_empty());
            } else {
                assert_eq!(rpath, "$ORIGIN");
            }
            for name in print_elf("--print-needed", &path).unwrap().lines() {
                assert!(!name.contains('/'), "absolute external dependency {name}");
                assert!(relocated.join("lib").join(name).is_file(), "missing {name}");
            }
        }
        let launcher = fs::read_to_string(relocated.join("bin/qemu-system-x86_64")).unwrap();
        assert!(launcher.contains("--library-path '$ORIGIN/../lib'"));
        assert!(!launcher.contains("/nix/store"));
    }

    /// Executes a compiled host probe, never QEMU. The same generated Linux
    /// launcher and bundled ELF loader must preserve the QMP fd0 / GDB fd1
    /// socket transports without consuming or decorating either byte stream.
    #[test]
    #[cfg(target_os = "linux")]
    fn launcher_preserves_inherited_qmp_and_gdb_socket_descriptors() {
        use std::{
            io::{Read, Write},
            os::{fd::OwnedFd, unix::net::UnixStream},
            process::Stdio,
            time::Duration,
        };
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let library = format!("seedbox_descriptor_fixture_{}", std::process::id());
        let dso_source = directory.path().join("owned_library.c");
        fs::write(
            &dso_source,
            "int seedbox_owned_fixture(void) { return 0x1234; }\n",
        )
        .unwrap();
        run(Command::new("cc")
            .args(["-shared", "-fPIC"])
            .arg(&dso_source)
            .arg("-o")
            .arg(directory.path().join(format!("lib{library}.so"))))
        .unwrap();
        let source = directory.path().join("host_descriptor_probe.rs");
        fs::write(&source, r#"
            use std::{fs::File, io::{Read, Write}, os::fd::FromRawFd};
            #[link(name = "FIXTURE_LIBRARY_NAME")]
            extern "C" { fn seedbox_owned_fixture() -> i32; }
            fn main() {
                assert_eq!(unsafe { seedbox_owned_fixture() }, 0x1234);
                let args: Vec<_> = std::env::args().collect();
                assert_eq!(args.len(), 4); assert_eq!(args[1], "-L");
                assert!(args[2].ends_with("/share/qemu")); assert_eq!(args[3], "--host-fixture");
                let mut qmp = unsafe { File::from_raw_fd(0) };
                let mut gdb = unsafe { File::from_raw_fd(1) };
                let mut bytes = [0; 3]; qmp.read_exact(&mut bytes).unwrap(); assert_eq!(&bytes, b"qmp");
                gdb.read_exact(&mut bytes).unwrap(); assert_eq!(&bytes, b"gdb");
                qmp.write_all(b"QMP").unwrap(); gdb.write_all(b"GDB").unwrap();
            }
        "#.replace("FIXTURE_LIBRARY_NAME", &library)).unwrap();
        let probe = directory.path().join("host-descriptor-probe");
        run(Command::new("rustc")
            .args(["--edition=2021"])
            .arg(&source)
            .arg("-L")
            .arg(format!("native={}", directory.path().display()))
            .arg("-C")
            .arg(format!(
                "link-arg=-Wl,-rpath,{}",
                directory.path().display()
            ))
            .arg("-o")
            .arg(&probe))
        .unwrap();
        let package = directory.path().join("closure with spaces:and colon");
        linux_closure(&probe, &package).unwrap();
        // Remove the only source copy, so a stale source RPATH or ambient libc
        // cannot mask failure to load this uniquely owned retained test DSO.
        fs::remove_file(directory.path().join(format!("lib{library}.so"))).unwrap();
        let (mut qmp, qmp_child) = UnixStream::pair().unwrap();
        let (mut gdb, gdb_child) = UnixStream::pair().unwrap();
        for socket in [&qmp, &gdb] {
            let guard = seedbox_test_time::HangGuard::new(
                "host descriptor probe socket exchange",
                Duration::from_secs(30),
            );
            socket
                .set_read_timeout(Some(guard.remaining().unwrap()))
                .unwrap();
            socket
                .set_write_timeout(Some(guard.remaining().unwrap()))
                .unwrap();
        }
        qmp.write_all(b"qmp").unwrap();
        gdb.write_all(b"gdb").unwrap();
        let qmp_fd: OwnedFd = qmp_child.into();
        let gdb_fd: OwnedFd = gdb_child.into();
        let diagnostics = directory.path().join("probe.stderr");
        let mut child = Child(
            seedbox_exec::spawn(
                Command::new(package.join("bin/qemu-system-x86_64"))
                    .env_clear()
                    .arg("--host-fixture")
                    .stdin(Stdio::from(qmp_fd))
                    .stdout(Stdio::from(gdb_fd))
                    .stderr(fs::File::create(&diagnostics).unwrap()),
            )
            .unwrap(),
        );
        let mut bytes = [0; 3];
        qmp.read_exact(&mut bytes).unwrap_or_else(|error| {
            panic!(
                "QMP descriptor: {error}, child {:?}: {}",
                child.0.try_wait().unwrap(),
                fs::read_to_string(&diagnostics).unwrap()
            )
        });
        assert_eq!(&bytes, b"QMP");
        gdb.read_exact(&mut bytes).unwrap_or_else(|error| {
            panic!(
                "GDB descriptor: {error}, child {:?}: {}",
                child.0.try_wait().unwrap(),
                fs::read_to_string(&diagnostics).unwrap()
            )
        });
        assert_eq!(&bytes, b"GDB");
        let guard = seedbox_test_time::HangGuard::new(
            "host descriptor fixture process exit",
            Duration::from_secs(30),
        );
        let status = guard
            .wait_for(|| child.0.try_wait().transpose())
            .expect("host descriptor fixture failed to exit")
            .unwrap();
        assert!(
            status.success(),
            "{}",
            fs::read_to_string(&diagnostics).unwrap()
        );
    }

    #[test]
    fn elf_resolution_uses_own_rpath_and_never_host_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("lib")).unwrap();
        let source = dir.path().join("program");
        fs::write(&source, b"fixture").unwrap();
        let library = dir.path().join("lib/fixture.so");
        fs::write(&library, b"library").unwrap();
        assert_eq!(
            resolve_elf(&source, "$ORIGIN/lib", "fixture.so").unwrap(),
            library
        );
        assert!(resolve_elf(&source, "", "libc.so.6").is_err());
    }
    #[test]
    fn packaged_members_preserve_bytes_and_modes_without_links_or_directories() {
        use std::io::Read;
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        fs::create_dir_all(input.join("tcg/x86_64")).unwrap();
        let executable = input.join("tcg/x86_64/mke2fs");
        fs::write(&executable, b"guest formatter fixture").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(input.join("tcg/x86_64/command-line"), b"").unwrap();
        let output = directory.path().join("runtime.tar.gz");
        archive(directory.path(), &input, &output).unwrap();
        fs::remove_dir_all(&input).unwrap();
        let decompressed = crate::guarded_output(
            Command::new("gzip")
                .arg("-dc")
                .arg(&output)
                .stderr(std::process::Stdio::piped()),
        )
        .unwrap();
        assert!(decompressed.status.success());
        let mut tar = tar::Archive::new(&decompressed.stdout[..]);
        let mut names = Vec::new();
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            assert!(entry.header().entry_type().is_file());
            let name = entry.path().unwrap().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if name.ends_with("mke2fs") {
                assert_eq!(bytes, b"guest formatter fixture");
                assert_eq!(entry.header().mode().unwrap(), 0o755);
            } else {
                assert!(bytes.is_empty());
            }
            names.push(name);
        }
        assert_eq!(
            names,
            vec![
                PathBuf::from("tcg/x86_64/command-line"),
                PathBuf::from("tcg/x86_64/mke2fs")
            ]
        );
    }

    #[test]
    fn package_source_links_and_special_files_are_not_followed() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        fs::create_dir(&input).unwrap();
        fs::write(directory.path().join("outside"), b"not owned").unwrap();
        std::os::unix::fs::symlink("../outside", input.join("link")).unwrap();
        assert!(archive(
            directory.path(),
            &input,
            &directory.path().join("runtime.tar.gz")
        )
        .is_err());
        assert_eq!(
            fs::read(directory.path().join("outside")).unwrap(),
            b"not owned"
        );
    }

    #[test]
    fn closure_preserves_modes_and_rejects_basename_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("one/lib")).unwrap();
        fs::create_dir_all(dir.path().join("two/lib")).unwrap();
        let first = dir.path().join("one/lib/libsame.so");
        let second = dir.path().join("two/lib/libsame.so");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"two").unwrap();
        fs::set_permissions(&first, fs::Permissions::from_mode(0o555)).unwrap();
        let out = dir.path().join("package");
        let mut seen = BTreeMap::new();
        let mut pending = VecDeque::new();
        add_library(first.clone(), &out, &mut seen, &mut pending, false).unwrap();
        add_library(first, &out, &mut seen, &mut pending, false).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            fs::metadata(out.join("lib/libsame.so"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(add_library(second, &out, &mut seen, &mut pending, false).is_err());
    }

    #[test]
    fn darwin_closure_qualifies_basename_conflicts_by_package() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "r7k52b2q44b3k9rdk72q1kcdiizp361x";
        fs::create_dir_all(dir.path().join("apple/lib")).unwrap();
        fs::create_dir_all(dir.path().join(format!("{hash}-libiconv-1.19/lib"))).unwrap();
        let first = dir.path().join("apple/lib/libiconv.2.dylib");
        let second = dir
            .path()
            .join(format!("{hash}-libiconv-1.19/lib/libiconv.2.dylib"));
        fs::write(&first, b"apple").unwrap();
        fs::write(&second, b"gnu").unwrap();
        let out = dir.path().join("package");
        let mut seen = BTreeMap::new();
        let mut pending = VecDeque::new();
        let add = |source: &PathBuf, seen: &mut _, pending: &mut _| {
            add_library(source.clone(), &out, seen, pending, true).unwrap()
        };
        assert_eq!(add(&first, &mut seen, &mut pending), "libiconv.2.dylib");
        let qualified = "libiconv-1.19-libiconv.2.dylib";
        assert_eq!(add(&second, &mut seen, &mut pending), qualified);
        assert_eq!(add(&second, &mut seen, &mut pending), qualified);
        assert_eq!(
            pending
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>(),
            ["libiconv.2.dylib", qualified]
        );
        assert_eq!(fs::read(out.join("lib").join(qualified)).unwrap(), b"gnu");
        // A third distinct library with the same package and name still fails.
        fs::create_dir_all(dir.path().join("other/libiconv-1.19/lib")).unwrap();
        let third = dir.path().join("other/libiconv-1.19/lib/libiconv.2.dylib");
        fs::write(&third, b"other").unwrap();
        assert!(add_library(third, &out, &mut seen, &mut pending, true).is_err());
    }
    #[test]
    fn macho_loader_and_rpath_resolution_is_explicit() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("lib")).unwrap();
        let source = dir.path().join("program");
        let library = dir.path().join("lib/a.dylib");
        fs::write(&library, b"library").unwrap();
        assert_eq!(
            resolve_macho(&source, &source, "", "@loader_path/lib/a.dylib").unwrap(),
            library
        );
        assert_eq!(
            resolve_macho(
                &source,
                &source,
                "    cmd LC_RPATH\ncmdsize 48\npath @loader_path/lib (offset 12)",
                "@rpath/a.dylib"
            )
            .unwrap(),
            library
        );
        assert!(resolve_macho(&source, &source, "", "@rpath/a.dylib").is_err());
    }
}
