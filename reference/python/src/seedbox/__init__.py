"""Deterministic execution with a controlled Linux ABI subset."""

import hashlib
from pathlib import Path

__version__ = "0.1.0"


def implementation_sha256() -> str:
    """Identify the actual runner source, including uncommitted local changes."""
    digest = hashlib.sha256()
    for source in sorted(Path(__file__).parent.glob("*.py")):
        digest.update(source.name.encode() + b"\0" + source.read_bytes() + b"\0")
    return digest.hexdigest()
