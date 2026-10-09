"""Independent DigitalOcean cleanup. Also imported by the Actions controller."""
import datetime
import json
import os
import time
import urllib.error
import urllib.request

MANAGED = "seedbox-ci-pv"
EXPIRY = "seedbox-ci-expires-"
HEARTBEAT = "seedbox-ci-watchdog-v2-"
IMAGE_TAG = "seedbox-ci-image"
JOB_PREFIX = "seedbox-pv-job-"
MAX_LIFETIME = 7200
IMAGE_IMPORT_LIFETIME = 7200


def timestamp(value):
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()


class API:
    def __init__(self, token, base="https://api.digitalocean.com/v2"):
        self.token, self.base = token, base

    def request(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=data, method=method,
                                         headers={"Authorization": "Bearer " + self.token,
                                                  "Content-Type": "application/json"})
        # Never retry creation: a lost response may already have created a resource.
        with urllib.request.urlopen(request, timeout=15) as response:
            raw = response.read()
            return json.loads(raw) if raw else {}

    def list(self, path, key):
        values = []
        separator = "&" if "?" in path else "?"
        for page in range(1, 101):
            response = self.request("GET", f"{path}{separator}per_page=200&page={page}")
            values.extend(response[key])
            if not response.get("links", {}).get("pages", {}).get("next"):
                return values
        raise RuntimeError("API pagination limit exceeded")


def missing(error):
    return isinstance(error, urllib.error.HTTPError) and error.code == 404


def droplets(api):
    # Name is a second discovery path if a tag association is lost. Both names
    # and this tag are reserved exclusively for this controller.
    return [d for d in api.list("/droplets", "droplets")
            if MANAGED in d.get("tags", []) or d.get("name", "").startswith(JOB_PREFIX)]


def images(api):
    return [i for i in api.list("/images?private=true", "images")
            if IMAGE_TAG in i.get("tags", []) or i.get("name", "").startswith("seedbox-pv-")]


def expiration(droplet):
    tags = [tag[len(EXPIRY):] for tag in droplet["tags"] if tag.startswith(EXPIRY)]
    # A malformed/missing expiry cannot make a managed resource immortal.
    if len(tags) != 1 or not tags[0].isdigit():
        return 0
    return min(int(tags[0]), timestamp(droplet["created_at"]) + MAX_LIFETIME)


def delete_resource(api, path):
    try:
        api.request("DELETE", path)
    except urllib.error.HTTPError as error:
        if missing(error):
            error.close()
            return True
        raise
    try:
        api.request("GET", path)
    except urllib.error.HTTPError as error:
        if missing(error):
            error.close()
            return True
        raise
    return False


def delete(api, droplet_id):
    return delete_resource(api, f"/droplets/{droplet_id}")


def image_victims(resources, active, now):
    in_use = {d.get("image", {}).get("id") for d in active}
    available = sorted((i for i in resources if i.get("status") == "available"),
                       key=lambda i: i["created_at"], reverse=True)
    keep = {i["id"] for i in available[:2]} | in_use
    return [i for i in resources if i["id"] not in keep and
            (i.get("status") == "available" or
             timestamp(i["created_at"]) + IMAGE_IMPORT_LIFETIME <= now)]


def heartbeat_age(api, now):
    stamps = [int(tag["name"][len(HEARTBEAT):]) for tag in api.list("/tags", "tags")
              if tag["name"].startswith(HEARTBEAT) and tag["name"][len(HEARTBEAT):].isdigit()]
    return now - max(stamps, default=0)


def sweep(api, now):
    errors, deleted = [], []
    active = droplets(api)
    for droplet in active:
        if expiration(droplet) > now:
            continue
        try:
            if delete(api, droplet["id"]):
                deleted.append(droplet["id"])
            else:
                errors.append(f"deletion pending: {droplet['id']}")
        except (OSError, ValueError) as error:
            errors.append(f"delete {droplet['id']}: {type(error).__name__}")
    # Retention runs here too: an interrupted import must not depend on a later
    # successful image build to stop paying for abandoned storage.
    for resource in image_victims(images(api), active, now):
        try:
            if not delete_resource(api, f"/images/{resource['id']}"):
                errors.append(f"image deletion pending: {resource['id']}")
        except (OSError, ValueError) as error:
            errors.append(f"delete image {resource['id']}: {type(error).__name__}")
    if errors:
        # Preserve expired resources and stale heartbeat so later sweeps retry,
        # and new provisioning fails closed.
        raise RuntimeError("; ".join(errors))
    for tag in api.list("/tags", "tags"):
        name = tag["name"]
        if name.startswith(HEARTBEAT) and name[len(HEARTBEAT):].isdigit():
            if int(name[len(HEARTBEAT):]) < now - 300:
                api.request("DELETE", "/tags/" + name)
        # Reservations survive job/resource deletion. Keep this and the previous
        # two months; never erase current-month budget accounting.
        if name.startswith(("seedbox-ci-reserve-", "seedbox-ci-build-")):
            day = name.split("-")[3]
            try:
                age = now - datetime.datetime.strptime(day, "%Y%m%d").replace(
                    tzinfo=datetime.timezone.utc).timestamp()
            except ValueError:
                continue
            if age > 93 * 86400:
                api.request("DELETE", "/tags/" + name)
    # No fresh heartbeat if any cleanup phase failed.
    api.request("POST", "/tags", {"name": HEARTBEAT + str(now)})
    return {"deleted": deleted, "checked_at": now}


def main(args):
    # Ignore invocation parameters: ownership scope and credentials are deployment-owned.
    result = sweep(API(os.environ["DIGITALOCEAN_TOKEN"]), int(time.time()))
    print(json.dumps(result))
    return {"body": result}
