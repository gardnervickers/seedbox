"""Write a recorded list-append history as a Jepsen/Elle EDN history, one op per line.

Input: an oracle log whose ``H {json}`` lines each hold one completed transaction
(``i``, ``p`` process, ``type`` ok/fail/info, ``t0`` invocation and ``t1``
completion in seconds, ``value`` micro-operations). Each transaction becomes an
``:invoke`` op at ``t0`` (reads ``nil``) and a completion op at ``t1``, as Jepsen
records them, sorted by time with ``:index`` in that order and ``:time`` in
nanoseconds:

    {:index 0, :time 2265724000, :type :invoke, :process 1, :f :txn, :value [[:append 1 1] [:r 0 nil]]}
    {:index 5, :time 2303639000, :type :ok, :process 1, :f :txn, :value [[:append 1 1] [:r 0 []]]}

``fail`` and ``info`` completions carry the invocation's micro-operations, as
Jepsen's do. A non-integer process (the oracle's final read) gets the next free
integer. Usage: python3 to_edn.py LOG [OUT.edn]
"""
import json
import sys


def load(path):
    out = []
    with open(path, errors="replace") as handle:
        for line in handle:
            if line.startswith("H "):
                out.append(json.loads(line[2:]))
    return out


def micro(ops, with_reads):
    parts = []
    for f, k, v in ops:
        if f == "append":
            parts.append(f"[:append {int(k)} {int(v)}]")
        elif not with_reads or v is None:
            parts.append(f"[:r {int(k)} nil]")
        else:
            parts.append(f"[:r {int(k)} [{' '.join(str(int(x)) for x in v)}]]")
    return "[" + " ".join(parts) + "]"


def ops(txns):
    numeric = [t["p"] for t in txns if isinstance(t["p"], int)]
    spare = max(numeric, default=-1) + 1
    events, sequence = [], {}
    for t in sorted(txns, key=lambda t: (t["t0"], t["i"])):
        process = t["p"] if isinstance(t["p"], int) else spare
        invoke = [[f, k, None if f == "r" else v] for f, k, v in t["value"]]
        # Ties in time keep each process's own order: its invocation, then its completion.
        n = sequence.get(process, 0)
        sequence[process] = n + 2
        events.append((round(t["t0"] * 1e9), n, t["i"], "invoke", process, micro(invoke, False)))
        events.append((round(t["t1"] * 1e9), n + 1, t["i"], t["type"], process,
                       micro(t["value"], t["type"] == "ok")))
    events.sort()
    for index, (time, _, _, kind, process, value) in enumerate(events):
        yield f"{{:index {index}, :time {time}, :type :{kind}, :process {process}, :f :txn, :value {value}}}"


def main():
    txns = load(sys.argv[1])
    out = open(sys.argv[2], "w") if len(sys.argv) > 2 else sys.stdout
    for line in ops(txns):
        out.write(line + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
