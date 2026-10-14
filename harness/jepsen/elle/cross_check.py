"""Cross-check list_append.py against Jepsen's Elle (elle-cli) on recorded histories.

Each history is an oracle log of ``H {json}`` lines (see to_edn.py). For every
history and consistency model, this runs our checker, exports the history as
EDN, runs ``elle-cli --model list-append --consistency-models MODEL --output
json`` on it, and compares validity and anomaly types and counts. Elle runs
in a JDK container (elle-cli 0.1.11 needs Java 21 with the jdk.random module)
through podman in the dstreal Lima VM, pinned to two CPUs; ``--local`` runs
podman on this host instead.

Usage:
  python3 cross_check.py [--models serializable,...] [--elle-only strict-serializable]
      [--out RESULTS.json] [--markdown TABLE.md] [--list FILE] [LABEL=LOG ...]

A LABEL may end in ``@model,model`` to override ``--models`` for that history.
Elle searches only for the anomalies a model forbids, so the comparison counts
only those; anomalies our checker found that the model allows (G2-item under
snapshot isolation) are shown apart.
Elle names some cycles differently: ``G-single-item`` is our ``G-single``, and
``G-nonadjacent-item`` (several rw edges, none adjacent) is a kind of our
``G2-item``. Elle-only models (realtime orders such as strict-serializable,
which our checker does not infer) are reported without comparison.
"""
import argparse
import json
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import list_append  # noqa: E402
import to_edn  # noqa: E402

JAR_URL = "https://github.com/ligurio/elle-cli/releases/download/0.1.11/elle-cli-bin-0.1.11.zip"
ZIP_SHA256 = "9d3c72eaf3ccc4d0c7dc1da710264dee01f03af58c9fa177fec3f78ff5af1779"
JAR_SHA256 = "85c39a6247509075bf21762d4c8ed3a2cb3d2c9b2c169cb2c80dde25fa760388"
JAR = "elle-cli-0.1.11-standalone.jar"
IMAGE = "docker.io/library/eclipse-temurin:21-jdk@sha256:f9bf7e9fe2f9bc4657537c1a7e20c768f0c3bafca41598263be4714783dd62eb"
VM = ["ssh", "-F", str(Path.home() / ".lima/dstreal/ssh.config"), "lima-dstreal"]
REMOTE = "ellecheck"  # directory under the VM user's home
ELLE_NAMES = {"G-single-item": "G-single", "G-nonadjacent-item": "G2-item"}

# Fetch and verify the jar once, then check every EDN file under h/ for every model in models.txt.
SCRIPT = r"""
set -eu
cd ~/{remote}
if [ ! -f jar/{jar} ]; then
  mkdir -p dl jar
  curl -sSL -o dl/elle.zip {url}
  echo "{zip_sha}  dl/elle.zip" | sha256sum -c - >/dev/null
  python3 -c 'import zipfile; zipfile.ZipFile("dl/elle.zip").extract("target/{jar}", "dl")'
  mv dl/target/{jar} jar/{jar}
fi
echo "{jar_sha}  jar/{jar}" | sha256sum -c - >/dev/null
rm -rf out; mkdir -p out
podman run --rm --cpuset-cpus {cpus} -v $PWD/jar:/j:ro,Z -v $PWD/h:/h:ro,Z -v $PWD/out:/out:Z {image} \
  sh -c 'for model in $(cat /h/models.txt); do for f in /h/*.edn; do
           b=$(basename $f .edn)
           java -Xmx3g -jar /j/{jar} -m list-append -c $model -v json $f > /out/$b.$model.json 2> /out/$b.$model.err || true
         done; done'
"""


def run(command, **kwargs):
    return subprocess.run(command, check=True, **kwargs)


def elle(histories, models, local, cpus):
    """Run elle-cli on {name: edn path} for each model; returns {(name, model): result dict}."""
    with tempfile.TemporaryDirectory() as tmp:
        staging = Path(tmp) / "h"
        staging.mkdir()
        for name, path in histories.items():
            (staging / f"{name}.edn").write_bytes(Path(path).read_bytes())
        (staging / "models.txt").write_text("\n".join(models) + "\n")
        script = SCRIPT.format(remote=REMOTE, jar=JAR, url=JAR_URL, zip_sha=ZIP_SHA256, jar_sha=JAR_SHA256,
                               image=IMAGE, cpus=cpus)

        def sh(command, data=None):
            argv = ["bash", "-c", command] if local else VM + ["bash -c " + shlex.quote(command)]
            return subprocess.run(argv, check=True, input=data, capture_output=data is None).stdout

        archive = subprocess.run(["tar", "-C", tmp, "-cf", "-", "h"], check=True, capture_output=True).stdout
        sh(f"mkdir -p ~/{REMOTE} && rm -rf ~/{REMOTE}/h && tar -C ~/{REMOTE} -xf -", archive)
        sh(script)
        run(["tar", "-C", tmp, "-xf", "-"], input=sh(f"tar -C ~/{REMOTE} -cf - out"))
        results = {}
        for name in histories:
            for model in models:
                path = Path(tmp) / "out" / f"{name}.{model}.json"
                text = path.read_text() if path.exists() else ""
                try:
                    results[(name, model)] = json.loads(text)
                except ValueError:
                    error = (Path(tmp) / "out" / f"{name}.{model}.err")
                    results[(name, model)] = {"error": (error.read_text() if error.exists() else "")[-2000:]
                                              or text[-2000:]}
        return results


def summarize_elle(result):
    if "error" in result:
        return {"valid": "error", "counts": {}, "raw": [], "error": result["error"]}
    anomalies = result.get("anomalies") or {}
    counts = {}
    for kind, items in anomalies.items():
        mapped = ELLE_NAMES.get(kind, kind)
        counts[mapped] = counts.get(mapped, 0) + (len(items) if isinstance(items, list) else 1)
    return {"valid": result.get("valid?"), "counts": counts, "raw": sorted(result.get("anomaly-types") or [])}


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("histories", nargs="*", metavar="LABEL=LOG")
    parser.add_argument("--list", help="file of LABEL=LOG lines, added to the positional histories")
    parser.add_argument("--models", default="serializable")
    parser.add_argument("--elle-only", default="", help="models run on Elle only (no comparison)")
    parser.add_argument("--local", action="store_true", help="run podman on this host instead of the VM")
    parser.add_argument("--cpus", default="8,9")
    parser.add_argument("--out", help="write all results as JSON")
    parser.add_argument("--markdown", help="write the comparison table as Markdown")
    args = parser.parse_args()

    if args.list:
        args.histories += [line.strip() for line in open(args.list) if line.strip() and not line.startswith("#")]
    plans, edn = {}, {}
    work = Path(tempfile.mkdtemp(prefix="elle-edn-"))
    for item in args.histories:
        label, log = item.split("=", 1)
        models = args.models.split(",")
        if "@" in label:
            label, models = label.split("@", 1)
            models = models.split(",")
        plans[label] = (log, models)
        path = work / f"{label}.edn"
        path.write_text("".join(line + "\n" for line in to_edn.ops(to_edn.load(log))))
        edn[label] = path
    every = sorted({m for _, models in plans.values() for m in models} | set(filter(None, args.elle_only.split(","))))
    elle_results = elle(edn, every, args.local, args.cpus)

    rows = []
    for label, (log, models) in plans.items():
        txns = [list_append.Txn(t["i"], t["type"], t["value"], t["p"], t["t0"], t["t1"], t.get("error"))
                for t in to_edn.load(log)]
        for model in models:
            ours = list_append.check(txns, model=model)
            theirs = summarize_elle(elle_results[(label, model)])
            # Elle only searches for anomalies the model forbids; compare those, and show the rest apart.
            our_counts = {k: v for k, v in ours["counts"].items() if k in ours["illegal"]}
            allowed = {k: v for k, v in ours["counts"].items() if k not in ours["illegal"]}
            agree_valid = ours["valid"] == theirs["valid"]
            agree_types = sorted(our_counts) == sorted(theirs["counts"])
            rows.append({"history": label, "model": model, "txns": ours["txns"],
                         "ours": {"valid": ours["valid"], "counts": our_counts, "allowed": allowed},
                         "elle": theirs, "agree_valid": agree_valid, "agree_types": agree_types,
                         "agree_counts": our_counts == theirs["counts"]})
        for model in filter(None, args.elle_only.split(",")):
            rows.append({"history": label, "model": model, "txns": None, "ours": None,
                         "elle": summarize_elle(elle_results[(label, model)])})

    lines = ["| History | Model | Txns ok/fail/info | Ours | Elle | Agree |", "| --- | --- | --- | --- | --- | --- |"]

    def fmt(side):
        if side is None:
            return "(not checked)"
        counts = ", ".join(f"{k} {v}" for k, v in sorted(side["counts"].items())) or "none"
        allowed = ", ".join(f"{k} {v}" for k, v in sorted((side.get("allowed") or {}).items()))
        return f"{side['valid']}: {counts}" + (f" (allowed: {allowed})" if allowed else "")

    for row in rows:
        t = row["txns"] or {}
        txns = f"{t.get('ok', 0)}/{t.get('fail', 0)}/{t.get('info', 0)}" if row["txns"] else ""
        if row["ours"] is None:
            agree = "(Elle only)"
        elif row["agree_counts"]:
            agree = "yes"
        elif row["agree_valid"] and row["agree_types"]:
            agree = "verdict and types (counts differ)"
        elif row["agree_valid"]:
            agree = "verdict only"
        else:
            agree = "**NO**"
        lines.append(f"| {row['history']} | {row['model']} | {txns} | {fmt(row['ours'])} | {fmt(row['elle'])} | {agree} |")
    table = "\n".join(lines)
    print(table)
    if args.markdown:
        Path(args.markdown).write_text(table + "\n")
    if args.out:
        Path(args.out).write_text(json.dumps(rows, indent=1, default=str))
    compared = [r for r in rows if r["ours"] is not None]
    return 0 if all(r["agree_valid"] for r in compared) else 1


if __name__ == "__main__":
    sys.exit(main())
