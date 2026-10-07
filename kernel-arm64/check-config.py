#!/usr/bin/env python3
"""Check fragment names against arm64 Kconfig and values after olddefconfig.

Kconfig itself resolves dependencies; this checks that none of our requests
were lost. Disabled symbols may be absent from .config when dependencies are
off, but must still exist in the source (so removed symbols cannot pass).
"""
import argparse
from pathlib import Path
import re


def settings(text):
    values = {}
    for line in text.splitlines():
        match = re.fullmatch(r"(CONFIG_\w+)=(.*)", line)
        disabled = re.fullmatch(r"# (CONFIG_\w+) is not set", line)
        if match:
            key, value = match.groups()
        elif disabled:
            key, value = disabled[1], "n"
        else:
            continue
        if key in values:
            raise ValueError(f"duplicate setting: {key}")
        values[key] = value
    return values


def defined_symbols(source):
    names = set()
    for path in source.rglob("Kconfig*"):
        if not path.is_file():
            continue
        parts = path.relative_to(source).parts
        if parts[0] == "arch" and len(parts) > 2 and parts[1] != "arm64":
            continue
        names.update("CONFIG_" + name for name in re.findall(
            r"^\s*(?:menu)?config\s+(\w+)\s*$", path.read_text(), re.M))
    return names


def errors(requested, actual, defined):
    failures = []
    for key, value in requested.items():
        if key not in defined:
            failures.append(f"{key}: not defined in arm64 Kconfig sources")
        elif actual.get(key, "n") != value:
            failures.append(f"{key}: requested {value}, resolved {actual.get(key, 'n')}")
    return failures


def fixture_errors(actual):
    # The spin fixture must observe an hrtimer expiry through timerfd.
    return [f"{key}: timerfd spin fixture requires y"
            for key in ("CONFIG_HIGH_RES_TIMERS", "CONFIG_TIMERFD")
            if actual.get(key) != "y"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("fragment", type=Path)
    parser.add_argument("config", type=Path)
    args = parser.parse_args()
    requested = settings(args.fragment.read_text())
    actual = settings(args.config.read_text())
    failures = errors(requested, actual, defined_symbols(args.source))
    failures.extend(fixture_errors(actual))
    if failures:
        raise SystemExit("configuration check failed:\n" + "\n".join(failures))
    print(f"verified {len(requested)} arm64 Kconfig symbols and resolved values")


if __name__ == "__main__":
    main()
