"""Acknowledged etcd writes on a named volume survive a machine crash and reboot."""
import argparse
import base64
import json
import time
import urllib.request

from seedbox import World


def call(endpoint, path, body, timeout=2):
    request = urllib.request.Request(f"http://{endpoint}{path}", json.dumps(body).encode(),
                                     {"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.load(response)


def encode(text):
    return base64.b64encode(text.encode()).decode()


def ready(endpoint, seconds):
    deadline = time.monotonic() + seconds
    while True:
        try:
            with urllib.request.urlopen(f"http://{endpoint}/health", timeout=1) as response:
                if json.load(response).get("health") in (True, "true"):
                    return
        except OSError:
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError(f"etcd readiness timed out: {endpoint}")
        time.sleep(0.1)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--keys", type=int, default=20)
    args = parser.parse_args()
    ready(args.endpoint, 30)
    # etcd fsyncs its WAL before acknowledging a put, so every acknowledged key
    # must be on the volume's stable sectors when the machine crashes.
    for index in range(args.keys):
        call(args.endpoint, "/v3/kv/put", {"key": encode(f"k/{index:04}"), "value": encode(f"v{index}")})
    with World.connect() as world:
        world.crash("n1")
        world.sleep(0.5)
        world.reboot("n1")
    ready(args.endpoint, 60)
    found = call(args.endpoint, "/v3/kv/range",
                 {"key": encode("k/"), "range_end": encode("k0")}).get("kvs", [])
    values = {base64.b64decode(kv["key"]).decode(): base64.b64decode(kv["value"]).decode() for kv in found}
    expected = {f"k/{index:04}": f"v{index}" for index in range(args.keys)}
    missing = sorted(set(expected) - set(values))
    print(f"RESULT {'pass' if values == expected else 'fail'} acked={args.keys} "
          f"found={len(values)} missing={missing[:5]}", flush=True)
    return 0 if values == expected else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"SCENARIO error {error}", flush=True)
        raise SystemExit(2)
