from dataclasses import asdict, dataclass, field
import ipaddress
import json
from pathlib import Path


def integer(value: object, name: str, minimum: int, maximum: int = 2**63 - 1) -> int:
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValueError(f"{name} must be an integer in {minimum}..{maximum}")
    return value


def fields(value: object, allowed: set[str], name: str) -> dict:
    if not isinstance(value, dict) or set(value) - allowed:
        raise ValueError(f"invalid {name} fields")
    return value


@dataclass(frozen=True)
class NetworkConfig:
    latency_ns: int = 10_000
    jitter_ns: int = 0
    loss_ppm: int = 0
    duplicate_ppm: int = 0
    partitions: tuple[dict, ...] = ()


@dataclass(frozen=True)
class DiskConfig:
    latency_ns: int = 5_000
    error_ppm: int = 0


@dataclass(frozen=True)
class RunConfig:
    tick_ns: int = 10
    quantum_min: int = 3
    quantum_max: int = 17
    max_instructions: int = 100_000
    epoch_ns: int = 1_577_836_800_000_000_000
    network: NetworkConfig = field(default_factory=NetworkConfig)
    disk: DiskConfig = field(default_factory=DiskConfig)


@dataclass(frozen=True)
class NodeSpec:
    name: str
    image: bytes
    ip: str
    argv: tuple[str, ...] = ("/app",)
    files: dict[str, bytes] = field(default_factory=dict)


def validate(config: RunConfig, nodes: list[NodeSpec]) -> None:
    integer(config.tick_ns, "tick_ns", 1, 10**9)
    integer(config.quantum_min, "quantum_min", 1, 100_000)
    integer(config.quantum_max, "quantum_max", config.quantum_min, 100_000)
    integer(config.max_instructions, "max_instructions", 1, 10**8)
    integer(config.epoch_ns, "epoch_ns", 0)
    for key, value in asdict(config.disk).items():
        integer(value, f"disk.{key}", 0, 10**6 if key.endswith("ppm") else 10**15)
    for key in ("latency_ns", "jitter_ns", "loss_ppm", "duplicate_ppm"):
        integer(getattr(config.network, key), f"network.{key}", 0,
                10**6 if key.endswith("ppm") else 10**15)
    if not nodes or len(nodes) > 32:
        raise ValueError("supply 1..32 nodes")
    names, ips = set(), set()
    for node in nodes:
        if not isinstance(node.name, str) or not node.name or len(node.name) > 64 or node.name in names:
            raise ValueError("node names must be unique nonempty strings of at most 64 characters")
        names.add(node.name)
        ip = str(ipaddress.IPv4Address(node.ip))
        if ip != node.ip or ip in ips or ip == "0.0.0.0":
            raise ValueError("nodes require unique canonical nonzero IPv4 addresses")
        ips.add(ip)
        if not node.argv or len(node.argv) > 128 or not all(isinstance(arg, str) and "\0" not in arg for arg in node.argv):
            raise ValueError("invalid guest argv")
        for path, data in node.files.items():
            if not isinstance(path, str) or not isinstance(data, bytes) or len(data) > 32 * 1024 * 1024:
                raise ValueError("invalid initial file")
    for rule in config.network.partitions:
        fields(rule, {"source", "destination", "start_ns", "end_ns"}, "partition")
        if set(rule) != {"source", "destination", "start_ns", "end_ns"}:
            raise ValueError("partition requires source, destination, start_ns, end_ns")
        if rule["source"] not in names or rule["destination"] not in names:
            raise ValueError("partition endpoints must be node names")
        integer(rule["start_ns"], "partition.start_ns", 0)
        integer(rule["end_ns"], "partition.end_ns", rule["start_ns"] + 1)


def load_manifest(path: Path) -> tuple[RunConfig, list[NodeSpec]]:
    value = fields(json.loads(path.read_text()), {"schema", "nodes", "scheduler", "network", "disk", "epoch_ns"}, "manifest")
    if value.get("schema") != 1 or type(value.get("schema")) is not int:
        raise ValueError("manifest schema must be 1")
    scheduler = fields(value.get("scheduler", {}), {"tick_ns", "quantum_min", "quantum_max", "max_instructions"}, "scheduler")
    net = fields(value.get("network", {}), {"latency_ns", "jitter_ns", "loss_ppm", "duplicate_ppm", "partitions"}, "network")
    disk = fields(value.get("disk", {}), {"latency_ns", "error_ppm"}, "disk")
    config = RunConfig(**scheduler, network=NetworkConfig(**{**net, "partitions": tuple(net.get("partitions", []))}),
                       disk=DiskConfig(**disk), epoch_ns=value.get("epoch_ns", RunConfig().epoch_ns))
    raw_nodes = value.get("nodes")
    if not isinstance(raw_nodes, list):
        raise ValueError("nodes must be a list")
    nodes = []
    for raw in raw_nodes:
        raw = fields(raw, {"name", "executable", "ip", "argv", "files"}, "node")
        initial = {}
        for name, data in fields(raw.get("files", {}), set(raw.get("files", {})), "files").items():
            if isinstance(data, str):
                initial[name] = data.encode()
            elif isinstance(data, dict) and set(data) == {"hex"} and isinstance(data["hex"], str):
                initial[name] = bytes.fromhex(data["hex"])
            else:
                raise ValueError("file contents must be text or {hex: string}")
        nodes.append(NodeSpec(raw["name"], (path.parent / raw["executable"]).read_bytes(), raw["ip"],
                              tuple(raw.get("argv", ["/app"])), initial))
    validate(config, nodes)
    return config, nodes
