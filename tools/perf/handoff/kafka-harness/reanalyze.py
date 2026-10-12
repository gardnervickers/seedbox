"""Reanalyze already-saved logs with the final parser; never rerun or retime guests."""
import importlib.util
import json
from pathlib import Path

here = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('bench', here/'kafka-bench.py')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
p = here/'baseline-1009-w4'
result = json.loads((p/'summary.json').read_text())
rows = ['# Guarded Kafka seed 1009 / 4 workers', '',
        '| Run | Verdict | Terminal sim s | Execution wall s | S/W |',
        '|---|---|---:|---:|---:|']
for label in ['baseline', 'replay']:
    summary = result[label]
    rows.append(f"| {label} | {summary['verdict']} | {summary['terminalSimulatedTimeNs']/1e9:.6f} | {summary['executionWallNs']/1e9:.6f} | {summary['simulatedOverWall']:.6f} |")
for label in ['baseline', 'replay']:
    summary = result[label]
    # The live run began before the harness gained an explicit start file.
    # Recover the same origin from its persisted receipt offset and timestamp.
    stamps = [s for log in summary['logs'].values() for s in log['milestones'].values()]
    assert stamps
    stamp = stamps[0]
    origin = stamp['receivedUnixNs'] - round(stamp['receiptWallSeconds']*1e9)
    bench.save(p/(label+'-start.json'), {'submissionStartUnixNs': origin,
               'reconstructedFromPersistedMilestone': True, 'roundingBoundNs': 256})
    summary['logs'] = {m: bench.analyze_logs(p/label/(m+'-received.jsonl'), origin)
                       for m in summary['logs']}
    bench.save(p/label/'summary.json', summary)
    rows += ['', f'## {label} milestones', '', '| Machine | Milestone | Sim s | Receipt wall s from submission |',
             '|---|---|---:|---:|']
    for machine, log in summary['logs'].items():
        for name, stamp in log['milestones'].items():
            rows.append(f"| {machine} | {name} | {stamp['simulatedTimeNs']/1e9:.6f} | {stamp['receiptWallSeconds']:.6f} |")
    rows += ['', 'Fatal matches: '+str(sum(len(l['fatalErrors']) for l in summary['logs'].values())),
             'Timeout log lines: '+str({m: l['requestTimeoutLines'] for m,l in summary['logs'].items()}),
             'Completed Raft transitions: '+str({m: len(l['kraftTransitions']) for m,l in summary['logs'].items()}),
             'Transitions into Leader: '+str({m: len(l['leaderTransitions']) for m,l in summary['logs'].items()}),
             'Log omissions: '+str(sum(len(l['omissions']) for l in summary['logs'].values())),
             'Scheduler: '+json.dumps(summary['scheduler'])]
bench.save(p/'summary.json', result)
rows += ['', 'Replay comparison: '+json.dumps(result['comparison']), '',
         'Reanalyzed saved logs with the final matcher (actual Kafka uses a backtick between Log4jController and MBean). No guest execution or wall metrics changed. Submission-origin reconstruction is within 256 ns of the original persisted offset.',
         'Host load and overlap: host-load.jsonl. This run is host-load-contaminated; do not treat it as an isolated performance comparison.']
(p/'summary.md').write_text('\n'.join(rows)+'\n')
