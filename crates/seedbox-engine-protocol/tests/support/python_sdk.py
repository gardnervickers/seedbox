"""Language-only adapter for Rust-owned production Python SDK contract tests.

No expectations or test cases live here. Rust supplies calls and peer bytes and
checks the observations. This file is not a unittest suite or guest program.
"""
import builtins
import dataclasses
import io
import json
import sys

import seedbox


class Peer:
    def __init__(self, response):
        self.response = io.BytesIO(bytes(response))
        self.requests = []
        self.closed = False

    def makefile(self, mode):
        self.mode = mode
        return self.response

    def sendall(self, request):
        self.requests.append(json.loads(request))

    def close(self):
        self.closed = True


def decode(value):
    if isinstance(value, list):
        return [decode(item) for item in value]
    if isinstance(value, dict):
        if "$machine" in value:
            return seedbox.Machine(value["$machine"])
        if "$bytes" in value:
            return bytes(value["$bytes"])
        if "$float" in value:
            return float(value["$float"])
        if "$integer" in value:
            return int(value["$integer"])
        if "$ref" in value:
            result = saved[value["$ref"]]
            for field in value.get("fields", []):
                result = getattr(result, field)
            return result
        if "$partition" in value:
            return seedbox.Partition(value["$partition"], None)
        return {key: decode(item) for key, item in value.items()}
    return value


def encode(value):
    if dataclasses.is_dataclass(value):
        fields = {field.name: getattr(value, field.name) for field in dataclasses.fields(value)}
        return {"type": type(value).__name__, "public_type_identity": type(value) is getattr(seedbox, type(value).__name__, None),
                "fields": {key: encode(item) for key, item in fields.items()},
                "reconstructed_equal": value == type(value)(**fields)}
    return value


saved = {}
world = None
context = None
peer = None
for line in sys.stdin:
    command = json.loads(line)
    action = command["action"]
    if action == "call":
        # Translation/setup failures must terminate the adapter, never appear
        # as an exception raised by the public SDK method under observation.
        method = getattr(world, command["op"])
        args = decode(command.get("args", []))
        kwargs = decode(command.get("kwargs", {}))
    output = {}
    try:
        if action == "open":
            peer = Peer(command["response"])
            context = seedbox.World(peer)
            world = context.__enter__()
        elif action == "connect":
            peer = None
            context = seedbox.World.connect(command["path"])
            world = context.__enter__()
        elif action == "close":
            context.__exit__(None, None, None)
        elif action == "call":
            value = method(*args, **kwargs)
        else:
            raise ValueError("unknown adapter action")
    except Exception as error:
        output["error"] = {"type": type(error).__name__,
                           "public_type_identity": type(error) is getattr(seedbox, type(error).__name__, getattr(builtins, type(error).__name__, None)),
                           "message": str(error),
                           "code": getattr(error, "code", None), "response": getattr(error, "response", None)}
    else:
        if action == "call":
            if "save" in command:
                saved[command["save"]] = value
            output["value"] = encode(value)
            output["is_last_effect"] = value == world.last_effect
    output["controller_is_world"] = seedbox.Controller is seedbox.World
    output["last_effect"] = encode(world.last_effect) if world else None
    if peer is not None:
        output.update(requests=peer.requests, closed=peer.closed, mode=peer.mode)
    print(json.dumps(output, allow_nan=False), flush=True)
