#!/usr/bin/env python3
"""Read upstream notices without extracting source archive paths."""
import tarfile

NOTICES = {
    "e2fsprogs": (("NOTICE", "COPYING"),),
    "musl": (("COPYRIGHT", "COPYING", "LICENSE"),),
}


def notices(sources, components=NOTICES):
    sections = []
    for component in components:
        groups = NOTICES[component]
        path = sources / (component + ".source")
        if not path.is_file():
            raise ValueError(f"missing provisioned upstream source: {path}")
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
    return result
