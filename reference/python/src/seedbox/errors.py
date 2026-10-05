class SimulationError(Exception):
    """A deterministic failure in the simulator or unsupported guest behavior."""


class UnsupportedGuestError(SimulationError):
    """Guest behavior beyond the explicitly supported execution contract."""


class GuestMemoryError(Exception):
    """A guest supplied an inaccessible address."""
