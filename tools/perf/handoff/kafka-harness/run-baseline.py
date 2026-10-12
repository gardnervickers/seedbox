import json
import os
from pathlib import Path
import re
import subprocess as sp
import time

here = Path(__file__).resolve().parent
start = time.monotonic()
with (here/'idle-wait.jsonl').open('w') as f:
    while True:
        processes = sp.check_output(['ps', '-Ao', 'pid,pcpu,command'], text=True)
        servers = [l for l in processes.splitlines() if re.match(r'^\s*\d+\s+[\d.]+\s+\S*/seedbox serve(?: |$)', l)]
        load = os.getloadavg()
        elapsed = time.monotonic()-start
        f.write(json.dumps({'unixNs': time.time_ns(), 'waitSeconds': elapsed,
                            'load': load, 'servers': servers, 'processes': processes})+'\n')
        f.flush()
        if not servers and load[0] < 6:
            break
        if elapsed >= 1200:
            break
        time.sleep(30)
code = sp.call(['python3', str(here/'kafka-bench.py'),
                '--worktree', '/Users/gardnervickers/.codex/worktrees/arm-perf',
                '--state', '/tmp/seedbox-arm-perf-harness',
                '--output', str(here/'baseline-1009-w4'), '--seed', '1009',
                '--workers', '4', '--wall-time', '60m', '--replay'])
(here/'baseline.exit').write_text(str(code)+'\n')
raise SystemExit(code)
