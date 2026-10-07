"""Package an unqualified Nix artifact closure without executing its payload."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import tarfile
import tempfile


REQUIRED = {
    "bin/seedbox", "bin/seedbox-vmm", "libexec/compose-init", "libexec/busybox",
    "share/seedbox/bzImage", "share/seedbox/kernel.config", "provenance.json",
    "nix-registration", "runtime-store-paths",
}


def entries(root, prefix):
    """Visit in byte-order, never following directory symlinks."""
    yield root, prefix
    if root.is_dir() and not root.is_symlink():
        for child in sorted(root.iterdir(), key=lambda path: os.fsencode(path.name)):
            yield from entries(child, prefix + "/" + child.name)


def package(payload, store_paths, output, *, store_root=Path("/nix/store")):
    payload, output = payload.resolve(), output.resolve()
    if output == payload or payload in output.parents:
        raise ValueError("output must be outside the payload")
    if output.exists() and any(output.iterdir()):
        raise ValueError("output must be empty")
    for name in sorted(REQUIRED):
        path = payload / name
        if path.is_symlink() or not path.is_file():
            raise ValueError("missing regular payload file: " + name)
    provenance = json.loads((payload / "provenance.json").read_text())
    if provenance.get("status") != "UNQUALIFIED_DRAFT":
        raise ValueError("payload must explicitly remain UNQUALIFIED_DRAFT")
    revision = provenance.get("sourceRevision", "")
    if len(revision) != 40 or any(c not in "0123456789abcdef" for c in revision):
        raise ValueError("sourceRevision must be a full immutable Git commit")
    paths = sorted(set(store_paths.read_text().splitlines()), key=os.fsencode)
    if not paths:
        raise ValueError("runtime closure must not be empty")
    if paths != sorted(set((payload / "runtime-store-paths").read_text().splitlines()), key=os.fsencode):
        raise ValueError("payload closure list differs from supplied closure")
    roots = [(payload, "payload")]
    for name in paths:
        path = Path(name)
        if path.parent.resolve() != store_root.resolve() or path.is_symlink() or not path.exists():
            raise ValueError("invalid or missing Nix store root: " + name)
        if output == path or path in output.parents:
            raise ValueError("output must be outside the runtime closure")
        roots.append((path, "nix/store/" + path.name))
    # Validate all members before publishing anything. Raw diagnostics/host paths
    # are deliberately outside the artifact; provenance describes pinned inputs.
    members = sorted((entry for root, prefix in roots for entry in entries(root, prefix)), key=lambda entry: os.fsencode(entry[1]))
    checksums = {}
    for path, name in members:
        mode = path.lstat().st_mode
        if stat.S_ISREG(mode):
            digest = hashlib.sha256()
            with path.open("rb") as stream:
                for block in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(block)
            checksums[name] = digest.hexdigest()
        elif not (stat.S_ISDIR(mode) or stat.S_ISLNK(mode)):
            raise ValueError("unsupported archive member: " + name)
    # Symlinks and modes are covered by the archive hash, regular bytes by this
    # manifest as well. Tar never inherits host mtime, uid/gid, PAX or traversal.
    manifest = (json.dumps(checksums, sort_keys=True, indent=2) + "\n").encode()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="seedbox-package-", dir=output.parent) as scratch:
        archive = Path(scratch) / "seedbox-x86_64-linux-nix.tar.gz"
        with archive.open("wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.GNU_FORMAT) as tar:
                for path, name in members:
                    info = tar.gettarinfo(str(path), arcname=name)
                    info.uid = info.gid = info.mtime = 0
                    info.uname = info.gname = ""
                    # Preserve executability and filesystem permissions; discard
                    # inode-based hardlink detection (readable files stay files).
                    if path.is_file() and not path.is_symlink():
                        info.type = tarfile.REGTYPE
                        info.linkname = ""
                        info.size = path.stat().st_size
                        with path.open("rb") as stream:
                            tar.addfile(info, stream)
                    else:
                        tar.addfile(info)
                info = tarfile.TarInfo("SHA256SUMS.json")
                info.mode, info.mtime, info.size = 0o644, 0, len(manifest)
                tar.addfile(info, io.BytesIO(manifest))
        with archive.open("rb") as stream:
            archive_digest = hashlib.file_digest(stream, "sha256").hexdigest()
        (output / "SHA256SUMS.json").write_bytes(manifest)
        (output / "archive.sha256").write_text(archive_digest + "  " + archive.name + "\n")
        archive.replace(output / archive.name)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--payload", required=True, type=Path)
    parser.add_argument("--store-paths", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        package(args.payload, args.store_paths, args.output)
    except (OSError, ValueError) as error:
        parser.exit(2, "error: " + str(error) + "\n")
