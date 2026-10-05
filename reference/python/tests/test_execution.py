from dataclasses import replace
import os
from pathlib import Path
import struct
import subprocess
import sys

import pytest
from unicorn import x86_const as x86

from seedbox.backend import Process
from seedbox.config import DiskConfig, NetworkConfig, NodeSpec, RunConfig, load_manifest
from seedbox.random import RandomStream
from seedbox.runtime import Simulation, canonical

ROOT = Path(__file__).resolve().parents[1]


def node(image, name="app", ip="10.0.0.1"):
    return NodeSpec(name, image, ip)


@pytest.mark.parametrize("budget", [1, 2, 3, 7, 17, 64])
def test_cpu_only_loop_preempts_at_exact_instruction_budget(guests, budget):
    process = Process("spin", guests["spin"], RandomStream(42, "entropy:spin"), ["/app"], 1)
    entered = []
    thread = process.threads[1]
    stop = process.run(thread, budget, lambda address, data: entered.append((address, data)))
    assert stop.reason == "quantum"
    assert stop.steps == len(entered) == budget
    assert process.machine.reg_read(x86.UC_X86_REG_RAX) == (budget + 1) // 2
    # A second quantum resumes the CPU context rather than restarting the loop.
    stop = process.run(thread, budget, lambda address, data: None)
    assert stop.steps == budget
    assert process.machine.reg_read(x86.UC_X86_REG_RAX) == budget


def test_syscall_stops_before_execution_and_resumes_after_completion(guests):
    process = Process("hello", guests["hello"], RandomStream(0, "test"), ["/app"], 1)
    thread = process.threads[1]
    stop = process.run(thread, 100, lambda address, data: None)
    assert stop.reason == "syscall"
    assert stop.request.number == 1
    assert stop.steps == 5
    assert process.machine.reg_read(x86.UC_X86_REG_RIP) == stop.request.pc
    process.complete(thread, stop.request, 6)
    stop = process.run(thread, 100, lambda address, data: None)
    assert stop.request.number == 60
    assert stop.steps == 3


def test_rep_string_instruction_can_be_preempted_and_resumed(assemble):
    image = assemble("""
        lea rsi, [rel source]
        lea rdi, [rel target]
        mov ecx, 64
        rep movsb
        mov eax, 60
        xor edi, edi
        syscall
    """, "source: times 64 db 0x5a", "target: resb 64")
    process = Process("copy", image, RandomStream(0, "copy"), ["/app"], 1)
    thread = process.threads[1]
    stop = process.run(thread, 4, lambda address, data: None)
    assert stop.reason == "quantum"
    remaining = process.machine.reg_read(x86.UC_X86_REG_RCX)
    assert 0 < remaining < 64
    stop = process.run(thread, 4, lambda address, data: None)
    assert stop.reason == "quantum"
    assert process.machine.reg_read(x86.UC_X86_REG_RCX) == remaining - 4
    report = Simulation([node(image)], 42).run()
    assert report["status"] == "completed"


def test_same_seed_regenerates_entire_cluster_report(guests):
    config, specs = load_manifest(ROOT / "examples/cluster.json")
    reports = [Simulation(specs, 42, config).run() for _ in range(3)]
    assert all(report["status"] == "completed" for report in reports)
    assert canonical(reports[0]) == canonical(reports[1]) == canonical(reports[2])
    assert bytes.fromhex(reports[0]["nodes"]["client"]["stdout_hex"]) == b"pong"
    assert bytes.fromhex(reports[0]["nodes"]["server"]["stdout_hex"]) == b"ping"


def test_different_seeds_explore_different_execution(guests):
    config, specs = load_manifest(ROOT / "examples/cluster.json")
    first, second = (Simulation(specs, seed, config).run() for seed in (1, 2))
    assert first["status"] == second["status"] == "completed"
    assert first["trace_sha256"] != second["trace_sha256"]
    assert first["instruction_sha256"] != second["instruction_sha256"]


def test_race_negative_control_and_atomic_fix(guests):
    config = RunConfig(quantum_min=1, quantum_max=7)
    observed = []
    for seed in range(20):
        broken = Simulation([node(guests["race"])], seed, config).run()
        fixed = Simulation([node(guests["atomic"])], seed, config).run()
        assert broken["status"] == fixed["status"] == "completed"
        observed.append(int.from_bytes(bytes.fromhex(broken["nodes"]["app"]["stdout_hex"]), "little"))
        assert int.from_bytes(bytes.fromhex(fixed["nodes"]["app"]["stdout_hex"]), "little") == 64
        assert len(fixed["nodes"]["app"]["threads"]) == 2
    assert any(value != 64 for value in observed)
    assert len(set(observed)) > 1
    first = Simulation([node(guests["race"])], 42, config).run()
    assert canonical(first) == canonical(Simulation([node(guests["race"])], 42, config).run())


def test_packet_delivery_deadlines_preempt_runnable_cpu_loop(guests):
    config = RunConfig(tick_ns=7, max_instructions=10_000,
                       network=NetworkConfig(latency_ns=10_003))
    specs = [node(guests["client"], "client"), node(guests["server"], "server", "10.0.0.2"),
             node(guests["spin"], "spin", "10.0.0.3")]
    report = Simulation(specs, 42, config).run()
    assert report["status"] == "instruction_limit"
    assert bytes.fromhex(report["nodes"]["client"]["stdout_hex"]) == b"pong"
    sends = [e["time_ns"] for e in report["trace"] if e["event"] == "packet_send"]
    arrivals = [e["time_ns"] for e in report["trace"] if e["event"] == "packet_deliver"]
    assert len(sends) == len(arrivals) == 2
    assert [arrive - send for arrive, send in zip(arrivals, sends)] == [10_003, 10_003]
    assert not any(e["event"] == "advance" for e in report["trace"])


def test_network_loss_is_seeded_and_reports_deadlock(guests):
    config, specs = load_manifest(ROOT / "examples/cluster.json")
    config = replace(config, network=NetworkConfig(loss_ppm=1_000_000))
    first, second = (Simulation(specs, 42, config).run() for _ in range(2))
    assert first["status"] == "deadlock"
    assert canonical(first) == canonical(second)
    assert any(e["event"] == "packet_drop" and e["reason"] == "loss" for e in first["trace"])
    assert first["nodes"]["client"]["stdout_hex"] == ""


def test_partition_can_drop_a_packet_already_in_flight(guests):
    cfg, specs = load_manifest(ROOT / "examples/cluster.json")
    rule = {"source": "client", "destination": "server", "start_ns": 1000, "end_ns": 1_000_000}
    report = Simulation(specs, 42, replace(cfg, network=NetworkConfig(partitions=(rule,)))).run()
    assert report["status"] == "deadlock"
    send = next(e for e in report["trace"] if e["event"] == "packet_send")
    drop = next(e for e in report["trace"] if e["event"] == "packet_drop")
    assert send["time_ns"] < 1000 <= drop["time_ns"]
    assert drop["reason"] == "partition"


def test_fault_choices_do_not_change_application_entropy(guests):
    cfg, specs = load_manifest(ROOT / "examples/cluster.json")
    normal = Simulation(specs, 42, cfg).run()
    faulted = Simulation(specs, 42, replace(cfg, network=NetworkConfig(loss_ppm=1_000_000))).run()
    assert bytes.fromhex(normal["nodes"]["storage"]["stdout_hex"])[5:13] == bytes.fromhex(
        faulted["nodes"]["storage"]["stdout_hex"])[5:13]


def test_disk_completion_durability_and_virtual_sleep(guests):
    report = Simulation([node(guests["disk"])], 42).run()
    assert report["status"] == "completed"
    output = bytes.fromhex(report["nodes"]["app"]["stdout_hex"])
    assert output[:5] == b"state"
    seconds_before, ns_before, seconds_after, ns_after = struct.unpack("<qqqq", output[13:])
    assert (seconds_after * 10**9 + ns_after) - (seconds_before * 10**9 + ns_before) >= 10**9
    assert report["nodes"]["app"]["files"]["/state"] == {
        "contents_hex": b"state".hex(), "durable_hex": b"state".hex()}
    assert report["instructions"] < 100
    assert any(e["event"] == "block" and e["kind"] == "disk" for e in report["trace"])


def test_disk_error_injection_does_not_apply_failed_writes(guests):
    report = Simulation([node(guests["disk"])], 42,
                        RunConfig(disk=DiskConfig(error_ppm=1_000_000))).run()
    assert report["status"] == "completed"
    assert report["nodes"]["app"]["files"]["/state"] == {"contents_hex": "", "durable_hex": ""}
    assert [e["op"] for e in report["trace"] if e["event"] == "disk_fault"] == [1, 74, 0]


def test_futex_wait_times_out_at_virtual_deadline(assemble):
    image = assemble("""
        mov eax, 202
        lea rdi, [rel futex_word]
        mov esi, 128
        xor edx, edx
        lea r10, [rel timeout]
        syscall
        mov [rel result], eax
        mov eax, 1
        mov edi, 1
        lea rsi, [rel result]
        mov edx, 4
        syscall
        mov eax, 60
        xor edi, edi
        syscall
    """, "futex_word: dd 0\nresult: dd 0\ntimeout: dq 0, 1000000")
    report = Simulation([node(image)], 42).run()
    assert report["status"] == "completed"
    assert int.from_bytes(bytes.fromhex(report["nodes"]["app"]["stdout_hex"]), "little", signed=True) == -110
    block = next(e for e in report["trace"] if e["event"] == "block")
    wake = next(e for e in report["trace"] if e["event"] == "wake")
    assert wake["time_ns"] == block["deadline_ns"] == block["time_ns"] + 1_000_000


@pytest.mark.parametrize("instruction", ["rdtsc", "rdtscp", "rdrand eax", "rdseed eax", "cpuid", "sysenter", "int 0x80"])
def test_unmodeled_clock_entropy_and_syscall_paths_fail_closed(assemble, instruction):
    report = Simulation([node(assemble(instruction + "\nmov eax,60\nxor edi,edi\nsyscall"))], 42).run()
    assert report["status"] == "unsupported"
    assert "unsupported instruction" in report["error"]


def test_unknown_syscall_fails_closed(assemble):
    report = Simulation([node(assemble("mov eax,9999\nsyscall"))], 42).run()
    assert report["status"] == "unsupported"
    assert report["error"] == "unsupported syscall 9999"


def test_invalid_guest_pointer_returns_linux_efault(assemble):
    image = assemble("""
        mov eax, 228
        mov edi, 1
        mov esi, 1
        syscall
        mov [rel result], eax
        mov eax, 1
        mov edi, 1
        lea rsi, [rel result]
        mov edx, 4
        syscall
        mov eax, 60
        xor edi, edi
        syscall
    """, "result: dd 0")
    report = Simulation([node(image)], 42).run()
    assert report["status"] == "completed"
    assert int.from_bytes(bytes.fromhex(report["nodes"]["app"]["stdout_hex"]), "little", signed=True) == -14


def test_unmapped_instruction_memory_is_a_guest_fault(assemble):
    report = Simulation([node(assemble("mov rax, [0x100]\n"))], 42).run()
    assert report["status"] == "fault"


def test_fault_before_first_instruction_of_a_quantum_is_a_guest_fault(assemble):
    report = Simulation([node(assemble("mov eax, 0x100\njmp rax"))], 42,
                        RunConfig(quantum_min=1, quantum_max=1)).run()
    assert report["status"] == "fault"
    assert report["instructions"] == 2


def test_nonblocking_socket_returns_linux_eagain(assemble):
    image = assemble("""
        mov eax, 41
        mov edi, 2
        mov esi, 0x802
        xor edx, edx
        syscall
        mov rdi, rax
        mov eax, 45
        lea rsi, [rel buffer]
        mov edx, 8
        xor r10d, r10d
        xor r8d, r8d
        xor r9d, r9d
        syscall
        mov [rel result], eax
        mov eax, 1
        mov edi, 1
        lea rsi, [rel result]
        mov edx, 4
        syscall
        mov eax, 60
        xor edi, edi
        syscall
    """, "result: dd 0", "buffer: resb 8")
    report = Simulation([node(image)], 42).run()
    assert report["status"] == "completed"
    assert int.from_bytes(bytes.fromhex(report["nodes"]["app"]["stdout_hex"]), "little", signed=True) == -11


def test_zero_length_write_after_seek_does_not_extend_file(assemble):
    image = assemble("""
        mov eax, 257
        mov rdi, -100
        lea rsi, [rel path]
        mov edx, 0x42
        mov r10d, 0o600
        syscall
        mov r12, rax
        mov eax, 8
        mov rdi, r12
        mov esi, 100
        xor edx, edx
        syscall
        mov eax, 1
        mov rdi, r12
        xor esi, esi
        xor edx, edx
        syscall
        mov eax, 60
        xor edi, edi
        syscall
    """, 'path: db "/empty", 0')
    report = Simulation([node(image)], 42).run()
    assert report["status"] == "completed"
    assert report["nodes"]["app"]["files"]["/empty"] == {"contents_hex": "", "durable_hex": ""}


def test_instruction_observability_does_not_change_execution(guests):
    first = Simulation([node(guests["race"])], 42).run()
    detailed = Simulation([node(guests["race"])], 42, instruction_trace=True).run()
    assert first["instruction_sha256"] == detailed["instruction_sha256"]
    assert first["nodes"] == detailed["nodes"]
    assert len([e for e in detailed["trace"] if e["event"] == "instruction"]) == detailed["instructions"]
    assert [e["time_ns"] for e in detailed["trace"]] == sorted(e["time_ns"] for e in detailed["trace"])


def test_fresh_python_processes_and_hash_seeds_regenerate_same_bytes(guests, tmp_path):
    outputs = []
    for seed in ("1", "999"):
        output = tmp_path / f"{seed}.json"
        subprocess.run([sys.executable, "-m", "seedbox", "run", str(ROOT / "examples/cluster.json"),
                        "--seed", "42", "--output", str(output)], check=True, capture_output=True,
                       env={**os.environ, "PYTHONHASHSEED": seed})
        outputs.append(output.read_bytes())
    assert outputs[0] == outputs[1]


def test_random_stream_is_chunk_independent_and_domain_separated():
    one, chunks, other = RandomStream(42, "entropy:a"), RandomStream(42, "entropy:a"), RandomStream(42, "schedule")
    expected = one.read(100)
    assert expected == chunks.read(1) + chunks.read(63) + chunks.read(36)
    assert expected != other.read(100)


@pytest.mark.parametrize("override", [dict(tick_ns=0), dict(quantum_min=0), dict(quantum_min=5, quantum_max=4), dict(max_instructions=-1)])
def test_invalid_scheduler_configuration_is_rejected_before_execution(guests, override):
    with pytest.raises(ValueError):
        Simulation([node(guests["hello"])], 42, RunConfig(**override))
