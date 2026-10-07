#!/usr/bin/env python3
"""Check this application's opaque receipts and Seedbox's retained verdict."""
import argparse
import json
from pathlib import Path

A = b"acknowledged-value-A".hex()
B = b"conflicting-value-B".hex()

class EvidenceError(ValueError):
    pass

def require(condition, message):
    if not condition:
        raise EvidenceError(message)

def receipts(console, kind):
    return [json.loads(line[len(kind) + 1:]) for line in console.splitlines()
            if line.startswith(kind + " ")]

def check_console(console, mode):
    require("qualification/setup error:" not in console, "application setup/qualification failed")
    results = receipts(console, "MINIO_RESULT")
    require(len(results) == 1, "missing/ambiguous application result")
    result = results[0]
    require(result.get("mode") == mode and result.get("passed") is (mode != "broken"),
            "wrong application verdict/mode")
    http = receipts(console, "MINIO_HTTP")
    setup_end = next((i for i, r in enumerate(http) if r['operation'] == 'create_bucket'), None)
    require(setup_end is not None and 1 <= setup_end <= 8, "readiness/bucket setup not qualified")
    ready = http[:setup_end]
    require(all(r['operation'] == 'list_buckets' for r in ready) and
            all(r['status'] in (500, 503) for r in ready[:-1]) and ready[-1]['status'] == 200 and
            http[setup_end]['status'] == 200, "readiness/bucket setup not qualified")
    require(all(not r['condition'] and r['key'] is None for r in http[:setup_end + 1]), "bad setup receipts")
    http = http[setup_end + 1:]
    gets = receipts(console, "MINIO_GET")
    require(all(isinstance(r.get('etag'), str) and r['etag'] for r in gets), "missing GET ETag")
    puts = [r for r in http if r['operation'] == 'put_object']
    require(all(isinstance(r.get('etag'), str) and r['etag'] for r in puts if r['status'] == 200),
            "missing successful PUT ETag")
    if mode == 'qualification':
        require(len(http) == 13 and len(puts) == 7, "incomplete CAS matrix")
        require([r['value_hex'] for r in puts[:5]] == [A, B, B, A, A], "wrong matrix write bytes")
        require(sorted(r['value_hex'] for r in puts[5:]) == sorted([A, B]), "wrong concurrent contenders")
        original, updated = puts[0]['etag'], puts[2]['etag']
        require(original != updated, "ETag did not change")
        expected = [(200, 'matrix', {'IfNoneMatch': '*'}),
                    (412, 'matrix', {'IfNoneMatch': '*'}),
                    (200, 'matrix', {'IfMatch': original}),
                    (412, 'matrix', {'IfMatch': original}),
                    (404, 'missing', {'IfMatch': original})]
        require([(r['status'], r['key'], r['condition']) for r in puts[:5]] == expected,
                "wrong conditional-write matrix")
        require(sorted(r['status'] for r in puts[5:]) == [200, 412] and
                all(r['key'] == 'concurrent' and r['condition'] == {'IfNoneMatch': '*'} for r in puts[5:]),
                "wrong concurrent-create outcomes")
        require([(r['key'], r['value_hex']) for r in gets[:4]] ==
                [('matrix', A), ('matrix', A), ('matrix', B), ('matrix', B)], "matrix bytes changed")
        require([r['etag'] for r in gets[:4]] == [original, original, updated, updated], "matrix ETag mismatch")
        require(len(gets) == 5 and gets[-1]['key'] == 'concurrent' and gets[-1]['value_hex'] in (A, B),
                "missing concurrent winner GET")
        winner = next(r for r in puts[5:] if r['status'] == 200)
        require(gets[-1]['etag'] == winner['etag'], "stored concurrent ETag is not winner")
        require(gets[-1]['value_hex'] == winner['value_hex'], "stored bytes are not acknowledged winner")
        reads = [r for r in http if r['operation'] == 'get_object']
        require([(r['key'], r['status']) for r in reads] ==
                [('matrix', 200)] * 4 + [('missing', 404), ('concurrent', 200)], "missing-key/read matrix wrong")
        require(all(not r['condition'] for r in reads), "unexpected GET condition")
    else:
        expected = [(200, {'IfNoneMatch': '*'}), (412, {'IfNoneMatch': '*'})]
        if mode == 'broken':
            expected.append((200, {}))
        require([(r['status'], r['condition']) for r in puts] == expected and
                all(r['key'] == 'slot' for r in puts), "planted conflict/overwrite not demonstrated")
        require([r['value_hex'] for r in puts] == ([A, B, B] if mode == 'broken' else [A, B]),
                "wrong planted write bytes")
        stored = B if mode == 'broken' else A
        require(len(http) == len(puts) + 1 and http[-1]['operation'] == 'get_object' and
                http[-1]['key'] == 'slot' and http[-1]['status'] == 200 and not http[-1]['condition'],
                "missing independent stored-value read")
        require(len(gets) == 1 and gets[0]['key'] == 'slot' and gets[0]['value_hex'] == stored,
                "wrong stored slot bytes")
        require(gets[0]['etag'] == puts[-1 if mode == 'broken' else 0]['etag'], "stored slot ETag mismatch")
        require(result.get('acknowledged_hex') == A and result.get('stored_hex') == stored,
                "acknowledgement invariant not demonstrated")
    return result

def verify(run, mode, replay=False):
    run = Path(run)
    record = json.loads((run / 'manifest.json').read_text())['record']
    expected = 'FAIL' if mode == 'broken' else 'PASS'
    require(record['outcome'] == expected and record.get('error') is None, "wrong retained outcome/error")
    require(not record.get('omitted_logs'), "omitted application evidence")
    report = record['report']
    require(report and report['reason'] == 'COMPLETED' and report['witness'], "incomplete/no witness")
    require(report['workload_exit'] == {'service': 'client', 'code': 1 if mode == 'broken' else 0},
            "setup exit must not count as planted FAIL")
    inputs = record['identity']['inputs']
    require(inputs['profile']['semantic_parameters']['memory_mb'] == '2048', "wrong trial memory profile")
    require(inputs['placement'] == {'client': 'client', 'minio': 'minio'}, "expected one VM per service")
    world = json.loads((run / 'output/vmm.json').read_text())
    require({m['name'] for m in world['machines']} == {'client', 'minio'}, "wrong world machines")
    require(not world['faults'] and not world.get('fault_error') and not world.get('rejected_fault_effects'),
            "unexpected fault/control error")
    comparison = record.get('replay')
    if replay or comparison is not None:
        require(record.get('replay_of') and comparison and comparison['status'] == 'MATCH' and
                comparison['outcome_matches'] and comparison['witness_matches'] is True and
                comparison['fault_history_matches'] and comparison.get('full_state_matches') is not False,
                "retained replay did not MATCH")
    result = check_console((run / 'output/machines/client.console').read_text(), mode)
    return dict(run=str(run), outcome=expected, replay=comparison, result=result, witness=report['witness'])

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('run', type=Path)
    parser.add_argument('--mode', required=True, choices=('qualification', 'broken', 'fixed'))
    parser.add_argument('--replay', action='store_true')
    args = parser.parse_args()
    print(json.dumps(verify(args.run, args.mode, args.replay), indent=2))
