"""Run the existing race workload and verify its atomic control inside the guest."""
import re
import subprocess


def main():
    for mode, extra in (("racy", []), ("atomic", ["atomic"])):
        result = subprocess.run(["/race", "3000000", *extra], capture_output=True, text=True, check=True)
        print(result.stdout, end="", flush=True)
        found = re.findall(r"RACE mode=(\w+) counter=(-?\d+) expected=(-?\d+) lost=(-?\d+)", result.stdout)
        if len(found) != 1 or found[0][0] != mode:
            raise RuntimeError("missing race result")
        _, count, expected, lost = found[0]
        if int(expected) - int(count) != int(lost) or (mode == "atomic" and int(lost) != 0):
            raise RuntimeError("invalid counter or atomic control lost updates")


if __name__ == "__main__":
    main()
