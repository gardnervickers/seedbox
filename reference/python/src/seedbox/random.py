"""Versioned, domain-separated randomness without host entropy."""

import hashlib


class RandomStream:
    def __init__(self, seed: int, domain: str):
        if type(seed) is not int or not 0 <= seed < 2**64:
            raise ValueError("seed must be an unsigned 64-bit integer")
        self.key = hashlib.sha256(
            b"seedbox-rng-v1\0" + seed.to_bytes(8, "little") + domain.encode("utf-8")
        ).digest()
        self.counter = 0
        self.buffer = b""

    def read(self, length: int) -> bytes:
        if length < 0:
            raise ValueError("negative random-data length")
        while len(self.buffer) < length:
            self.buffer += hashlib.sha256(
                self.key + self.counter.to_bytes(16, "little")
            ).digest()
            self.counter += 1
        value, self.buffer = self.buffer[:length], self.buffer[length:]
        return value

    def below(self, bound: int) -> int:
        if not 0 < bound <= 2**64:
            raise ValueError("random bound must be in 1..2**64")
        limit = 2**64 - 2**64 % bound
        while True:
            value = int.from_bytes(self.read(8), "little")
            if value < limit:
                return value % bound
