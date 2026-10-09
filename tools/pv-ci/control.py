#!/usr/bin/env python3
"""GitHub-hosted lifecycle controller. No cloud credentials reach the worker."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import sys
import time
import urllib.error

spec = importlib.util.spec_from_file_location(
    "sweeper", Path(__file__).parent / "watchdog/packages/seedbox/sweep/__main__.py")
sweeper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sweeper)
spec = importlib.util.spec_from_file_location("pv_budget", Path(__file__).parent / "budget.py")
budget = importlib.util.module_from_spec(spec)
spec.loader.exec_module(budget)
CANARY = "seedbox-ci-canary-v2-"


def output(key, value):
    with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
        stream.write(f"{key}={value}\n")


def healthy(api):
    now = int(time.time())
    age = sweeper.heartbeat_age(api, now)
    if not 0 <= age <= 300:
        raise RuntimeError("watchdog heartbeat missing/stale; deploy or repair the scheduled Function")
    if any(sweeper.expiration(d) <= now for d in sweeper.droplets(api)):
        raise RuntimeError("expired droplet still exists; cleanup must complete before provisioning")
    if sweeper.image_victims(sweeper.images(api), sweeper.droplets(api), now):
        raise RuntimeError("image cleanup is overdue; wait for the independent watchdog")


def canary_proven(api):
    stamps = [int(t["name"][len(CANARY):]) for t in api.list("/tags", "tags")
              if t["name"].startswith(CANARY) and t["name"][len(CANARY):].isdigit()]
    if not 0 <= int(time.time()) - max(stamps, default=0) <= 30 * 86400:
        raise RuntimeError("run the cleanup canary first; a heartbeat alone does not prove deletion access")


def attempt():
    return f"{os.environ['GITHUB_RUN_ID']}-{os.environ['GITHUB_RUN_ATTEMPT']}"


def matching_image(api, args):
    images = [i for i in sweeper.images(api)
              if i["name"].startswith("seedbox-pv-" + args.key + "-")
              and i.get("status") == "available" and args.region in i.get("regions", [])]
    return max(images, key=lambda i: i["created_at"]) if images else None


def cleanup(api, tag):
    # Query by creation-time tag, even when creation returned no ID or outputs.
    deadline = time.monotonic() + 180
    while True:
        owned = [d for d in sweeper.droplets(api) if tag in d["tags"] or d.get("name") == tag]
        if not owned:
            return
        for droplet in owned:
            sweeper.delete(api, droplet["id"])
        if time.monotonic() >= deadline:
            raise RuntimeError("droplet deletion not confirmed; independent watchdog will retry")
        time.sleep(5)


def cleanup_runner(github, repo, name):
    # JIT runners deregister after a job, but a boot failure may leave one
    # registered without ever accepting a job.
    for runner in github.list(f"/repos/{repo}/actions/runners", "runners"):
        if runner["name"] == name:
            github.request("DELETE", f"/repos/{repo}/actions/runners/{runner['id']}")


def provision(api, github, args):
    healthy(api)
    canary_proven(api)
    if sweeper.droplets(api):
        raise RuntimeError("another PV droplet exists; only one active batch is allowed")
    image = matching_image(api, args)
    if image is None:
        raise RuntimeError("no matching dependency image; run the PV runner image workflow first")
    budget.check_size(api, args.size, args.region)
    budget.reserve(api, attempt(), int(time.time()), seconds=sweeper.MAX_LIFETIME + 300)
    label = args.tag
    registered = github.request("POST", f"/repos/{args.repo}/actions/runners/generate-jitconfig", {
        "name": label, "runner_group_id": 1,
        "labels": ["self-hosted", "Linux", "X64", label], "work_folder": "_work",
    })
    jit = registered["encoded_jit_config"]
    print("::add-mask::" + jit)
    started = int(time.time())
    try:
        # Identity and expiration are atomic with creation, not a later tagging step.
        api.request("POST", "/droplets", {
            "name": label, "region": args.region, "size": args.size,
            "image": image["id"], "backups": False, "ipv6": False,
            "tags": [sweeper.MANAGED, args.tag, sweeper.EXPIRY + str(started + sweeper.MAX_LIFETIME)],
            "user_data": json.dumps({"jit": jit}),
        })
        deadline = time.monotonic() + 600
        while time.monotonic() < deadline:
            runners = github.list(f"/repos/{args.repo}/actions/runners", "runners")
            if any(r["name"] == label and r["status"] == "online" for r in runners):
                output("label", label)
                output("startup_seconds", int(time.time()) - started)
                print(f"runner online after {int(time.time()) - started}s")
                return
            time.sleep(5)
        raise RuntimeError("runner did not register within ten minutes")
    except BaseException:
        cleanup(api, args.tag)
        raise


def image_slot(api, *, reclaim=False):
    """Never create a third image, including pending imports (serialized workflows)."""
    resources = sweeper.images(api)
    if len(resources) < 2:
        return
    if len(resources) > 2:
        raise RuntimeError("two-image cap exceeded; wait for independent cleanup")
    in_use = {d.get("image", {}).get("id") for d in sweeper.droplets(api)}
    candidates = sorted((i for i in resources if i.get("status") == "available"
                         and i["id"] not in in_use), key=lambda i: i["created_at"])
    if not candidates:
        raise RuntimeError("two-image cap reached; no unused completed image can be replaced")
    if not reclaim:
        return
    path = f"/images/{candidates[0]['id']}"
    deadline = time.monotonic() + 120
    while not sweeper.delete_resource(api, path):
        if time.monotonic() >= deadline:
            raise RuntimeError("old image deletion not confirmed; refusing a third image")
        time.sleep(5)
    # Also require list visibility to agree; an ambiguous result fails closed.
    if len(sweeper.images(api)) >= 2:
        raise RuntimeError("image inventory still full after deletion; refusing a third image")


def import_image(api, args):
    healthy(api)
    name = f"seedbox-pv-{args.key}-{attempt()}"
    if any(i["name"] == name for i in sweeper.images(api)):
        raise RuntimeError("this image import already exists; do not retry an ambiguous creation")
    with open(args.url_file) as stream:
        url = stream.read().strip()
    image_slot(api, reclaim=True)
    # URL contains temporary Spaces credentials; never print it or API bodies.
    response = api.request("POST", "/images", {
        "name": f"seedbox-pv-{args.key}-{attempt()}",
        "url": url, "distribution": "Unknown", "region": args.region,
        "description": "NixOS PV Actions runner with resident development and Cargo dependencies",
        "tags": [sweeper.IMAGE_TAG],
    })
    image_id = response["image"]["id"]
    output("image_id", image_id)
    deadline = time.monotonic() + 1800
    while time.monotonic() < deadline:
        image = api.request("GET", f"/images/{image_id}")["image"]
        if image["status"] == "available":
            # Independent Function owns retention even if this process dies now.
            return
        if image["status"] == "errored":
            raise RuntimeError(f"image import failed: {image_id}")
        time.sleep(10)
    raise RuntimeError(f"image import timed out: {image_id}; inspect and remove it if unused")


def cleanup_canary(api, args):
    healthy(api)
    if sweeper.droplets(api):
        raise RuntimeError("another PV droplet exists; cannot run cleanup canary")
    budget.check_size(api, args.size, args.region)
    budget.reserve(api, attempt(), int(time.time()), seconds=sweeper.MAX_LIFETIME + 300)
    expires = int(time.time()) + 60
    response = api.request("POST", "/droplets", {
        "name": args.tag, "region": args.region, "size": args.size,
        "image": "ubuntu-24-04-x64", "backups": False, "ipv6": False,
        "tags": [sweeper.MANAGED, args.tag, sweeper.EXPIRY + str(expires)],
    })
    identifier = response["droplet"]["id"]
    deadline = time.monotonic() + 480
    observed = False
    while time.monotonic() < deadline:
        time.sleep(10)
        try:
            api.request("GET", f"/droplets/{identifier}")
            observed = True
        except urllib.error.HTTPError as error:
            if not sweeper.missing(error):
                raise
            error.close()
            if not observed or time.time() < expires:
                raise RuntimeError("canary disappeared before its expiry was observed; no deletion proof recorded")
            api.request("POST", "/tags", {"name": CANARY + str(int(time.time()))})
            print("independent watchdog deletion confirmed")
            return
    raise RuntimeError("watchdog did not delete canary within eight minutes; do not enable test runners")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=["provision", "cleanup", "cleanup-runner", "health",
                                             "import-image", "image-plan", "canary"])
    parser.add_argument("--tag")
    parser.add_argument("--key")
    parser.add_argument("--repo", default="gardnervickers/seedbox")
    parser.add_argument("--region", default="nyc3")
    parser.add_argument("--size", default="s-4vcpu-8gb-amd")
    parser.add_argument("--url-file")
    args = parser.parse_args()
    if args.command == "cleanup-runner":
        if not args.tag or not args.tag.startswith("seedbox-pv-job-"):
            parser.error("cleanup-runner needs a seedbox-pv-job- tag")
        github = sweeper.API(os.environ["RUNNER_ADMIN_TOKEN"], "https://api.github.com")
        cleanup_runner(github, args.repo, args.tag)
        return
    api = sweeper.API(os.environ["DIGITALOCEAN_TOKEN"])
    if args.command == "health":
        healthy(api)
    elif args.command == "image-plan":
        if not args.key:
            parser.error("image-plan needs --key")
        healthy(api)
        needed = matching_image(api, args) is None
        if needed:
            image_slot(api)
            budget.reserve(api, attempt(), int(time.time()), image=True)
        output("needed", str(needed).lower())
    elif args.command == "canary":
        if not args.tag or not args.tag.startswith(sweeper.JOB_PREFIX):
            parser.error("canary needs a seedbox-pv-job- tag")
        cleanup_canary(api, args)
    elif args.command == "cleanup":
        if not args.tag or not args.tag.startswith("seedbox-pv-job-"):
            parser.error("cleanup needs a seedbox-pv-job- tag")
        cleanup(api, args.tag)
    elif args.command == "import-image":
        if not args.key or not args.url_file:
            parser.error("import-image needs --key and --url-file")
        import_image(api, args)
    else:
        if not args.key or not args.tag or not args.tag.startswith("seedbox-pv-job-"):
            parser.error("provision needs --key and a seedbox-pv-job- --tag")
        github = sweeper.API(os.environ["RUNNER_ADMIN_TOKEN"], "https://api.github.com")
        provision(api, github, args)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError) as error:
        # HTTPError's default text excludes credentials and response bodies.
        sys.exit(f"pv-ci: {error}")
