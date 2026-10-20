"""Language-only boundary to unchanged MinIO guest/client and host utilities.

Rust owns cases, mutation matrices, expectations, deadlines and discovery.
Only call translation and synthetic transport/clock wiring live here.
"""
import contextlib
import io
import json
from pathlib import Path
import socket
import sys
import threading
import types

ROOT = Path(__file__).resolve().parents[4]
sys.path[:0] = [str(ROOT / 'examples/minio/client'), str(ROOT / 'examples/minio')]
import cas
import boto3
import botocore
from botocore.exceptions import ReadTimeoutError
from urllib3 import exceptions as urllib_errors
from minio_transport import (Store, Transport, Clock, StartupTransport, factory,
                             response, RunnableRendezvous)
import verify
import stage_inputs
import generate


def forbid_socket(*args, **kwargs):
    raise RuntimeError('portable MinIO controls forbid sockets')


socket.socket = forbid_socket
request = json.load(sys.stdin)
observation = {}
stdout, stderr = io.StringIO(), io.StringIO()
store = Store()
for key, value in request.get('store', {}).items():
    setattr(store, key, value)
make = factory(store)
ledger = Path(request.get('ledger', '/nonexistent/ledger'))
clock = None
transport = None
rendezvous = RunnableRendezvous()
cas.Barrier = rendezvous.barrier
cas.ThreadPoolExecutor = rendezvous.executor


def outcome(value):
    if isinstance(value, int):
        return value
    if value == 'ValueError':
        return ValueError('unexpected transport bug')
    return getattr(botocore.exceptions, value)(endpoint_url='http://synthetic.invalid:9000')


try:
    with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        action = request['action']
        if request.get('omit_condition'):
            original_put = cas.put
            cas.put = lambda s3, key, value, **condition: original_put(s3, key, value)
        if request.get('wrong_winner'):
            original_get = cas.get

            def wrong_get(s3, key):
                value, etag = original_get(s3, key)
                return (b'unacknowledged', etag) if key == 'concurrent' else (value, etag)

            cas.get = wrong_get
        if action == 'qualify':
            observation['value'] = cas.qualify(make(), make)
        elif action == 'planted':
            if request.get('ambiguous'):
                observation['put_calls'] = 0

                def ambiguous(*args, **kwargs):
                    observation['put_calls'] += 1
                    raise ReadTimeoutError(endpoint_url='http://synthetic.invalid')

                cas.put = ambiguous
            observation['value'] = cas.planted(make(), make, request['mode'], ledger)
        elif action == 'headers':
            s3 = make()
            first_status, first = cas.put(s3, 'header', cas.A, IfNoneMatch='*')
            second_status, _ = cas.put(s3, 'header', cas.B, IfMatch=first['ETag'])
            observation.update(versions=[boto3.__version__, botocore.__version__],
                               statuses=[first_status, second_status], etag=first['ETag'],
                               connect_timeout=s3.meta.config.connect_timeout,
                               read_timeout=s3.meta.config.read_timeout,
                               attempts=s3.meta.config.retries['total_max_attempts'])
        elif action in ('ready', 'main'):
            clock = Clock()
            s3 = make()
            transport = StartupTransport(clock, outcome(request['outcome']),
                                         request.get('ready_at'), request.get('request_seconds', 0))
            s3._endpoint.http_session = transport
            cas.time = types.SimpleNamespace(monotonic=clock.monotonic, sleep=clock.sleep)
            if action == 'ready':
                observation['value'] = cas.ready(s3)
            else:
                observation.update(create_calls=0, workload_calls=0)

                def create(*args):
                    observation['create_calls'] += 1

                def workload(*args):
                    observation['workload_calls'] += 1

                cas.client = lambda *args: s3
                cas.create_bucket = create
                cas.planted = workload
                sys.argv = ['cas.py', 'broken', '--endpoint', 'http://synthetic.invalid:9000',
                            '--access-key', 'fixture-access', '--secret-key', 'fixture-secret-only',
                            '--ledger', str(ledger)]
                observation['value'] = cas.main()
        elif action == 'translation':
            errors = {
                'NewConnectionError': lambda: urllib_errors.NewConnectionError(None, 'connection refused'),
                'ProtocolError': lambda: urllib_errors.ProtocolError('connection reset', ConnectionResetError()),
                'ConnectTimeoutError': lambda: urllib_errors.ConnectTimeoutError('connect timed out'),
                'ReadTimeoutError': lambda: urllib_errors.ReadTimeoutError(None, '/', 'read timed out'),
            }
            s3 = make()
            # Replace only the underlying urllib3 connection, leaving the real
            # botocore HTTP session's exception translation intact.
            s3 = cas.client('http://synthetic.invalid:9000', 'fixture-access', 'fixture-secret-only')
            observation['urlopen_calls'] = 0

            def urlopen(*args, **kwargs):
                observation['urlopen_calls'] += 1
                raise errors[request['underlying']]()

            connection = types.SimpleNamespace(urlopen=urlopen)
            manager = types.SimpleNamespace(connection_from_url=lambda *args, **kwargs: connection)
            s3._endpoint.http_session._get_connection_manager = lambda *args, **kwargs: manager
            s3.list_buckets()
        elif action == 'put_failure':
            class Failing:
                def __init__(self):
                    self.calls = 0

                def send(self, request):
                    self.calls += 1
                    if request_data['timeout']:
                        raise ReadTimeoutError(endpoint_url=request.url)
                    return response(request, 503)

            request_data = request
            transport = Failing()
            s3 = make()
            s3._endpoint.http_session = transport
            s3.put_object(Bucket=cas.BUCKET, Key='slot', Body=cas.A, IfNoneMatch='*')
        elif action == 'concurrent':
            constructed, arrivals, setups = set(), set(), set()
            lock = threading.Lock()
            observation['dispatch'] = []

            class TrackedBarrier:
                def __init__(self, parties, timeout):
                    self.real = rendezvous.barrier(parties, timeout)

                def wait(self):
                    with lock:
                        if threading.get_ident() not in constructed:
                            raise cas.QualificationError('rendezvous before client setup')
                        if request.get('omit_barrier'):
                            return 0
                        arrivals.add(threading.get_ident())
                    return self.real.wait()

                def abort(self):
                    self.real.abort()

            class GuardedTransport(Transport):
                def send(self, request):
                    with lock:
                        observation['dispatch'].append([len(constructed), len(arrivals), arrivals == constructed])
                        if len(constructed) != 2 or arrivals != constructed:
                            raise cas.QualificationError('start rendezvous omitted before PUT')
                    return super().send(request)

            original_make = make

            def tracked_make():
                if request.get('setup_failure'):
                    raise ValueError('synthetic client setup failure')
                if request.get('stagger_setup'):
                    with lock:
                        first = not setups
                        setups.add(threading.get_ident())
                    if not first:
                        rendezvous.first_waiter.wait()
                        observation['setup_waited_for_peer'] = True
                s3 = original_make()
                s3._endpoint.http_session = GuardedTransport(store)
                with lock:
                    constructed.add(threading.get_ident())
                return s3

            cas.Barrier = TrackedBarrier
            if request.get('sequential'):
                executor = cas.ThreadPoolExecutor
                cas.ThreadPoolExecutor = lambda max_workers: executor(max_workers=1)
            observation['value'] = [(status, value.hex()) for status, value in cas.concurrent_creates(tracked_make)]
            observation['arrivals'] = len(arrivals)
        elif action == 'console':
            cas.emit('MINIO_HTTP', operation='list_buckets', key=None, condition={}, status=200, etag=None)
            cas.emit('MINIO_HTTP', operation='create_bucket', key=None, condition={}, status=200, etag=None)
            if request['mode'] == 'qualification':
                observation['value'] = cas.qualify(make(), make)
            else:
                observation['value'] = cas.planted(make(), make, request['mode'], ledger)
        elif action == 'check_console':
            observation['value'] = verify.check_console(request['text'], request['mode'])
        elif action == 'verify':
            observation['value'] = verify.verify(Path(request['root']), request['mode'])
        elif action in ('stage', 'generate'):
            stage_inputs.PINS = request['pins']
            if action == 'stage':
                observation['value'] = stage_inputs.stage(Path(request['root']), Path(request['binary']), Path(request['wheels']))
            else:
                observation['value'] = str(generate.generate(Path(request['root']), Path(request['binary']), Path(request['wheels'])))
        else:
            raise ValueError('unknown adapter action')
except Exception as error:
    observation['error'] = {'type': type(error).__name__, 'message': str(error)}
observation.update(rendezvous_timeouts=rendezvous.timeouts,
                   rendezvous_stalls=rendezvous.stalls,
                   rendezvous_departures=rendezvous.departures)
observation.update(stdout=stdout.getvalue(), stderr=stderr.getvalue(), calls=store.calls,
                   ledger=ledger.read_bytes().hex() if ledger.exists() else None)
if transport is not None:
    observation['transport_calls'] = transport.calls
if clock is not None:
    observation.update(now=clock.now, sleeps=clock.sleeps)
print(json.dumps(observation))
