"""Copy Cargo configuration with the executable files it references."""
from pathlib import Path
import shutil


def copy_cargo_config(source, root):
    source = Path(source)
    root = Path(root)
    checkout = source.parent.parent
    for relative in ['.cargo/config.toml', 'tools/runtime/linker.py', 'crates/seedbox-arm64/entitlements.plist']:
        destination = root / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(checkout / relative, destination)
