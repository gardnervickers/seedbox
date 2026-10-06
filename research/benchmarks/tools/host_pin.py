#!/usr/bin/env python3
"""Read-only future host input capture. Do not execute during W5a's priority.
No sysctl/MSR/kernel/service changes. Pin before/after the actual benchmark.
"""
import argparse, hashlib, json, pathlib, platform, subprocess

def read(path):
    try:return pathlib.Path(path).read_text().strip()
    except OSError:return None

def capture(counter_profile):
    if platform.system()!='Linux':raise RuntimeError('actual benchmark host pin requiresLinux; local Darwin smoke is not thatpin')
    cpus={}
    for block in pathlib.Path('/proc/cpuinfo').read_text().strip().split('\n\n'):
        values=dict(line.split(':',1) for line in block.splitlines() if ':' in line)
        values={k.strip():v.strip() for k,v in values.items()}
        cpus[values.get('processor','unknown')]={k:values.get(k) for k in ('vendor_id','cpu family','model','model name','stepping','microcode','flags')}
    notes=pathlib.Path('/sys/kernel/notes')
    topology=json.loads(subprocess.check_output(['lscpu','--json'],text=True))
    # lscpu's live scaling/frequency fields are diagnostics, not immutableCPU pins.
    topology['lscpu']=[row for row in topology['lscpu'] if 'mhz' not in row['field'].lower() and 'bogomips' not in row['field'].lower()]
    return {'qualification':'read-only captured host input;counter qualification supplied separately',
            'kernel_release':platform.release(),'kernel_version':platform.version(),
            'kernel_notes_sha256':hashlib.sha256(notes.read_bytes()).hexdigest() if notes.exists() else None,
            'lscpu_stable_topology':topology,
            'cpu_inputs':cpus,'perf_event_paranoid':read('/proc/sys/kernel/perf_event_paranoid'),
            'counter_profile_id':counter_profile}

def main():
    ap=argparse.ArgumentParser();ap.add_argument('--counter-profile',required=True);ap.add_argument('--output',type=pathlib.Path,required=True);ap.add_argument('--compare',type=pathlib.Path)
    args=ap.parse_args();pin=capture(args.counter_profile)
    args.output.write_text(json.dumps(pin,indent=2)+'\n')
    if args.compare and pin!=json.loads(args.compare.read_text()):raise RuntimeError('host kernel/CPU/microcode/counter inputs changed; rejectrun')
    print(json.dumps({'qualification':'captured','before_after_match':True if args.compare else None,'output':str(args.output)}))

if __name__=='__main__':main()
