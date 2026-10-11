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
from seedbox import ClockDrift, ClockSkew, Controller, FaultError, Machine, Partition, ProtocolError, Shape, Spawned, Throttle, World


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
    def test_machine_lifecycle_targets_and_admission_effects(self):
        for op in ("pause", "resume", "crash", "reboot"):
            for target, wire in (("server", {"service": "server"}),
                                 (Machine("server-vm"), {"machine": "server-vm"})):
                with self.subTest(op=op, target=target):
                    peer = Peer(reply(effect_at_ns=20, replied_at_ns=30))
                    with World(peer) as world:
                        effect = getattr(world, op)(target)
                        self.assertEqual(effect.effect_at_ns, effect.admitted_at_ns)
                        self.assertEqual(effect.replied_at_ns, 30)
                        self.assertEqual(world.last_effect, effect)
                    self.assertEqual(peer.requests, [
                        {"version": 1, "id": 1, "op": op, "target": wire}])

    def test_machine_lifecycle_errors_preserve_timing_and_connection(self):
        for op in ("pause", "resume", "crash", "reboot"):
            for code in ("invalid_state", "target_unavailable"):
                with self.subTest(op=op, code=code):
                    peer = Peer(reply(ok=False, effect_at_ns=20,
                                      error={"code": code, "message": "unavailable transition"})
                                + reply(2, {"now_ns": 30}))
                    with World(peer) as world:
                        with self.assertRaises(FaultError) as failure:
                            getattr(world, op)("server")
                        self.assertEqual(failure.exception.code, code)
                        self.assertEqual(failure.exception.response["effect_at_ns"], 20)
                        self.assertEqual(world.now_ns(), 30)
                    self.assertEqual(len(peer.requests), 2)

    def test_machine_lifecycle_invalid_targets_do_not_send(self):
        peer = Peer(b"")
        with World(peer) as world:
            for op in ("pause", "resume", "crash", "reboot"):
                for target in ("", Machine(""), None, "x" * 16384):
                    with self.subTest(op=op, target=target):
                        with self.assertRaises(ValueError):
                            getattr(world, op)(target)
        self.assertEqual(peer.requests, [])

    def test_throttle_sends_a_speed_in_hz_and_heals_its_handle(self):
        peer = Peer(reply(result={"handle": "p3"}) + reply(2) + reply(3, result={"handle": "p4"}))
        with World(peer) as world:
            slow = world.throttle("server", "800MHz")
            self.assertEqual(slow, Throttle("p3", slow.effect))
            self.assertEqual(world.heal(slow).effect_at_ns, 30)
            world.throttle("server", 1_500_000_000)
        self.assertEqual(peer.requests, [
            {"version": 1, "id": 1, "op": "throttle", "target": {"service": "server"},
             "speed_hz": 800_000_000},
            {"version": 1, "id": 2, "op": "heal", "handle": "p3"},
            {"version": 1, "id": 3, "op": "throttle", "target": {"service": "server"},
             "speed_hz": 1_500_000_000}])
        with World(Peer(b"")) as world:
            for speed in ("2", "2ghz", "999Hz", "1.1THz", "0.5Hz", 2.5, True, "x", 0):
                with self.subTest(speed=speed), self.assertRaises(ValueError):
                    world.throttle("server", speed)
        peer = Peer(reply(result={}))
        with World(peer) as world, self.assertRaises(ProtocolError):
            world.throttle(Machine("server-vm"), "1GHz")

    def test_process_pause_and_resume_wire_and_errors(self):
        for op in ("pause_process", "resume_process"):
            for target, wire in (("server", {"service": "server"}),
                                 (Machine("server-vm"), {"machine": "server-vm"})):
                with self.subTest(op=op, target=target):
                    peer = Peer(reply(effect_at_ns=25, replied_at_ns=35)
                                + reply(2, ok=False, effect_at_ns=40, replied_at_ns=50,
                                        error={"code": "invalid_state", "message": "not paused"}))
                    with World(peer) as world:
                        effect = getattr(world, op)(target)
                        self.assertEqual(effect.effect_at_ns, 25)
                        with self.assertRaises(FaultError) as failure:
                            getattr(world, op)(target)
                        self.assertEqual(failure.exception.code, "invalid_state")
                    self.assertEqual(peer.requests, [
                        {"version": 1, "id": 1, "op": op, "target": wire},
                        {"version": 1, "id": 2, "op": op, "target": wire}])
                    with World(Peer(b"")) as world, self.assertRaises(ValueError):
                        getattr(world, op)("")

    def test_optional_response_details_do_not_relax_required_version(self):
        peer = Peer(reply(result={"now_ns": 30, "extra_result_detail": "opaque"},
                          extra_response_detail={"arbitrary": [1, None]}))
        with World(peer) as world:
            self.assertEqual(world.now_ns(), 30)
        for version in (0, 2):
            peer = Peer(reply(result={"now_ns": 30}, version=version,
                              extra_response_detail="still invalid"))
            with World(peer) as world:
                with self.assertRaises(ProtocolError):
                    world.now_ns()

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

    def test_shape_sends_absolute_nanosecond_policy_and_returns_healable_handle(self):
        self.assertIs(Controller, World)
        peer = Peer(reply(result={"handle": "p1"}) + reply(2))
        with World(peer) as world:
            shaped = world.shape(["client"], [Machine("server-vm")], latency=20_000_000, jitter=1_000_000, loss_ppm=500_000)
            self.assertIsInstance(shaped, Shape)
            world.heal(shaped)
        self.assertEqual(peer.requests[0], {"version": 1, "id": 1, "op": "shape", "from": [{"service": "client"}], "to": [{"machine": "server-vm"}], "latency_ns": 20_000_000, "jitter_ns": 1_000_000, "loss_ppm": 500_000})
        self.assertEqual(peer.requests[1]["handle"], "p1")

    def test_clock_skew_sends_one_offset_kind_and_returns_healable_handle(self):
        peer = Peer(reply(result={"handle": "p1", "offset_ns": -7}) + reply(2, {"handle": "p2", "offset_ns": 5}) + reply(3))
        with World(peer) as world:
            seeded = world.clock_skew("server", max_offset=1_000)
            self.assertEqual(seeded, ClockSkew("p1", -7, seeded.effect))
            fixed = world.clock_skew(Machine("db-vm"), offset=5)
            self.assertEqual(fixed.offset_ns, 5)
            world.heal(seeded)
        self.assertEqual(peer.requests, [
            {"version": 1, "id": 1, "op": "clock_skew", "target": {"service": "server"}, "max_offset_ns": 1_000},
            {"version": 1, "id": 2, "op": "clock_skew", "target": {"machine": "db-vm"}, "offset_ns": 5},
            {"version": 1, "id": 3, "op": "heal", "handle": "p1"},
        ])

    def test_clock_skew_rejects_invalid_local_values(self):
        peer = Peer(b"")
        with World(peer) as world:
            for kwargs in [{}, {"offset": 1, "max_offset": 1}, {"offset": 0}, {"max_offset": 0}, {"max_offset": -1},
                           {"offset": 1.5}, {"offset": True}, {"offset": 365 * 86_400 * 10**9 + 1}]:
                with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                    world.clock_skew("server", **kwargs)
        self.assertEqual(peer.requests, [])

    def test_clock_drift_sends_one_rate_kind_and_returns_healable_handle(self):
        peer = Peer(reply(result={"handle": "p1", "rate_ppb": -7}) + reply(2, {"handle": "p2", "rate_ppb": 5}) + reply(3))
        with World(peer) as world:
            seeded = world.clock_drift("server", max_ppb=1_000)
            self.assertEqual(seeded, ClockDrift("p1", -7, seeded.effect))
            fixed = world.clock_drift(Machine("db-vm"), ppb=5)
            self.assertEqual(fixed.rate_ppb, 5)
            world.heal(seeded)
        self.assertEqual(peer.requests, [
            {"version": 1, "id": 1, "op": "clock_drift", "target": {"service": "server"}, "max_rate_ppb": 1_000},
            {"version": 1, "id": 2, "op": "clock_drift", "target": {"machine": "db-vm"}, "rate_ppb": 5},
            {"version": 1, "id": 3, "op": "heal", "handle": "p1"},
        ])

    def test_clock_drift_rejects_invalid_local_values(self):
        peer = Peer(b"")
        with World(peer) as world:
            for kwargs in [{}, {"ppb": 1, "max_ppb": 1}, {"ppb": 0}, {"max_ppb": 0}, {"max_ppb": -1},
                           {"ppb": 1.5}, {"ppb": True}, {"ppb": -100_000_001}, {"max_ppb": 100_000_001}]:
                with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                    world.clock_drift("server", **kwargs)
        self.assertEqual(peer.requests, [])

    def test_connection_discovery_drop_and_heal_preserve_explicit_zero_evidence(self):
        connection = {"from": "server-vm", "to": "client-vm", "src_addr": "10.0.0.2",
                      "dst_addr": "10.0.0.1", "src_port": 80, "dst_port": 4000, "incarnation": 7}
        page = {"connections": [connection], "next_after": 7}
        peer = Peer(reply(result=page) + reply(2, {"handle": "p1"}) + reply(3, {"matched": 0, "dropped": 0}))
        with World(peer) as world:
            self.assertEqual(world.connections("server", Machine("client-vm"), after=3), page)
            handle = world.drop_connection(connection)
            self.assertEqual(world.heal(handle), {"matched": 0, "dropped": 0})
        self.assertEqual(peer.requests[0], {"version": 1, "id": 1, "op": "connections",
                         "from": {"service": "server"}, "to": {"machine": "client-vm"}, "after": 3})
        self.assertEqual(peer.requests[1]["connection"], connection)
        self.assertEqual(peer.requests[2]["handle"], "p1")

    def test_shape_defaults_and_invalid_local_values(self):
        peer = Peer(reply(result={"handle": "p1"}))
        with World(peer) as world:
            for kwargs in [{"latency": True}, {"latency": 1.5}, {"latency": -1}, {"latency": 10, "jitter": 10}, {"latency": 10, "loss_ppm": 1_000_001}, {"latency": 3_600_000_000_000, "jitter": 1}, {"latency": 10, "bandwidth_bps": 0}, {"latency": 10, "bandwidth_bps": True}, {"latency": 10, "bandwidth_bps": 1.5}, {"latency": 10, "queue_bound": 1}, {"latency": 10, "bandwidth_bps": 1, "queue_bound": -1}]:
                with self.assertRaises(ValueError):
                    world.shape(["client"], ["server"], **kwargs)
            self.assertEqual(peer.requests, [])
            world.shape(["client"], ["server"], latency=10)
        self.assertEqual(peer.requests[0]["jitter_ns"], 0)
        self.assertEqual(peer.requests[0]["loss_ppm"], 0)

    def test_bandwidth_shape_and_heal_preserve_queue_evidence(self):
        counts = {"matched": 2, "queue_dropped": 1, "delayed_max_ns": 24_000_000}
        peer = Peer(reply(result={"handle": "p1"}) + reply(2, {"queue_counts": counts}))
        with World(peer) as world:
            handle = world.shape(["client"], ["server"], latency=1_000_000,
                                 bandwidth_bps=1_000_000, queue_bound=12_000_000)
            self.assertEqual(world.heal(handle), {"queue_counts": counts})
        self.assertEqual(peer.requests[0]["bandwidth_bps"], 1_000_000)
        self.assertEqual(peer.requests[0]["queue_bound_ns"], 12_000_000)

    def test_each_network_fault_uses_shape_and_returns_heal_evidence(self):
        for kwargs, wire in [
            ({"reorder_ppm": 500_000, "reorder_delay": 100}, {"reorder_ppm": 500_000, "reorder_delay_ns": 100}),
            ({"duplicate_ppm": 1_000_000}, {"duplicate_ppm": 1_000_000}),
            ({"burst_loss_ppm": 10_000, "burst_length": 4}, {"burst_loss_ppm": 10_000, "burst_length": 4}),
            ({"latency_spike_ppm": 100_000, "latency_spike": 200}, {"latency_spike_ppm": 100_000, "latency_spike_ns": 200}),
            ({"tcp_reset_ppm": 1_000_000}, {"tcp_reset_ppm": 1_000_000}),
        ]:
            counts = {"matched": 3, "duplicated": 3}
            peer = Peer(reply(result={"handle": "p1"}) + reply(2, {"network_fault_counts": counts}))
            with World(peer) as world:
                handle = world.shape(["client"], ["server"], latency=10, **kwargs)
                self.assertEqual(world.heal(handle), {"network_fault_counts": counts})
            for key, value in wire.items():
                self.assertEqual(peer.requests[0]["network_faults"][key], value)
            self.assertEqual(peer.requests[1]["handle"], "p1")

    def test_invalid_network_faults_are_rejected_before_sending(self):
        peer = Peer(reply(result={"handle": "p1"}))
        with World(peer) as world:
            for kwargs in [
                {"duplicate_ppm": True}, {"duplicate_ppm": 0.5}, {"duplicate_ppm": 1_000_001},
                {"reorder_ppm": 1}, {"reorder_delay": -1},
                {"reorder_ppm": 1, "reorder_delay": 3_600_000_000_000},
                {"burst_loss_ppm": 1}, {"burst_length": 2**64},
                {"latency_spike_ppm": 1}, {"latency_spike": True},
                {"tcp_reset_ppm": -1},
            ]:
                with self.assertRaises(ValueError):
                    world.shape(["client"], ["server"], latency=10, **kwargs)
            self.assertEqual(peer.requests, [])
            world.shape(["client"], ["server"], latency=10)
        self.assertNotIn("network_faults", peer.requests[0])

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



class SpawnTests(unittest.TestCase):
    def test_spawn_sends_options_and_returns_the_new_machine(self):
        peer = Peer(reply(result={"machine": "etcd-4", "address": "10.0.0.5"},
                          effect_at_ns=20, replied_at_ns=30))
        with World(peer) as world:
            spawned = world.spawn("etcd-1", name="etcd-4", env={"ETCD_NAME": "etcd-4"},
                                  args=["etcd", "--initial-cluster-state=existing"])
        self.assertEqual(spawned, Spawned("etcd-4", "10.0.0.5", spawned.effect))
        self.assertEqual(spawned.effect.effect_at_ns, 20)
        self.assertEqual(peer.requests, [{
            "version": 1, "id": 1, "op": "spawn", "from": {"service": "etcd-1"}, "name": "etcd-4",
            "env": {"ETCD_NAME": "etcd-4"}, "argv": ["etcd", "--initial-cluster-state=existing"]}])

    def test_spawn_defaults_send_only_the_source(self):
        peer = Peer(reply(result={"machine": "db-2", "address": "10.0.0.3"}))
        with World(peer) as world:
            self.assertEqual(world.spawn(Machine("db")).machine, "db-2")
        self.assertEqual(peer.requests, [{"version": 1, "id": 1, "op": "spawn", "from": {"machine": "db"}}])

    def test_spawn_accepts_empty_env_and_materializes_args_once(self):
        peer = Peer(reply(result={"machine": "db-2", "address": "10.0.0.3"}))
        with World(peer) as world:
            world.spawn("db", env={}, args=(arg for arg in ("/bin/sh", "-c", "true")))
        self.assertEqual(peer.requests, [{"version": 1, "id": 1, "op": "spawn",
                                          "from": {"service": "db"}, "env": {},
                                          "argv": ["/bin/sh", "-c", "true"]}])

    def test_invalid_spawn_options_do_not_send(self):
        invalid = ({"name": ""}, {"env": []}, {"env": ""}, {"env": {"A=B": "x"}},
                   {"env": {"A\0": "x"}}, {"env": {"A": "x\0"}}, {"env": {"A": 1}},
                   {"args": "sh"}, {"args": b"sh"}, {"args": {}}, {"args": 3},
                   {"args": []}, {"args": iter(())}, {"args": [""]}, {"args": ["sh", "a\0"]})
        for options in invalid:
            with self.subTest(options=options):
                peer = Peer(b"")
                with World(peer) as world, self.assertRaises(ValueError):
                    world.spawn("db", **options)
                self.assertEqual(peer.requests, [])

    def test_spawn_response_without_machine_closes(self):
        peer = Peer(reply(result={"machine": "db-2"}))
        world = World(peer)
        with self.assertRaises(ProtocolError):
            world.spawn("db")
        self.assertTrue(peer.closed)


class TriggerTests(unittest.TestCase):
    def test_arm_and_cancel_literal_wire_and_partition_handle(self):
        from seedbox import Trigger
        peer = Peer(reply(result={"trigger_id": 1}) + reply(2) + reply(3, {"trigger_id": 2, "handle": "p1"}) + reply(4))
        with World(peer) as world:
            stop = world.arm_console_stop("server", b"A\x00B", Machine("server-vm"), match_timeout=1)
            self.assertIsInstance(stop, Trigger)
            world.cancel_trigger(stop)
            cut = world.arm_console_partition(Machine("server-vm"), b"DONE", ["server"], ["client"], match_timeout=2)
            world.heal(cut.partition)
        self.assertEqual(peer.requests[0], {"version": 1, "id": 1, "op": "arm_trigger", "source": {"service": "server"}, "marker": [65, 0, 66], "match_timeout_ns": 1_000_000_000, "fault": {"action": "stop", "target": {"machine": "server-vm"}}})
        self.assertEqual(peer.requests[1], {"version": 1, "id": 2, "op": "cancel_trigger", "trigger_id": 1})
        self.assertEqual(peer.requests[2]["fault"], {"action": "partition", "left": [{"service": "server"}], "right": [{"service": "client"}]})
        self.assertEqual(peer.requests[3]["handle"], "p1")

    def test_invalid_markers_timeouts_and_reply_identity(self):
        for marker, timeout in [(b"", 1), (b"x" * 257, 1), ("text", 1), (b"x", 0), (b"x", -1)]:
            peer = Peer(b"")
            with World(peer) as world:
                with self.assertRaises(ValueError):
                    world.arm_console_stop("server", marker, "server", match_timeout=timeout)
            self.assertEqual(peer.requests, [])
        for result in [{}, {"trigger_id": True}, {"trigger_id": 0}, {"trigger_id": 65}]:
            with World(Peer(reply(result=result))) as world:
                with self.assertRaises(ProtocolError):
                    world.arm_console_stop("server", b"x", "server", match_timeout=1)


if __name__ == "__main__":
    unittest.main()
