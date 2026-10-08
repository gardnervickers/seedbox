#!/usr/bin/env python3
"""Package fixed Linux guest inputs. No binaries or source builds are executed."""
import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import tarfile

ASSETS = {
    "bzImage": ("kernel", None),
    "compose-init": ("supervisor", "x86_64"),
    "busybox": ("busybox", "x86_64"),
    "mke2fs": ("filesystem_formatter", "x86_64"),
}
RECIPES = (
    "flake.lock", "flake.nix", "guest/compose-init.c", "guest/control.h",
    "guest/barrier.h", "guest/io.h", "kernel-arm64/supervisor-io.h",
    "kernel/apply.py", "kernel/config.options", "kernel/build.sh",
    "guest/build-compose.sh", "tools/linux-runtime/package.py",
    "tools/linux-runtime/build-cli.sh",
)
# Copy actual upstream texts from the provisioned source archives. Required
# candidates are fixed; absent source or notices fail the maintainer build.
NOTICES = {
    "linux": (("COPYING",), ("LICENSES/preferred/GPL-2.0",)),
    "busybox": (("LICENSE", "COPYING"),),
    "e2fsprogs": (("NOTICE", "COPYING"),),
    "musl": (("COPYRIGHT", "COPYING", "LICENSE"),),
}


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def notices(sources, components=NOTICES):
    sections = []
    hashes = {}
    for component in components:
        groups = NOTICES[component]
        path = sources / (component + ".source")
        if not path.is_file():
            raise ValueError(f"missing provisioned upstream source: {path}")
        hashes[component] = {"sha256": digest(path)}
        with tarfile.open(path, "r:*") as archive:
            # Upstream archives have a single top-level source directory. Do
            # not extract trees or follow archived links to discover licenses.
            roots = {entry.name.split("/", 1)[0] for entry in archive.getmembers() if entry.name}
            if len(roots) != 1:
                raise ValueError(f"{component}: source archive needs one top-level directory")
            prefix = next(iter(roots))
            for choices in groups:
                member = None
                for candidate in choices:
                    try:
                        member = archive.getmember(prefix + "/" + candidate)
                    except KeyError:
                        continue
                    break
                if member is None or not member.isfile() or not 0 < member.size <= 2 << 20:
                    raise ValueError(f"{component}: missing or invalid actual upstream notice {choices}")
                with archive.extractfile(member) as source:
                    content = source.read((2 << 20) + 1)
                if len(content) != member.size or len(content) > 2 << 20 or not content.decode("utf-8").strip():
                    raise ValueError(f"{component}: invalid notice text")
                sections.append((f"Source component: {component}; upstream file: {member.name.split('/', 1)[1]}\n\n").encode() + content + b"\n")
    result = b"\n".join(sections)
    if len(result) > 8 << 20:
        raise ValueError("combined actual upstream notices exceed 8 MiB")
    return result, hashes


def validate_guest(name, content):
    import struct
    def number(offset, code):
        try:
            return struct.unpack_from("<" + code, content, offset)[0]
        except struct.error as error:
            raise ValueError(f"{name}: truncated executable header") from error
    if name == "bzImage":
        if len(content) < 0x290 or content[0x1fe:0x200] != b"\x55\xaa" or content[0x202:0x206] != b"HdrS":
            raise ValueError("bzImage: expected x86 boot header")
        setup = (content[0x1f1] + 1) * 512
        if (not 1 <= content[0x1f1] <= 64 or setup >= len(content)
                or number(0x206, "H") < 0x020c or not content[0x211] & 1
                or not number(0x236, "H") & 1 or not 0 < number(0x1f4, "I") * 16 <= len(content) - setup):
            raise ValueError("bzImage: invalid x86_64 header or truncated protected-mode payload")
        return
    if (content[:7] != b"\x7fELF\x02\x01\x01" or number(16, "H") not in (2, 3)
            or number(18, "H") != 62 or number(20, "I") != 1
            or number(52, "H") != 64 or number(54, "H") != 56):
        raise ValueError(f"{name}: expected static x86_64 ELF64 executable")
    offset, count = number(32, "Q"), number(56, "H")
    if offset < 64 or not 1 <= count <= 1024 or offset + count * 56 > len(content):
        raise ValueError(f"{name}: program header table outside ELF")
    # Package-time admission is deliberately small. The Rust installer remains
    # authoritative for load spans, entrypoints and dynamic DT_NEEDED state.
    for index in range(count):
        at = offset + index * 56
        kind, start, size = number(at, "I"), number(at + 8, "Q"), number(at + 32, "Q")
        if start + size > len(content):
            raise ValueError(f"{name}: segment outside ELF")
        if kind == 3:
            raise ValueError(f"{name}: PT_INTERP is unsupported")



def canonical(value):
    return (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()


def package(assets, sources, output, revision, dirty):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("source revision must be a full lowercase Git SHA")
    root = Path(__file__).resolve().parents[2]
    contents = {}
    for name in ASSETS:
        path = assets / name
        if not path.is_file() or path.is_symlink() or not 0 < path.stat().st_size <= 128 << 20:
            raise ValueError(f"missing, linked or oversized guest asset: {path}")
        contents[name] = path.read_bytes()
        validate_guest(name, contents[name])
    notice, source_hashes = notices(sources)
    contents["NOTICE.txt"] = notice
    recipe_hashes = {name: digest(root / name) for name in RECIPES}
    provenance = {
        "format": 1, "source_revision": revision, "source_dirty": dirty,
        "flake_lock_sha256": recipe_hashes["flake.lock"],
        "notice_sha256": hashlib.sha256(notice).hexdigest(),
        "recipes": recipe_hashes, "sources": source_hashes,
        "artifacts": {name: hashlib.sha256(contents[name]).hexdigest() for name in ASSETS},
    }
    contents["provenance.json"] = canonical(provenance)
    records = {}
    for name, content in contents.items():
        role, arch = ASSETS.get(name, ("notice" if name == "NOTICE.txt" else "provenance", None))
        records[name] = {"role": role, "execution_arch": arch, "size": len(content),
                         "sha256": hashlib.sha256(content).hexdigest()}
    contents["assets.json"] = canonical({"format": 1, "host_platform": "x86_64-linux",
                                         "guest_arch": "x86_64", "files": records})
    if len(contents["assets.json"]) > 64 << 10 or len(contents["provenance.json"]) > 1 << 20:
        raise ValueError("package metadata exceeds role limit")
    if sum(len(content) + 1024 for content in contents.values()) > 512 << 20:
        raise ValueError("package exceeds decompressed limit")
    output.parent.mkdir(parents=True, exist_ok=True)
    # A temporary sibling prevents publishing incomplete compressed bytes.
    import tempfile
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=output.parent, delete=False) as destination:
            temporary = Path(destination.name)
            with gzip.GzipFile(fileobj=destination, mode="wb", filename="", mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                    for name in sorted(contents):
                        content = contents[name]
                        header = tarfile.TarInfo(name)
                        header.size = len(content)
                        header.mode = 0o755 if name in ASSETS and name != "bzImage" else 0o600
                        header.mtime = header.uid = header.gid = 0
                        header.uname = header.gname = ""
                        archive.addfile(header, io.BytesIO(content))
        if temporary.stat().st_size > 256 << 20:
            raise ValueError("package exceeds compressed limit")
        temporary.replace(output)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets", required=True, type=Path)
    parser.add_argument("--sources", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--dirty", required=True, choices=("0", "1"))
    arguments = parser.parse_args()
    try:
        package(arguments.assets, arguments.sources, arguments.output, arguments.revision, arguments.dirty == "1")
    except (ValueError, OSError, tarfile.TarError, UnicodeError) as error:
        parser.exit(2, f"package: {error}\n")


if __name__ == "__main__":
    main()
