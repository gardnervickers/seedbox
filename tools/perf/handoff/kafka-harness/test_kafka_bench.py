import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('bench', Path(__file__).with_name('kafka-bench.py'))
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


class LogAnalysisTest(unittest.TestCase):
    def test_fragmented_milestone_streams_and_protocol_counts(self):
        rows = []
        def chunk(text, sim=12, received=2000000000, stream='stdout'):
            rows.append({'receivedUnixNs': received, 'record': {'type': 'entry', 'entry': {
                'type': 'bytes', 'machine': 'k1', 'incarnation': 0, 'stream': stream,
                'simulatedTimeNs': sim, 'bytes': list(text.encode())}}})
        chunk('Registered `kafka:type=kafka.Log4jController')
        chunk('other stream\n', stream='stderr')
        chunk('` MBean\nStarting controller\n', sim=24)
        chunk('request.timeout.ms = 2000\n')
        chunk('[RaftManager id=1] Completed transition to Leader(epoch=1)\n')
        chunk('[RaftManager id=1] Completed transition to FollowerState(epoch=2)\n')
        chunk('Disconnecting from node 2 due to request timeout.\n')
        chunk('A fatal error has been detected by the Java Runtime Environment:\n')
        chunk('SIGSEGV at pc=0')
        rows.append({'receivedUnixNs': 3, 'record': {'type': 'closed'}})
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'logs.jsonl'
            path.write_text(''.join(json.dumps(r)+'\n' for r in rows))
            result = bench.analyze_logs(path, 1000000000)
        self.assertEqual(result['milestones']['mbean']['simulatedTimeNs'], 24)
        self.assertEqual(result['milestones']['mbean']['receiptWallSeconds'], 1)
        self.assertEqual(result['requestTimeoutLines'], 1)
        self.assertEqual(len(result['leaderTransitions']), 1)
        self.assertEqual(len(result['kraftTransitions']), 2)
        self.assertEqual(len(result['fatalErrors']), 2)
        self.assertEqual(result['unterminatedText'], ['SIGSEGV at pc=0'])
        self.assertEqual(result['omissions'], [])

    def test_omission_preserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'logs.jsonl'
            records = [{'type': 'gap', 'first': 1, 'last': 4},
                       {'type': 'entry', 'sequence': 5, 'entry': {'type': 'machineGap', 'records': 3}}]
            path.write_text(''.join(json.dumps({'receivedUnixNs': 0, 'record': r})+'\n' for r in records))
            result = bench.analyze_logs(path, 0)
            self.assertEqual(result['omissions'], records)


if __name__ == '__main__':
    unittest.main()
