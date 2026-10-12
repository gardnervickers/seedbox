import unittest
from check_parked_publication import check_application_return


def evidence():
    lines = ['BEGIN case=application-return iterations=20 outer=0x1000 poll=0x1008 '
             'read=0x1080 edit=0x1040 x28=0x2000']
    for i in range(1, 21):
        lines += [f'KERNEL_READ iteration={i} syscall=63 pc=0x300c sp=0x4000 fd=3'] * 2
        lines += [f'RETURN_PUBLICATION iteration={i} outer_pc=0x1008 edited_pc=0x1040 '
                  f'read_pc=0x1084 read_effects=0 done={i} latency_us=42',
                  f'ITERATION case=application-return iteration={i} faults=1 nested=1 '
                  f'reads=1 effects=1 edited={int(i % 2 == 0)} '
                  f'outer_body={(0x600 if i % 2 else 0x800) + i} read_body={0x700+i} '
                  'all_gpr=31 all_q=32 bad=0']
    lines += ['END case=application-return completed=20 faults=20 nested=20 reads=20 edits=10 bad=0']
    return '\n'.join(lines)


class ParkedApplicationReturnEvidence(unittest.TestCase):
    def test_complete(self):
        self.assertEqual(check_application_return(evidence(), 0)['kernel_read_pc'], 0x300c)

    def test_missing_kernel_read_or_restart(self):
        line = evidence().splitlines()[1]
        with self.assertRaises(ValueError):
            check_application_return(evidence().replace(line, '', 1), 0)
        with self.assertRaises(ValueError):
            check_application_return(evidence().replace('pc=0x300c', 'pc=0x3008', 1), 0)

    def test_missing_fault_nested_signal_wrong_edit_or_duplicate_effect(self):
        for before, after in [('faults=1', 'faults=0'), ('nested=1', 'nested=0'),
                              ('effects=1', 'effects=2'), ('edited=1', 'edited=0'),
                              ('outer_body=2050', 'outer_body=1538'),
                              ('read_effects=0', 'read_effects=1'),
                              ('all_gpr=31', 'all_gpr=30')]:
            with self.assertRaises(ValueError):
                check_application_return(evidence().replace(before, after), 0)

    def test_success_does_not_hide_crash(self):
        with self.assertRaises(ValueError): check_application_return(evidence(), -11)
        with self.assertRaises(ValueError): check_application_return(evidence() + '\nFAIL', 0)


class GeneratedSyscallEvidence(unittest.TestCase):
    def test_actual_do_syscall_return_pc(self):
        import tempfile
        from pathlib import Path
        from run_parked_publication import gencode_read_proof
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'gencode-shared.test'
            path.write_text('do_syscall:\n  0x3008 d4000001 svc $0\n  0x300c f9000380 str x0\ndo_clone_syscall:\n')
            self.assertEqual(gencode_read_proof(Path(directory), 0x300c)['svc'], '0x3008')
            with self.assertRaises(ValueError): gencode_read_proof(Path(directory), 0x3008)
            path.write_text('unrelated_syscall:\n  0x3008 d4000001 svc $0\n')
            with self.assertRaises(ValueError): gencode_read_proof(Path(directory), 0x300c)


if __name__ == '__main__':
    unittest.main()
