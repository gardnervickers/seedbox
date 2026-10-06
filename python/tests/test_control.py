"""Protocol contract tests use literal peer messages, independent of the SDK."""
import io
import json
from pathlib import Path
import sys
import socket
import tempfile
import threading
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from seedbox import FaultError, Machine, Partition, ProtocolError, World


def reply(request_id=1, result=None, **changes):
    value = dict(version=1, id=request_id, ok=True, result=result or {},
                 requested_at_ns=10, admitted_at_ns=20, effect_at_ns=30, replied_at_ns=40)
    value.update(changes)
    return (json.dumps(value) + "\n").encode()


class Peer:
    def __init__(self, response):
        self.response = io.BytesIO(response)
        self.requests = []
        self.closed = False

    def makefile(self, mode):
        assert mode == "rb"
        return self.response

    def sendall(self, request):
        self.requests.append(json.loads(request))

    def close(self):
        self.closed = True


class ControlTests(unittest.TestCase):
    def test_sequential_calls_use_public_rpc_and_confirmed_effects(self):
        peer = Peer(reply(result={"handle": "opaque-7"}) + reply(2) + reply(3) + reply(4) + reply(5) + reply(6, {"now_ns": 30}))
        with World(peer) as world:
            cut = world.partition(["client"], [Machine("server-vm")])
            self.assertIsInstance(cut, Partition)
            self.assertEqual(cut.handle, "opaque-7")
            self.assertEqual(cut.effect.effect_at_ns, 30)
            world.heal(cut)
            world.stop("server")
            world.restart("server")
            self.assertEqual(world.sleep(0.000000001).replied_at_ns, 40)
            self.assertEqual(world.now_ns(), 30)
        self.assertEqual(peer.requests, [
            {"version": 1, "id": 1, "op": "partition", "left": [{"service": "client"}], "right": [{"machine": "server-vm"}]},
            {"version": 1, "id": 2, "op": "heal", "handle": "opaque-7"},
            {"version": 1, "id": 3, "op": "stop", "target": {"service": "server"}},
            {"version": 1, "id": 4, "op": "restart", "target": {"service": "server"}},
            {"version": 1, "id": 5, "op": "sleep", "duration_ns": 1},
            {"version": 1, "id": 6, "op": "now"},
        ])
        self.assertTrue(peer.closed)

    def test_graceful_reconnect_starts_new_ids_and_preserves_explicit_handles(self):
        # Use actual Unix streams and independent literal peer messages. The
        # public client never sees the broker's private session/relay counters.
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / "control.sock")
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(path)
            listener.listen(1)
            requests, failures = [], []

            def serve():
                try:
                    for result in ({"handle": "retained-p1"}, {}):
                        channel, _ = listener.accept()
                        with channel, channel.makefile("rb") as reader:
                            request = json.loads(reader.readline())
                            requests.append(request)
                            channel.sendall(reply(result=result))
                            self.assertEqual(reader.read(1), b"")
                except BaseException as error:
                    failures.append(error)
                finally:
                    listener.close()

            server = threading.Thread(target=serve, daemon=True)
            server.start()
            with World.connect(path) as first:
                handle = first.partition(["client"], ["server"])
            with World.connect(path) as second:
                second.heal(handle)
            server.join(5)
            self.assertFalse(server.is_alive())
            self.assertEqual(failures, [])
            self.assertEqual([request["id"] for request in requests], [1, 1])
            self.assertEqual([request["op"] for request in requests], ["partition", "heal"])
            self.assertEqual(requests[1]["handle"], "retained-p1")

    def test_transport_loss_never_reconnects_or_retries_automatically(self):
        peer = Peer(b"")
        world = World(peer)
        with self.assertRaises(ProtocolError):
            world.stop("server")
        self.assertTrue(peer.closed)
        with self.assertRaisesRegex(ProtocolError, "closed"):
            world.restart("server")
        self.assertEqual(len(peer.requests), 1)

    def test_error_retains_code_and_raw_timing_without_implicit_retry(self):
        peer = Peer(reply(ok=False, error={"code": "inactive_handle", "message": "already healed"}))
        with World(peer) as world:
            with self.assertRaises(FaultError) as failure:
                world.heal(Partition("old", None))
            self.assertEqual(failure.exception.code, "inactive_handle")
            self.assertEqual(failure.exception.response["requested_at_ns"], 10)
        self.assertEqual(len(peer.requests), 1)

    def test_invalid_peer_response_closes_connection(self):
        responses = [reply().decode().encode("utf-16-be"), b"", b"{}", b"[]\n", b"not json\n", b"x" * 16385 + b"\n",
                     reply(2), reply(version=True), reply(effect_at_ns=19),
                     reply(replied_at_ns=-1), reply(result=[]), reply(ok="true"),
                     reply(ok=False, error={"code": "bad", "message": "bad"}, effect_at_ns=None)]
        # Empty-list result needs explicit replacement because reply() defaults {}.
        responses[-3] = reply().replace(b'"result": {}', b'"result": []')
        for response in responses:
            with self.subTest(response=response[:80]):
                peer = Peer(response)
                with World(peer) as world:
                    with self.assertRaises(ProtocolError):
                        world.stop("server")
                self.assertTrue(peer.closed)
                self.assertEqual(len(peer.requests), 1)

    def test_invalid_local_arguments_do_not_send_requests(self):
        peer = Peer(b"")
        with World(peer) as world:
            for seconds in [True, -1, float("nan"), float("inf"), 1e100, 0.0000000001]:
                with self.assertRaises(ValueError):
                    world.sleep(seconds)
            with self.assertRaises(ValueError):
                world.partition("client", ["server"])
            with self.assertRaises(ValueError):
                world.stop("")
            with self.assertRaises(ValueError):
                world.stop("x" * 16384)
        self.assertEqual(peer.requests, [])

    def test_inspection_exposes_protocol_result(self):
        peer = Peer(reply(result={"services": {"server": "vm-1"}, "active_handles": []}))
        with World(peer) as world:
            self.assertEqual(world.inspect()["services"], {"server": "vm-1"})
        self.assertEqual(peer.requests[0]["op"], "inspect")


if __name__ == "__main__":
    unittest.main()
