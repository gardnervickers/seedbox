"""Copy already-acquired, hash-verified inputs; no fetching/building/execution."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parent
PINS = json.loads((ROOT / "inputs.lock.json").read_text())

def verify_input(source, pin):
    if source.stat().st_size != pin["size"]:
        raise ValueError(f"size mismatch: {source}")
    with source.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != pin["sha256"]:
            raise ValueError(f"hash mismatch: {source}")

def stage(root, minio, wheels):
    copies = [(Path(minio), root / "minio/minio", PINS["minio"])]
    copies += [(Path(wheels) / item["filename"], root / "client/wheels" / item["filename"], item)
               for item in PINS["wheels"]]
    for source, destination, pin in copies:
        if destination.exists() or destination.is_symlink():
            raise FileExistsError(f"refusing existing destination: {destination}")
        verify_input(source, pin)
    for source, destination, pin in copies:
        destination.parent.mkdir(parents=True, exist_ok=True)
        with source.open("rb") as stream, destination.open("xb") as output:
            shutil.copyfileobj(stream, output)
        verify_input(destination, pin)

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--minio", required=True, type=Path)
    parser.add_argument("--wheel-directory", required=True, type=Path)
    args = parser.parse_args()
    stage(ROOT, args.minio, args.wheel_directory)
    print("staged verified inputs; no executable was run")
