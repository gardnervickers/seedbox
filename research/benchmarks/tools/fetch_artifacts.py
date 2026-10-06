#!/usr/bin/env python3
"""Fixed public artifact preparation only; no host installation or runtime downloads."""
import argparse, base64, hashlib, json, pathlib, urllib.request

def fetch(url):
    with urllib.request.urlopen(url, timeout=30) as response:
        return response.read()

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--output', type=pathlib.Path, required=True)
    ap.add_argument('--initialize-lock', action='store_true', help='one-time maintainer pin creation, not a build fallback')
    args = ap.parse_args()
    root = pathlib.Path(__file__).resolve().parents[1]
    path = root / 'artifacts.lock.json'
    lock = json.loads(path.read_text())
    args.output.mkdir(parents=True, exist_ok=True)
    for item in lock['artifacts']:
        stem = f"{item['artifact']}-{item['version']}"
        relative = f"{item['group'].replace('.', '/')}/{item['artifact']}/{item['version']}/{stem}"
        url = lock['repository'] + '/' + relative
        jar_path = args.output / (stem + '.jar')
        jar = jar_path.read_bytes() if jar_path.exists() else fetch(url + '.jar')
        digest = hashlib.sha256(jar).hexdigest()
        if args.initialize_lock:
            if item['sha256'] is not None:
                raise ValueError('refuse to replace existing pin; make a reviewed explicit version change')
            # Maven Central release checksum verifies transfer, not publisher identity.
            sha1 = fetch(url + '.jar.sha1').decode().split()[0]
            if hashlib.sha1(jar).hexdigest() != sha1:
                raise ValueError('published release checksum mismatch')
            item.update(sha256=digest, hash='sha256-' + base64.b64encode(bytes.fromhex(digest)).decode(),
                        url=url + '.jar', published_sha1=sha1, bytes=len(jar))
            pom = fetch(url + '.pom')
            item['pom_sha256'] = hashlib.sha256(pom).hexdigest()
            (args.output / (stem + '.pom')).write_bytes(pom)
        elif item['sha256'] is None or digest != item['sha256']:
            raise ValueError('missing/mismatched locked hash: ' + stem)
        jar_path.write_bytes(jar)
    if args.initialize_lock:
        lock['qualification'] = 'release jars verified against published Maven checksums and frozen bySHA256; complete direct compile/runtime graph'
        path.write_text(json.dumps(lock, indent=2) + '\n')
    print(json.dumps({'artifacts_verified':len(lock['artifacts']), 'output':str(args.output.resolve())}))

if __name__ == '__main__': main()
