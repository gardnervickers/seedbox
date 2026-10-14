"""An Elle-style checker for list-append transaction histories, in plain Python.

Each transaction is a list of micro-operations on keys whose values are lists:
``["append", k, v]`` adds ``v`` to the end of ``k``'s list, and ``["r", k, L]``
reads ``k``'s whole list ``L`` (``None`` while unknown). Appended values are
unique per key, so every element of a read names the transaction that wrote it,
and because a read returns the whole list, the version order of a key is the
prefix order of the lists read: the longest read is the order, and every other
read must be a prefix of it. See Kingsbury and Alvaro, "Elle: Inferring
Isolation Anomalies from Experimental Observations" (VLDB 2020).

Input is a list of completed transactions (``Txn``); ``from_history`` pairs a
Jepsen-style invoke/complete history into them. A completion's type is:

* ``ok``: committed; its reads are known.
* ``fail``: definitely aborted; nothing it did may be visible.
* ``info``: indeterminate (a timeout or a lost connection): it may or may not
  have committed and its reads are unknown. As in Elle, an ``info``
  transaction counts as committed when some committed read observes one of its
  appends, and is otherwise left out of the dependency graph.

Non-cyclic anomalies:

* ``G1a`` (aborted read): a committed read observes a failed transaction's append.
* ``G1b`` (intermediate read): a committed read ends with an append that its
  writer later followed with another append to the same key.
* ``internal``: a transaction's read disagrees with its own earlier reads and
  appends of that key.
* ``incompatible-order``: two committed reads of a key where neither is a
  prefix of the other.
* ``duplicate-elements``: a read lists a value twice.
* ``garbage``: a read lists a value that nobody appended.

The dependency graph over committed transactions has ww (write-write: T2
appended directly after T1 in the version order), wr (write-read: T2 read T1's
append as the last element written by another transaction) and rw
(read-write, an anti-dependency: T1 read a version and T2 wrote the next one)
edges. A committed append that no read observed must follow every observed
element, so it gets ww edges from the last observed writer and rw edges from
every committed read of that key that missed it; both are implied orders and
keep each cycle's count of rw edges. Cycles, found per strongly connected
component, are classified as in Adya's thesis:

* ``G0``: only ww edges;
* ``G1c``: ww and wr edges, at least one wr;
* ``G-single``: exactly one rw edge;
* ``G2-item``: two or more rw edges (item anti-dependencies; this workload
  has no predicates).

Under serializability every anomaly is a violation. Snapshot isolation allows
G2-item only, and read committed allows G-single and G2-item as well.
"""
import collections
import itertools

CYCLE_TYPES = ("G0", "G1c", "G-single", "G2-item")
ALLOWED = {
    "serializable": set(),
    "snapshot-isolation": {"G2-item"},
    "repeatable-read": {"G2-item"},
    "read-committed": {"G-single", "G2-item"},
}
# How many example edges each cycle search starts from, per component.
SEARCH_LIMIT = 200


class Txn:
    """One completed transaction: ``type`` is ok, fail or info."""

    __slots__ = ("id", "type", "value", "process", "invoke", "complete", "error")

    def __init__(self, id, type, value, process=None, invoke=None, complete=None, error=None):
        if type not in ("ok", "fail", "info"):
            raise ValueError(f"transaction {id}: type {type!r} is not ok, fail or info")
        self.id, self.type, self.value = id, type, [list(op) for op in value]
        self.process, self.invoke, self.complete, self.error = process, invoke, complete, error

    def describe(self):
        return {"id": self.id, "type": self.type, "process": self.process, "invoke": self.invoke,
                "complete": self.complete, "value": self.value}

    def __repr__(self):
        return f"Txn({self.id!r}, {self.type!r}, {self.value!r})"


def from_history(ops):
    """Pair a Jepsen-style history (dicts with process, type, value) into ``Txn``s.

    An invocation without a completion is ``info``. An ``info`` or ``fail``
    completion keeps the invocation's micro-operations (reads unknown).
    """
    pending, txns = {}, []
    for index, op in enumerate(ops):
        kind = op["type"]
        if kind == "invoke":
            if op["process"] in pending:
                raise ValueError(f"op {index}: process {op['process']} invoked twice")
            pending[op["process"]] = (index, op)
            continue
        start, invoke = pending.pop(op["process"])
        value = op["value"] if kind == "ok" else [[f, k, None if f == "r" else v] for f, k, v in invoke["value"]]
        txns.append(Txn(op.get("index", start), kind, value, op["process"], invoke.get("time"), op.get("time"),
                        op.get("error")))
    for start, invoke in pending.values():
        txns.append(Txn(invoke.get("index", start), "info",
                        [[f, k, None if f == "r" else v] for f, k, v in invoke["value"]],
                        invoke["process"], invoke.get("time")))
    return txns


class Graph:
    """Dependency edges: ``edges[a][b]`` maps an edge type to its first explanation."""

    def __init__(self):
        self.edges = collections.defaultdict(dict)

    def add(self, a, b, kind, why):
        if a == b:
            return
        self.edges[a].setdefault(b, {}).setdefault(kind, why)

    def nodes(self):
        out = set(self.edges)
        for targets in self.edges.values():
            out.update(targets)
        return out

    def count(self):
        counts = collections.Counter()
        for targets in self.edges.values():
            for kinds in targets.values():
                counts.update(list(kinds))
        return dict(counts)


def components(graph):
    """Strongly connected components with more than one node (iterative Tarjan)."""
    index, low, on_stack, stack, out = {}, {}, set(), [], []
    counter = itertools.count()
    for root in sorted(graph.nodes(), key=repr):
        if root in index:
            continue
        work = [(root, iter(sorted(graph.edges.get(root, {}), key=repr)))]
        index[root] = low[root] = next(counter)
        stack.append(root)
        on_stack.add(root)
        while work:
            node, children = work[-1]
            advanced = False
            for child in children:
                if child not in index:
                    index[child] = low[child] = next(counter)
                    stack.append(child)
                    on_stack.add(child)
                    work.append((child, iter(sorted(graph.edges.get(child, {}), key=repr))))
                    advanced = True
                    break
                if child in on_stack:
                    low[node] = min(low[node], index[child])
            if advanced:
                continue
            work.pop()
            if work:
                low[work[-1][0]] = min(low[work[-1][0]], low[node])
            if low[node] == index[node]:
                members = []
                while True:
                    member = stack.pop()
                    on_stack.discard(member)
                    members.append(member)
                    if member == node:
                        break
                if len(members) > 1:
                    out.append(sorted(members, key=repr))
    return out


def _steps(graph, node, members, allowed):
    """Outgoing (target, kind) steps, strongest kind first (ww, then wr, then rw)."""
    for target in sorted(graph.edges.get(node, {}), key=repr):
        if target in members:
            kinds = graph.edges[node][target]
            for kind in ("ww", "wr", "rw"):
                if kind in kinds and kind in allowed:
                    yield target, kind


def _path(graph, members, start, goal, allowed, need_rw, budget=200_000):
    """Shortest simple path start -> goal inside ``members`` over ``allowed`` edge kinds.

    Without ``need_rw`` this is a breadth-first search. With it the path must
    use at least one rw edge, found by iterative deepening over simple paths
    within ``budget`` expansions. Returns a list of (from, to, kind) steps, or None.
    """
    if not need_rw:
        parents = {start: None}
        queue = collections.deque([start])
        while queue:
            node = queue.popleft()
            for target, kind in _steps(graph, node, members, allowed):
                if target in parents:
                    continue
                parents[target] = (node, kind)
                if target == goal:
                    steps = []
                    while parents[target] is not None:
                        previous, kind = parents[target]
                        steps.append((previous, target, kind))
                        target = previous
                    return steps[::-1]
                queue.append(target)
        return None
    remaining = [budget]

    def search(node, depth, used, seen, steps):
        remaining[0] -= 1
        if remaining[0] < 0:
            return None
        for target, kind in _steps(graph, node, members, allowed):
            uses = used or kind == "rw"
            if target == goal:
                if uses:
                    return steps + [(node, target, kind)]
                continue
            if depth > 1 and target not in seen:
                seen.add(target)
                found = search(target, depth - 1, uses, seen, steps + [(node, target, kind)])
                seen.discard(target)
                if found:
                    return found
        return None

    for depth in range(1, len(members)):
        found = search(start, depth, False, {start, goal}, [])
        if found or remaining[0] < 0:
            return found
    return None


def _cycles(graph, members):
    """One shortest example cycle of each type found in one component."""
    member_set = set(members)
    found = {}

    def edges_of(kind):
        return [(a, b) for a in members for b, kinds in sorted(graph.edges.get(a, {}).items(), key=lambda kv: repr(kv[0]))
                if b in member_set and kind in kinds]

    def best(candidates, allowed, need_rw):
        shortest = None
        for a, b, kind in candidates[:SEARCH_LIMIT]:
            back = _path(graph, member_set, b, a, allowed, need_rw)
            if back is not None and (shortest is None or len(back) + 1 < len(shortest)):
                shortest = [(a, b, kind)] + back
        return shortest

    ww = [(a, b, "ww") for a, b in edges_of("ww")]
    wr = [(a, b, "wr") for a, b in edges_of("wr")]
    rw = [(a, b, "rw") for a, b in edges_of("rw")]
    cycle = best(ww, {"ww"}, False)
    if cycle:
        found["G0"] = cycle
    cycle = best(wr, {"ww", "wr"}, False)
    if cycle:
        found["G1c"] = cycle
    cycle = best(rw, {"ww", "wr"}, False)
    if cycle:
        found["G-single"] = cycle
    cycle = best(rw, {"ww", "wr", "rw"}, True)
    if cycle:
        found["G2-item"] = cycle
    return found


def explain(graph, cycle, txns):
    """Elle-style text: each step T_a < T_b with its reason, then the contradiction."""
    names = {}
    for a, _, _ in cycle:
        names.setdefault(a, f"T{len(names) + 1}")
    lines = ["Let:"]
    for txn_id, name in names.items():
        txn = txns[txn_id]
        lines.append(f"  {name} = {{id {txn.id}, process {txn.process}, {txn.type}, value {txn.value}}}")
    lines.append("Then:")
    for position, (a, b, kind) in enumerate(cycle):
        why = graph.edges[a][b][kind]
        prefix = "However, " if position == len(cycle) - 1 else ""
        reason = {
            "ww": "{a} appended {v} to {k} and {b} appended {w} after it",
            "wr": "{b} observed {a}'s append of {v} to {k}",
            "rw": "{a} did not observe {b}'s append of {w} to {k}",
        }[kind].format(a=names[a], b=names[b], **why)
        tail = ": a contradiction!" if position == len(cycle) - 1 else "."
        lines.append(f"  - {prefix}{names[a]} < {names[b]}, because {reason}{tail}")
    return "\n".join(lines)


def _classify(cycle):
    rw = sum(1 for _, _, kind in cycle if kind == "rw")
    wr = sum(1 for _, _, kind in cycle if kind == "wr")
    if rw >= 2:
        return "G2-item"
    if rw == 1:
        return "G-single"
    return "G1c" if wr else "G0"


def analyze(txns):
    """Infer dependencies and anomalies. Returns (anomalies, graph, by_id)."""
    by_id = {}
    for txn in txns:
        if txn.id in by_id:
            raise ValueError(f"duplicate transaction id {txn.id!r}")
        by_id[txn.id] = txn
    anomalies = collections.defaultdict(list)

    writer, last_append = {}, collections.defaultdict(dict)
    for txn in txns:
        for f, k, v in txn.value:
            if f != "append":
                continue
            if (k, v) in writer:
                raise ValueError(f"value {v!r} appended to key {k!r} twice ({writer[(k, v)]} and {txn.id})")
            writer[(k, v)] = txn.id
            last_append[txn.id][k] = v

    # Internal consistency, and the committed reads that feed version orders.
    reads = []  # (txn id, key, list)
    for txn in txns:
        if txn.type != "ok":
            continue
        known, suffix = {}, collections.defaultdict(list)
        for position, (f, k, v) in enumerate(txn.value):
            if f == "append":
                if k in known:
                    known[k] = known[k] + [v]
                else:
                    suffix[k].append(v)
                continue
            if f != "r" or v is None:
                continue
            value = list(v)
            if k in known and value != known[k]:
                anomalies["internal"].append({"txn": txn.id, "op": position, "key": k, "expected": known[k], "read": value})
            elif k not in known and suffix[k] and value[-len(suffix[k]):] != suffix[k]:
                anomalies["internal"].append({"txn": txn.id, "op": position, "key": k,
                                              "expected-suffix": suffix[k], "read": value})
            known[k] = value
            if len(set(value)) != len(value):
                duplicates = sorted({x for x in value if value.count(x) > 1}, key=repr)
                anomalies["duplicate-elements"].append({"txn": txn.id, "key": k, "read": value, "duplicates": duplicates})
            reads.append((txn.id, k, value))

    # Version order per key: the longest committed read, which every read must prefix.
    order = {}
    for _, k, value in reads:
        order.setdefault(k, [])
        if len(value) > len(order[k]):
            order[k] = value
    compatible = []
    for txn_id, k, value in reads:
        if order[k][:len(value)] == value:
            compatible.append((txn_id, k, value))
        else:
            anomalies["incompatible-order"].append({"key": k, "txn": txn_id, "read": value, "longest": order[k]})

    observed = set()
    for txn_id, k, value in reads:
        for v in value:
            observed.add((k, v))
            source = writer.get((k, v))
            if source is None:
                anomalies["garbage"].append({"txn": txn_id, "key": k, "value": v})
            elif by_id[source].type == "fail":
                anomalies["G1a"].append({"reader": txn_id, "writer": source, "key": k, "value": v})
        # The last element another transaction wrote must be that writer's final append to the key.
        foreign = [v for v in value if writer.get((k, v)) not in (None, txn_id)]
        if foreign:
            v = foreign[-1]
            source = writer[(k, v)]
            if by_id[source].type != "fail" and last_append[source][k] != v:
                anomalies["G1b"].append({"reader": txn_id, "writer": source, "key": k, "value": v,
                                         "writer-final": last_append[source][k]})

    committed = {txn.id for txn in txns if txn.type == "ok"}
    committed |= {writer[kv] for kv in observed if kv in writer and by_id[writer[kv]].type == "info"}

    def committed_writer(k, v):
        source = writer.get((k, v))
        return source if source in committed else None

    graph = Graph()
    unobserved = collections.defaultdict(list)  # key -> committed appends no read saw
    for (k, v), source in writer.items():
        if source in committed and (k, v) not in observed:
            unobserved[k].append((v, source))
    for k in set(order) | set(unobserved):
        versions = [(v, committed_writer(k, v)) for v in order.get(k, [])]
        versions = [(v, w) for v, w in versions if w is not None]
        for (v, a), (w, b) in zip(versions, versions[1:]):
            graph.add(a, b, "ww", {"k": k, "v": v, "w": w})
        if versions:
            v, a = versions[-1]
            for w, b in unobserved.get(k, []):
                graph.add(a, b, "ww", {"k": k, "v": v, "w": w})

    for reader, k, value in compatible:
        mine = {v for f, kk, v in by_id[reader].value if f == "append" and kk == k}
        for v in reversed(value):
            if v in mine:
                continue
            source = committed_writer(k, v)
            if source is not None:
                graph.add(source, reader, "wr", {"k": k, "v": v})
                break
        rest = order[k][len(value):]
        for w in rest:
            source = committed_writer(k, w)
            if source is None:
                continue  # an aborted append is no version
            if source != reader:
                graph.add(reader, source, "rw", {"k": k, "w": w})
            break
        for w, source in unobserved.get(k, []):
            if source != reader:
                graph.add(reader, source, "rw", {"k": k, "w": w})

    for members in components(graph):
        for kind, cycle in _cycles(graph, members).items():
            assert _classify(cycle) == kind, (kind, cycle)
            anomalies[kind].append({"cycle": [a for a, _, _ in cycle],
                                    "steps": [{"from": a, "to": b, "type": t, **graph.edges[a][b][t]} for a, b, t in cycle],
                                    "explanation": explain(graph, cycle, by_id), "component-size": len(members)})
    return dict(anomalies), graph, by_id


def check(txns, model="serializable"):
    """Check a history against ``model``; returns a JSON-able result."""
    if model not in ALLOWED:
        raise ValueError(f"unknown consistency model {model!r}; expected one of {sorted(ALLOWED)}")
    anomalies, graph, by_id = analyze(txns)
    types = collections.Counter(txn.type for txn in txns)
    illegal = sorted(kind for kind in anomalies if kind not in ALLOWED[model])
    return {
        "valid": not illegal,
        "model": model,
        "anomaly-types": sorted(anomalies),
        "illegal": illegal,
        "counts": {kind: len(items) for kind, items in sorted(anomalies.items())},
        "anomalies": anomalies,
        "txns": dict(types),
        "edges": graph.count(),
    }
