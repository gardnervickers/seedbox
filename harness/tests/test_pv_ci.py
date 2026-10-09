"""Cost/lifecycle failure cases without cloud access or resource creation."""
import importlib.util
import datetime
import os
from pathlib import Path
import time
import types
import tempfile
import unittest
from unittest.mock import patch
import urllib.error


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("pv_control", ROOT / "tools/pv-ci/control.py")
control = importlib.util.module_from_spec(spec)
spec.loader.exec_module(control)
sweep = control.sweeper


def droplet(identifier=12, expires=2000, tags=()):
    return {"id": identifier, "name": f"seedbox-pv-job-test-{identifier}",
            "tags": [sweep.MANAGED, sweep.EXPIRY + str(expires), *tags],
            "created_at": "1970-01-01T00:16:40Z"}


class FakeAPI:
    def __init__(self, resources=(), tags=(), deletion_confirmed=True, images=()):
        self.resources, self.tags = list(resources), [{"name": tag} for tag in tags]
        self.images = list(images)
        self.sizes = [{"slug": "size", "available": True, "regions": ["nyc3"], "price_hourly": 0.095}]
        self.calls = []
        self.deletion_confirmed = deletion_confirmed

    def list(self, path, key):
        return {"tags": self.tags, "droplets": self.resources,
                "images": self.images, "sizes": self.sizes, "runners": []}[key]

    def request(self, method, path, body=None):
        self.calls.append((method, path, body))
        if path.startswith(("/droplets/", "/images/")):
            resources = self.images if path.startswith("/images/") else self.resources
            if method == "GET" and self.deletion_confirmed:
                raise urllib.error.HTTPError(path, 404, "not found", {}, None)
            if method == "DELETE" and self.deletion_confirmed:
                resources[:] = [d for d in resources if int(path.rsplit("/", 1)[1]) != d["id"]]
        if path == "/tags" and method == "POST":
            self.tags.append(body)
        if path.startswith("/tags/") and method == "DELETE":
            self.tags[:] = [tag for tag in self.tags if tag["name"] != path[len("/tags/"):]]
        return {}


class LifecycleTests(unittest.TestCase):
    def test_expiration_cannot_exceed_two_hours(self):
        self.assertEqual(sweep.expiration(droplet(expires=99999)), 8200)

    def test_missing_duplicate_or_invalid_expiration_is_expired(self):
        for tags in ([sweep.MANAGED], [sweep.EXPIRY + "bad"],
                     [sweep.EXPIRY + "2000", sweep.EXPIRY + "3000"]):
            resource = droplet()
            resource["tags"] = tags
            self.assertEqual(sweep.expiration(resource), 0)

    def test_sweep_deletes_only_expired_resources(self):
        api = FakeAPI([droplet(), droplet(13, expires=4000)])
        result = sweep.sweep(api, 3000)
        self.assertEqual(result["deleted"], [12])
        self.assertEqual([d["id"] for d in api.resources], [13])
        self.assertIn(("POST", "/tags", {"name": sweep.HEARTBEAT + "3000"}), api.calls)

    def test_pending_deletion_preserves_stale_heartbeat_for_retry(self):
        api = FakeAPI([droplet()], deletion_confirmed=False)
        with self.assertRaisesRegex(RuntimeError, "deletion pending"):
            sweep.sweep(api, 3000)
        self.assertFalse(any(method == "POST" for method, _, _ in api.calls))
        self.assertEqual(len(api.resources), 1)

    def test_auth_error_is_not_treated_as_confirmed_deletion(self):
        api = FakeAPI()
        def forbidden(*args):
            error = urllib.error.HTTPError("/droplets/12", 403, "forbidden", {}, None)
            error.close()
            raise error
        api.request = forbidden
        with self.assertRaises(urllib.error.HTTPError):
            sweep.delete(api, 12)

    def test_cleanup_uses_atomic_creation_tag_without_droplet_id(self):
        tag = "seedbox-pv-job-123-1"
        api = FakeAPI([droplet(tags=[tag]), droplet(13, tags=["other-job"])])
        with patch.object(control.time, "sleep"):
            control.cleanup(api, tag)
        self.assertEqual([d["id"] for d in api.resources], [13])

    def test_missing_stale_or_future_heartbeat_blocks_creation(self):
        for stamp in (None, 1, 10000):
            tags = [] if stamp is None else [sweep.HEARTBEAT + str(stamp)]
            api = FakeAPI(tags=tags)
            with patch.object(control.time, "time", return_value=5000):
                with self.assertRaisesRegex(RuntimeError, "heartbeat"):
                    control.healthy(api)
            self.assertFalse(api.calls)

    def test_overdue_cleanup_blocks_creation_even_with_fresh_heartbeat(self):
        api = FakeAPI([droplet()], tags=[sweep.HEARTBEAT + "3000"])
        with patch.object(control.time, "time", return_value=3001):
            with self.assertRaisesRegex(RuntimeError, "expired droplet"):
                control.healthy(api)

    def test_failed_provisioning_cleans_up_even_when_create_response_lost(self):
        tag = "seedbox-pv-job-123-1"
        api = FakeAPI(tags=[sweep.HEARTBEAT + str(int(time.time())),
                            control.CANARY + str(int(time.time()))])
        image = {"id": 8, "name": "seedbox-pv-key-1", "status": "available",
                 "regions": ["nyc3"], "created_at": "2026-10-09T00:00:00Z"}
        original_list = api.list
        api.list = lambda path, key: [image] if key == "images" else original_list(path, key)
        original_request = api.request

        def lost_response(method, path, body=None):
            if path == "/droplets" and method == "POST":
                self.assertIn(tag, body["tags"])
                self.assertTrue(any(t.startswith(sweep.EXPIRY) for t in body["tags"]))
                self.assertNotIn("RUNNER_ADMIN_TOKEN", body["user_data"])
                api.resources.append(droplet(tags=[tag]))
                raise TimeoutError("lost creation response")
            return original_request(method, path, body)

        api.request = lost_response
        github = FakeAPI()
        github.request = lambda *args: {"encoded_jit_config": "single-use-config"}
        args = types.SimpleNamespace(key="key", region="nyc3", tag=tag, repo="owner/repo", size="size")
        with patch.object(control.time, "sleep"), patch.dict(os.environ, {
                "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1"}):
            with self.assertRaises(TimeoutError):
                control.provision(api, github, args)
        self.assertEqual(api.resources, [])

    def test_missing_ownership_tag_is_still_discovered_by_reserved_name(self):
        owned = droplet()
        owned["tags"] = []
        unrelated = {"id": 99, "name": "production", "tags": [], "created_at": owned["created_at"]}
        api = FakeAPI([owned, unrelated])
        sweep.sweep(api, 3000)
        self.assertEqual([d["id"] for d in api.resources], [99])

    def test_fresh_heartbeat_without_successful_canary_is_insufficient(self):
        api = FakeAPI(tags=[sweep.HEARTBEAT + str(int(time.time()))])
        with self.assertRaisesRegex(RuntimeError, "canary"):
            control.canary_proven(api)

    def test_abandoned_and_excess_images_are_cleaned_without_build_completion(self):
        def image(i, status, created):
            return {"id": i, "status": status, "name": "seedbox-pv-test",
                    "created_at": created, "tags": [sweep.IMAGE_TAG]}
        images = [image(1, "available", "1970-01-01T00:00:00Z"),
                  image(2, "available", "1970-01-01T00:01:00Z"),
                  image(3, "available", "1970-01-01T00:02:00Z"),
                  image(4, "errored", "1970-01-01T00:00:00Z"),
                  image(5, "pending", "1970-01-01T02:30:00Z")]
        api = FakeAPI(images=images)
        sweep.sweep(api, 10000)
        self.assertEqual([i["id"] for i in api.images], [2, 3, 5])

    def test_image_used_by_active_droplet_is_not_pruned(self):
        images = [{"id": i, "status": "available", "created_at": str(i)} for i in range(3)]
        self.assertEqual(sweep.image_victims(images, [{"image": {"id": 0}}], 10000), [])

    def test_canary_requires_observed_existence_then_absence_after_expiry(self):
        for early_absence in (True, False):
            now = [1000]
            api = FakeAPI(tags=[sweep.HEARTBEAT + "1000"])
            original = api.request
            def request(method, path, body=None):
                if path == "/droplets":
                    return {"droplet": {"id": 12}}
                if path == "/droplets/12":
                    if early_absence or now[0] >= 1060:
                        raise urllib.error.HTTPError(path, 404, "gone", {}, None)
                    return {"droplet": {"id": 12}}
                return original(method, path, body)
            api.request = request
            args = types.SimpleNamespace(tag="seedbox-pv-job-1-1", region="nyc3", size="size")
            with patch.object(control.time, "time", side_effect=lambda: now[0]), \
                    patch.object(control.time, "monotonic", side_effect=lambda: now[0]), \
                    patch.object(control.time, "sleep", side_effect=lambda _: now.__setitem__(0, now[0] + 30)), \
                    patch.dict(os.environ, {"GITHUB_RUN_ID": "1", "GITHUB_RUN_ATTEMPT": "1"}):
                if early_absence:
                    with self.assertRaisesRegex(RuntimeError, "no deletion proof"):
                        control.cleanup_canary(api, args)
                else:
                    control.cleanup_canary(api, args)
            self.assertEqual(any(t["name"].startswith(control.CANARY) for t in api.tags), not early_absence)


class BudgetTests(unittest.TestCase):
    NOW = datetime.datetime(2026, 10, 9, tzinfo=datetime.timezone.utc).timestamp()

    def test_repeated_failed_launches_eventually_exhaust_budget(self):
        api = FakeAPI()
        for index in range(95):
            control.budget.reserve(api, f"{index}-1", self.NOW)
        with self.assertRaisesRegex(RuntimeError, "monthly"):
            control.budget.reserve(api, "96-1", self.NOW)
        self.assertEqual(len(api.tags), 95)

    def test_expensive_unknown_or_invalid_rate_is_rejected(self):
        for rate in ("20", "NaN", "Infinity", "-1", "0"):
            api = FakeAPI()
            api.sizes[0]["price_hourly"] = rate
            with self.assertRaises(RuntimeError):
                control.budget.check_size(api, "size", "nyc3")
            self.assertEqual(api.calls, [])

    def test_attempt_cannot_create_twice(self):
        api = FakeAPI()
        control.budget.reserve(api, "12-1", self.NOW)
        with self.assertRaisesRegex(RuntimeError, "already reserved"):
            control.budget.reserve(api, "12-1", self.NOW)

    def test_month_boundary_books_both_months(self):
        now = datetime.datetime(2026, 10, 31, 23, 59, tzinfo=datetime.timezone.utc).timestamp()
        api = FakeAPI()
        control.budget.reserve(api, "12-1", now)
        self.assertEqual({t["name"].split("-")[3][:6] for t in api.tags}, {"202610", "202611"})

    def test_image_build_rate_and_monthly_limits(self):
        api = FakeAPI()
        for day in range(4):
            for attempt in range(2):
                control.budget.reserve(api, f"{day}-{attempt}", self.NOW + day * 86400, image=True)
        with self.assertRaisesRegex(RuntimeError, "daily"):
            control.budget.reserve(api, "100-1", self.NOW, image=True)
        with self.assertRaisesRegex(RuntimeError, "monthly"):
            control.budget.reserve(api, "100-1", self.NOW + 5 * 86400, image=True)

    def test_sweeper_preserves_budget_after_droplet_deletion(self):
        api = FakeAPI([droplet()])
        control.budget.reserve(api, "12-1", self.NOW)
        sweep.sweep(api, int(self.NOW))
        self.assertTrue(any(t["name"].startswith(control.budget.RESERVE) for t in api.tags))


class StorageTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("pv_storage", ROOT / "tools/pv-ci/storage.py")
        cls.storage = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.storage)

    def policy(self):
        return {"Rules": [{"Status": "Enabled", "Filter": {"Prefix": "pv-images/"},
                            "Expiration": {"Days": 1},
                            "AbortIncompleteMultipartUpload": {"DaysAfterInitiation": 1}}]}

    def test_requires_both_expiration_and_multipart_abort(self):
        self.storage.validate(self.policy(), {})
        policy = self.policy()
        del policy["Rules"][0]["AbortIncompleteMultipartUpload"]
        with self.assertRaisesRegex(RuntimeError, "multipart"):
            self.storage.validate(policy, {})

    def test_conditional_lifecycle_or_versioned_bucket_rejected(self):
        for state in ("Enabled", "Suspended"):
            with self.assertRaisesRegex(RuntimeError, "versioning"):
                self.storage.validate(self.policy(), {"Status": state})
        policy = self.policy()
        policy["Rules"][0]["Filter"]["Tag"] = {"Key": "optional", "Value": "yes"}
        with self.assertRaises(RuntimeError):
            self.storage.validate(policy, {})

    def test_diagnostics_are_bounded_and_do_not_follow_symlinks(self):
        spec = importlib.util.spec_from_file_location("pv_artifacts", ROOT / "tools/pv-ci/artifacts.py")
        artifacts = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(artifacts)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            reports = root / "target/check-reports"
            reports.mkdir(parents=True)
            for index in range(10):
                with (reports / f"large-{index}.log").open("wb") as stream:
                    stream.truncate(10 * 1024 * 1024)
            (reports / "secret.log").symlink_to("/etc/passwd")
            (reports / "disk.img").write_bytes(b"not diagnostics")
            artifacts.collect(root)
            files = [p for p in (root / "target/pv-artifacts").rglob("*") if p.is_file()]
            self.assertLessEqual(sum(p.stat().st_size for p in files), artifacts.LIMIT)
            self.assertFalse(any(p.name in ("secret.log", "disk.img") for p in files))


if __name__ == "__main__":
    unittest.main()
