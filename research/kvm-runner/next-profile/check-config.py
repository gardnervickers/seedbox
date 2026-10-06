"""Fail if a future resolved kernel silently drops the pinned tickless profile."""
import pathlib, sys
required = {
    "CONFIG_NO_HZ_IDLE": "y", "CONFIG_NO_HZ_FULL": "n",
    "CONFIG_HZ_100": "y", "CONFIG_HZ": "100",
    "CONFIG_HZ_250": "n", "CONFIG_HZ_300": "n", "CONFIG_HZ_1000": "n",
    "CONFIG_HIGH_RES_TIMERS": "y", "CONFIG_SMP": "n", "CONFIG_PREEMPT_NONE": "y",
}
found = {}
for line in pathlib.Path(sys.argv[1]).read_text().splitlines():
    if line.startswith("CONFIG_") and "=" in line:
        k, v = line.split("=", 1); found[k] = v
    elif line.startswith("# CONFIG_") and line.endswith(" is not set"):
        found[line[2:-len(" is not set")]] = "n"
bad = {k: {"expected": v, "actual": found.get(k)} for k, v in required.items() if found.get(k) != v}
if bad:
    raise SystemExit(f"TICKLESS_PROFILE_REJECTED {bad}")
print("TICKLESS_PROFILE_MATCH (not proof of one-shot device/idle operation)")
