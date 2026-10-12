#!/usr/bin/env python3
"""Copy the retained corrected count client and add untimed JIT diagnostics.

Never modifies the reference bundle or builds DR. Compile this separate client
with the matching retained SDK, and load it ONLY for diagnostic runs. Analyze
callbacks distinguish actual emission from fault-context reconstruction.
"""
import argparse
from pathlib import Path
import shutil

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("count_client", type=Path)
p.add_argument("output", type=Path)
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=False)
for name in ["CMakeLists.txt", "budget.h"]:
    shutil.copyfile(a.count_client / name, a.output / name)
s = (a.count_client / "progress.c").read_text()
old = 'dr_fprintf(STDERR, "REF_PLAN %p %u\\n", tag, plan->length);'
assert s.count(old) == 1
s = s.replace(old, 'dr_fprintf(STDERR, "REF_PLAN %p %u translating=%d trace=%d\\n", tag, plan->length, translating, trace);')
at = "static void finished(void)"
assert s.count(at) == 1
s = s.replace(at, '''static void deleted(void *context, void *tag)
{
    dr_fprintf(STDERR, "REF_DELETE %p\\n", tag);
}
static bool filter_syscall(void *context, int number) { return number == 172; }
static bool before_syscall(void *context, int number)
{
    if (number == 172) {
        reg_t mark = dr_syscall_get_param(context, 0);
        if (mark == 0x4a495401 || mark == 0x4a495402)
            dr_fprintf(STDERR, "REF_PHASE %s payments=%llu left=%lld\\n",
                       mark == 0x4a495401 ? "begin" : "end",
                       diagnostic_payments, (long long)*budget());
    }
    return true;
}
''' + at)
at = "    DR_ASSERT(drmgr_init());"
assert s.count(at) == 1
s = s.replace(at, at + '''
    dr_register_delete_event(deleted);
    drmgr_register_filter_syscall_event(filter_syscall);
    drmgr_register_pre_syscall_event(before_syscall);
''')
(a.output / "progress.c").write_text(s)
