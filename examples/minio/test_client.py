"""Local SDK/checker controls using a fake HTTP transport; no MinIO or sockets."""
import hashlib
import io
from pathlib import Path
import tempfile
import threading
import unittest
from urllib.parse import urlparse
from unittest.mock import patch
import sys

sys.path.insert(0, str(Path(__file__).parent / "client"))
import cas
import boto3
import botocore
from botocore.awsrequest import AWSResponse
from botocore.exceptions import ClientError, ReadTimeoutError

class Raw:
    def __init__(self, body):
        self.body = body
    def stream(self, amt=None, decode_content=False):
        yield self.body
    def read(self, amt=None):
        return self.body
    def close(self):
        pass

def response(request, status, body=b"", headers=None):
    headers = {"content-length": str(len(body)), **(headers or {})}
    if status >= 400:
        body = b"<Error><Code>PreconditionFailed</Code><Message>synthetic</Message></Error>"
        headers["content-length"] = str(len(body))
    return AWSResponse(request.url, status, headers, Raw(body))

class Store:
    def __init__(self):
        self.objects = {}
        self.calls = []
        self.lock = threading.Lock()
        self.missing_match_status = 404
        self.overwrite_on_conflict = False
        self.ignore_conditions = False
        self.first_put_status = None

class Transport:
    def __init__(self, store):
        self.store = store
    def send(self, request):
        path = urlparse(request.url).path
        headers = {key.lower(): value.decode() if isinstance(value, bytes) else value for key, value in request.headers.items()}
        condition = {key: value for key, value in headers.items() if key in ("if-match", "if-none-match")}
        body = request.body or b""
        if hasattr(body, "read"):
            body = body.read()
        with self.store.lock:
            self.store.calls.append((request.method, path, condition))
            old = self.store.objects.get(path)
            if request.method == "GET":
                if old is None:
                    return response(request, 404)
                value, etag = old
                return response(request, 200, value, {"etag": etag})
            if request.method != "PUT":
                raise AssertionError(f"unexpected synthetic request: {request.method}")
            if self.store.first_put_status:
                return response(request, self.store.first_put_status)
            etag = '"opaque-' + hashlib.sha256(body).hexdigest()[:10] + '"'
            if not self.store.ignore_conditions:
                if headers.get("if-none-match") == "*" and old is not None:
                    if self.store.overwrite_on_conflict:
                        self.store.objects[path] = body, etag
                    return response(request, 412)
                if "if-match" in headers:
                    if old is None:
                        return response(request, self.store.missing_match_status)
                    if headers["if-match"] != old[1]:
                        return response(request, 412)
            self.store.objects[path] = body, etag
            return response(request, 200, headers={"etag": etag})

def factory(store):
    def make():
        s3 = cas.client("http://synthetic.invalid:9000", "fixture-access", "fixture-secret-only")
        s3._endpoint.http_session = Transport(store)
        return s3
    return make

class ClientControls(unittest.TestCase):
    def test_readiness_is_bounded_and_put_ambiguity_never_acknowledges(self):
        calls = []
        class Unready:
            def list_buckets(self):
                calls.append(1)
                return {"ResponseMetadata": {"HTTPStatusCode": 503}}
        with patch.object(cas.time, "sleep") as sleep:
            with self.assertRaisesRegex(cas.QualificationError, "readiness budget"):
                cas.ready(Unready())
            self.assertEqual(len(calls), 8)
            self.assertEqual(sleep.call_count, 7)
        make = factory(Store())
        def ambiguous(*args, **kwargs):
            raise ReadTimeoutError(endpoint_url="http://synthetic.invalid")
        with tempfile.TemporaryDirectory() as directory, patch.object(cas, "put", side_effect=ambiguous) as put:
            ledger = Path(directory) / "ledger"
            with self.assertRaises(ReadTimeoutError):
                cas.planted(make(), make, "broken", ledger)
            self.assertEqual(put.call_count, 1)
            self.assertFalse(ledger.exists())

    def test_public_headers_and_current_sdk_pins(self):
        self.assertEqual(boto3.__version__, "1.40.45")
        self.assertEqual(botocore.__version__, "1.40.45")
        store = Store()
        s3 = factory(store)()
        status, first = cas.put(s3, "header", cas.A, IfNoneMatch="*")
        self.assertEqual(status, 200)
        status, _ = cas.put(s3, "header", cas.B, IfMatch=first["ETag"])
        self.assertEqual(status, 200)
        self.assertEqual(store.calls[0][2], {"if-none-match": "*"})
        self.assertEqual(store.calls[1][2], {"if-match": first["ETag"]})
        self.assertEqual(s3.meta.config.connect_timeout, 1)
        self.assertEqual(s3.meta.config.read_timeout, 2)
        self.assertEqual(s3.meta.config.retries["total_max_attempts"], 1)

    def test_http_503_and_read_timeout_are_not_retried(self):
        class Failing:
            def __init__(self, timeout):
                self.calls = 0
                self.timeout = timeout
            def send(self, request):
                self.calls += 1
                if self.timeout:
                    raise ReadTimeoutError(endpoint_url=request.url)
                return response(request, 503)
        for timeout in (False, True):
            with self.subTest(timeout=timeout):
                s3 = cas.client("http://synthetic.invalid", "fixture-access", "fixture-secret-only")
                transport = Failing(timeout)
                s3._endpoint.http_session = transport
                with self.assertRaises(ReadTimeoutError if timeout else ClientError):
                    s3.put_object(Bucket=cas.BUCKET, Key="slot", Body=cas.A, IfNoneMatch="*")
                self.assertEqual(transport.calls, 1)

    def test_fixed_preserves_ack_broken_is_detected(self):
        for mode, expected in (("fixed", 0), ("broken", 1)):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                store = Store()
                make = factory(store)
                ledger = Path(directory) / "ledger"
                self.assertEqual(cas.planted(make(), make, mode, ledger), expected)
                self.assertEqual(ledger.read_bytes(), cas.A)
                writes = [call for call in store.calls if call[0] == "PUT"]
                self.assertEqual(len(writes), 2 if mode == "fixed" else 3)
                if mode == "broken":
                    self.assertEqual(writes[-1][2], {})
                else:
                    self.assertTrue(all(call[2] == {"if-none-match": "*"} for call in writes))

    def test_no_ack_on_first_conflict(self):
        store = Store()
        store.first_put_status = 412
        make = factory(store)
        with tempfile.TemporaryDirectory() as directory:
            ledger = Path(directory) / "ledger"
            with self.assertRaises(cas.QualificationError):
                cas.planted(make(), make, "fixed", ledger)
            self.assertFalse(ledger.exists())

    def test_qualification_checker_and_false_conflict_negative_control(self):
        store = Store()
        make = factory(store)
        self.assertEqual(cas.qualify(make(), make), 0)
        store = Store()
        store.overwrite_on_conflict = True
        make = factory(store)
        with self.assertRaisesRegex(cas.QualificationError, "unexpected stored bytes"):
            cas.qualify(make(), make)

    def test_unqualified_missing_match_semantics_are_refused(self):
        store = Store()
        store.missing_match_status = 412
        make = factory(store)
        with self.assertRaisesRegex(cas.QualificationError, "expected HTTP 404"):
            cas.qualify(make(), make)

    def test_condition_header_omission_is_detectable(self):
        store = Store()
        make = factory(store)
        original_put = cas.put
        def missing_header(s3, key, value, **condition):
            return original_put(s3, key, value)
        with patch.object(cas, "put", missing_header):
            with self.assertRaisesRegex(cas.QualificationError, "expected HTTP 412"):
                cas.qualify(make(), make)

    def test_wrong_concurrent_winner_value_is_detectable(self):
        store = Store()
        make = factory(store)
        original_get = cas.get
        def wrong_get(s3, key):
            value, etag = original_get(s3, key)
            return (b"unacknowledged", etag) if key == "concurrent" else (value, etag)
        with patch.object(cas, "get", wrong_get):
            with self.assertRaisesRegex(cas.QualificationError, "unexpected stored bytes"):
                cas.qualify(make(), make)

    def test_both_constructed_clients_rendezvous_before_transport_dispatch(self):
        for omitted in (False, True):
            with self.subTest(omitted=omitted):
                store = Store()
                constructed = set()
                arrivals = set()
                lock = threading.Lock()
                class TrackedBarrier:
                    def __init__(self, parties, timeout):
                        self.real = threading.Barrier(parties, timeout=timeout)
                    def wait(self):
                        with lock:
                            if threading.get_ident() not in constructed:
                                raise cas.QualificationError("rendezvous before client setup")
                            if omitted:
                                return 0
                            arrivals.add(threading.get_ident())
                        return self.real.wait()
                    def abort(self):
                        self.real.abort()
                class GuardedTransport(Transport):
                    def send(self, request):
                        with lock:
                            if len(constructed) != 2 or arrivals != constructed:
                                raise cas.QualificationError("start rendezvous omitted before PUT")
                        return super().send(request)
                original_make = factory(store)
                def make():
                    s3 = original_make()
                    s3._endpoint.http_session = GuardedTransport(store)
                    with lock:
                        constructed.add(threading.get_ident())
                    return s3
                with patch.object(cas, "Barrier", TrackedBarrier):
                    if omitted:
                        with self.assertRaisesRegex(cas.QualificationError, "rendezvous omitted"):
                            cas.concurrent_creates(make)
                        self.assertEqual(store.calls, [])
                    else:
                        outcomes = cas.concurrent_creates(make)
                        self.assertEqual(sorted(status for status, _ in outcomes), [200, 412])
                        self.assertEqual(len(arrivals), 2)

    def test_sequentialized_executor_fails_bounded_rendezvous_without_puts(self):
        store = Store()
        make = factory(store)
        original_executor = cas.ThreadPoolExecutor
        with patch.object(cas, "ThreadPoolExecutor", lambda max_workers: original_executor(max_workers=1)):
            with self.assertRaisesRegex(cas.QualificationError, "start rendezvous failed"):
                cas.concurrent_creates(make)
        self.assertEqual(store.calls, [])

    def test_concurrent_client_setup_failure_is_qualification_error(self):
        def failed_setup():
            raise ValueError("synthetic client setup failure")
        with self.assertRaisesRegex(cas.QualificationError, "client setup failed"):
            cas.concurrent_creates(failed_setup)

if __name__ == "__main__":
    unittest.main()
