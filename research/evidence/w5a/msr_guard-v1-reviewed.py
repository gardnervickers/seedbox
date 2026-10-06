#!/usr/bin/env python3
"""Narrow root-only research guard. No module loading, sysctls or mitigation changes.
Fixed MSR/CPUs. Retains an exclusive shared measurement lock for the child.
Child must be the fixed owned probe executable. SIGTERM/INT/HUP restore via finally.
SIGKILL/power loss cannot run cleanup; an external restore receipt is retained.
"""
import argparse,fcntl,hashlib,json,os,pathlib,signal,subprocess,sys,time
ROOT=pathlib.Path('/tmp/seedbox-round-1.S74xJ4')
MSR=0xc0011020
BIT=1<<54
CPUS=(2,18)
PROBE=ROOT/'amd-counter/target/debug/preempt'
parser=argparse.ArgumentParser()
parser.add_argument('--workaround',choices=('original','enabled'),required=True)
parser.add_argument('--timeout',type=float,default=30)
parser.add_argument('--receipt',required=True)
parser.add_argument('--probe-sha256',required=True)
parser.add_argument('probe_args',nargs=argparse.REMAINDER)
a=parser.parse_args()
receipt=pathlib.Path(a.receipt)
if receipt.parent.resolve()!= (ROOT/'evidence/w5a').resolve():raise SystemExit('receipt must be directly inside owned evidence/w5a')
if os.geteuid()!=0:raise SystemExit('requires root only for host perf all-guest access and guarded MSR IO')
if not PROBE.is_file():raise SystemExit('fixed owned probe missing')
if not (0<a.timeout<=120):raise SystemExit('timeout out of scope')
probe_bytes=PROBE.read_bytes()
if hashlib.sha256(probe_bytes).hexdigest()!=a.probe_sha256:raise SystemExit('reviewed probe SHA256 mismatch')
# Immutable in-memory executable snapshot prevents path replacement after review.
probe_fd=os.memfd_create('seedbox-reviewed-preempt',os.MFD_ALLOW_SEALING)
written=0
while written<len(probe_bytes):written+=os.write(probe_fd,probe_bytes[written:])
fcntl.fcntl(probe_fd,fcntl.F_ADD_SEALS,fcntl.F_SEAL_WRITE|fcntl.F_SEAL_GROW|fcntl.F_SEAL_SHRINK|fcntl.F_SEAL_SEAL)
receipt_fd=os.open(receipt,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o644)
lock=os.open(ROOT/'measurement.lock',os.O_CREAT|os.O_RDWR,0o600)
try:fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
except BlockingIOError:raise SystemExit(75)
fds={};original={};record={'msr':hex(MSR),'bit':54,'cpus':list(CPUS),'mode':a.workaround,'reviewed_probe_sha256':a.probe_sha256,'child_args':a.probe_args,'start_ns':time.time_ns(),'original':{},'applied':{},'restored':{},'errors':[]}
child=None
class Interrupted(Exception):pass
def interrupt(signum,frame):raise Interrupted('guard received signal '+str(signum))
for s in (signal.SIGTERM,signal.SIGINT,signal.SIGHUP):signal.signal(s,interrupt)
def read(cpu):
 data=os.pread(fds[cpu],8,MSR)
 if len(data)!=8:raise RuntimeError('short MSR read CPU'+str(cpu))
 return int.from_bytes(data,'little')
def write(cpu,value):
 if os.pwrite(fds[cpu],value.to_bytes(8,'little'),MSR)!=8:raise RuntimeError('short MSR write')
def save():
 data=(json.dumps(record,indent=2)+'\n').encode()
 os.lseek(receipt_fd,0,os.SEEK_SET);os.ftruncate(receipt_fd,0)
 written=0
 while written<len(data):written+=os.write(receipt_fd,data[written:])
 os.fsync(receipt_fd)
 directory_fd=os.open(receipt.parent,os.O_RDONLY|os.O_DIRECTORY)
 try:os.fsync(directory_fd)
 finally:os.close(directory_fd)
try:
 # Open all first, save all originals before any mutation. No modprobe.
 for cpu in CPUS:fds[cpu]=os.open('/dev/cpu/'+str(cpu)+'/msr',(os.O_RDWR if a.workaround=='enabled' else os.O_RDONLY))
 for cpu in CPUS:original[cpu]=read(cpu);record['original'][str(cpu)]=hex(original[cpu])
 save()
 if a.workaround=='enabled':
  for cpu in CPUS:
   write(cpu,original[cpu]|BIT)
  for cpu in CPUS:
   actual=read(cpu);record['applied'][str(cpu)]=hex(actual)
   if actual!=original[cpu]|BIT:raise RuntimeError('workaround readback mismatch CPU'+str(cpu))
  save()
 # Fixed CPU affinity inherited by caller-owned vCPU thread; child-only root.
 child=subprocess.Popen(['/proc/self/fd/'+str(probe_fd),*a.probe_args],pass_fds=(probe_fd,),start_new_session=True,preexec_fn=lambda:os.sched_setaffinity(0,{2}))
 try:record['child_exit']=child.wait(timeout=a.timeout)
 except subprocess.TimeoutExpired:
  record['timed_out']=True;os.killpg(child.pid,signal.SIGTERM)
  try:child.wait(timeout=3)
  except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait()
  record['child_exit']=child.returncode
except BaseException as e:
 record['errors'].append(str(e))
finally:
 # Finish the child before touching MSRs. Prevent a second ordinary stop signal
 # from interrupting restoration; SIGKILL and host failure remain external risks.
 for s in (signal.SIGTERM,signal.SIGINT,signal.SIGHUP):signal.signal(s,signal.SIG_IGN)
 try:
  if child and child.poll() is None:
   try:os.killpg(child.pid,signal.SIGTERM)
   except ProcessLookupError:pass
   try:child.wait(timeout=3)
   except subprocess.TimeoutExpired:
    try:os.killpg(child.pid,signal.SIGKILL)
    except ProcessLookupError:pass
    child.wait(timeout=3)
 except BaseException as e:record['errors'].append('child termination: '+str(e))
 if a.workaround=='enabled':
  for cpu,value in original.items():
   try:
    current=read(cpu)
    if (current&~BIT)!=(value&~BIT):record['errors'].append('unrelated MSR bits changed CPU'+str(cpu)+': '+hex(current))
    write(cpu,value)
   except BaseException as e:record['errors'].append('restore write CPU'+str(cpu)+': '+str(e))
 for cpu,value in original.items():
  try:
   actual=read(cpu);record['restored'][str(cpu)]=hex(actual)
   if actual!=value:record['errors'].append('restore verify CPU'+str(cpu)+': '+hex(actual))
  except BaseException as e:record['errors'].append('restore read CPU'+str(cpu)+': '+str(e))
 record['end_ns']=time.time_ns();save()
 for fd in fds.values():os.close(fd)
 os.close(lock);os.close(probe_fd);os.close(receipt_fd)
print(json.dumps({'guard_receipt':str(receipt),'guard_errors':record['errors'],'child_exit':record.get('child_exit'),'timed_out':record.get('timed_out',False)}),file=sys.stderr)
raise SystemExit(1 if record['errors'] or record.get('timed_out') else record.get('child_exit',1))
