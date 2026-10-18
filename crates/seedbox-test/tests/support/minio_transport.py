"""Synthetic HTTP/clock models for Rust-owned tests of the Python MinIO client.

No test cases, assertions, discovery or expectations live here. The models retain
opaque ETags, conditional request semantics and SDK-level AWSResponse objects.
"""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import threading
from urllib.parse import urlparse
from botocore.awsrequest import AWSResponse
import cas


class Raw:
    def __init__(self, body):
        self.body = body

    def stream(self, amt=None, decode_content=False):
        yield self.body

    def read(self, amt=None):
        return self.body

    def close(self):
        pass


def response(request, status, body=b'', headers=None, error_code='PreconditionFailed'):
    headers = {'content-length': str(len(body)), **(headers or {})}
    if status >= 400:
        body = f'<Error><Code>{error_code}</Code><Message>synthetic</Message></Error>'.encode()
        headers['content-length'] = str(len(body))
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
        headers = {key.lower(): value.decode() if isinstance(value, bytes) else value
                   for key, value in request.headers.items()}
        condition = {key: value for key, value in headers.items()
                     if key in ('if-match', 'if-none-match')}
        body = request.body or b''
        if hasattr(body, 'read'):
            body = body.read()
        with self.store.lock:
            self.store.calls.append((request.method, path, condition))
            old = self.store.objects.get(path)
            if request.method == 'GET':
                if old is None:
                    return response(request, 404)
                value, etag = old
                return response(request, 200, value, {'etag': etag})
            if request.method != 'PUT':
                raise ValueError(f'unexpected synthetic request: {request.method}')
            if self.store.first_put_status:
                return response(request, self.store.first_put_status)
            etag = '"opaque-' + hashlib.sha256(body).hexdigest()[:10] + '"'
            if not self.store.ignore_conditions:
                if headers.get('if-none-match') == '*' and old is not None:
                    if self.store.overwrite_on_conflict:
                        self.store.objects[path] = body, etag
                    return response(request, 412)
                if 'if-match' in headers:
                    if old is None:
                        return response(request, self.store.missing_match_status)
                    if headers['if-match'] != old[1]:
                        return response(request, 412)
            self.store.objects[path] = body, etag
            return response(request, 200, headers={'etag': etag})


def factory(store):
    def make():
        s3 = cas.client('http://synthetic.invalid:9000', 'fixture-access', 'fixture-secret-only')
        s3._endpoint.http_session = Transport(store)
        return s3
    return make


class Clock:
    def __init__(self):
        self.now = 0
        self.sleeps = []

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


class StartupTransport:
    def __init__(self, clock, outcome, ready_at=None, request_seconds=0):
        self.clock = clock
        self.outcome = outcome
        self.ready_at = ready_at
        self.request_seconds = request_seconds
        self.calls = 0

    def send(self, request):
        self.calls += 1
        self.clock.now += self.request_seconds
        if self.ready_at is not None and self.clock.now >= self.ready_at:
            return response(request, 200, b'<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>')
        if isinstance(self.outcome, Exception):
            raise self.outcome
        return response(request, self.outcome, error_code='XMinioServerNotInitialized')


class RunnableRendezvous:
    """Real threads/barrier; fail only when every runnable slot is waiting.

    The guest's Barrier timeout is simulated time. In this host-only adapter,
    preserve its requested value as evidence, but never turn host scheduling
    delay into a rendezvous failure. A queued contender with every executor
    slot blocked at the barrier is a deterministic no-progress condition.
    """
    def __init__(self):
        self.lock = threading.Lock()
        self.first_waiter = threading.Event()
        self.timeouts = []
        self.stalls = []
        self.submitted = self.running = self.waiting = self.queued = 0
        self.departures = 0
        self.worker_slots = 0

    def barrier(self, parties, timeout):
        self.timeouts.append(timeout)
        self.real = threading.Barrier(parties)  # no host-time failure policy
        return self

    def abort(self):
        self.real.abort()

    def abort_if_stalled(self):
        # Caller holds lock. In particular, an available worker slot is NOT
        # stalled just because that worker has not been scheduled yet.
        if (not self.stalls and self.queued and self.worker_slots
                and self.running == self.waiting == self.worker_slots):
            self.stalls.append(dict(submitted=self.submitted, running=self.running,
                                    waiting=self.waiting, queued=self.queued,
                                    worker_slots=self.worker_slots))
            self.real.abort()

    def wait(self):
        with self.lock:
            self.waiting += 1
            self.first_waiter.set()
            self.abort_if_stalled()
        try:
            result = self.real.wait()
            with self.lock:
                self.departures += 1
            return result
        finally:
            with self.lock:
                self.waiting -= 1

    def executor(self, max_workers):
        scheduler = self
        scheduler.worker_slots = max_workers

        class Executor:
            def __init__(self):
                self.pool = ThreadPoolExecutor(max_workers=max_workers)

            def __enter__(self):
                self.pool.__enter__()
                return self

            def __exit__(self, *args):
                return self.pool.__exit__(*args)

            def submit(self, function, *args):
                def run():
                    with scheduler.lock:
                        scheduler.queued -= 1
                        scheduler.running += 1
                    try:
                        return function(*args)
                    finally:
                        with scheduler.lock:
                            scheduler.running -= 1
                with scheduler.lock:
                    scheduler.submitted += 1
                    scheduler.queued += 1
                    future = self.pool.submit(run)
                    scheduler.abort_if_stalled()
                return future

        return Executor()
