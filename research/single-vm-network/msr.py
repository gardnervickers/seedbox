#!/usr/bin/env python3
"""Private snapshot and best-effort all-core restoration for the assigned pair."""
import json,os,struct,sys
cmd,path,cpustr=sys.argv[1:];cpus=list(map(int,cpustr.split(',')))
assert cpus==[14,30], 'only allocated CPUs 14,30 authorized'
msr=0xc0011020;bit=1<<54
def read(c):
 f=os.open(f'/dev/cpu/{c}/msr',os.O_RDONLY)
 try:return struct.unpack('<Q',os.pread(f,8,msr))[0]
 finally:os.close(f)
def write(c,v):
 f=os.open(f'/dev/cpu/{c}/msr',os.O_WRONLY)
 try:
  if os.pwrite(f,struct.pack('<Q',v),msr)!=8:raise OSError('short MSR write')
 finally:os.close(f)
if cmd=='set':
 before={str(c):read(c)for c in cpus}
 with open(path,'x')as f:json.dump(before,f)
 for c in cpus:write(c,before[str(c)]|bit)
 after={str(c):read(c)for c in cpus};assert all(v&bit for v in after.values())
 print(json.dumps({'before':before,'set':after}))
elif cmd=='show':
 print(json.dumps({'current':{str(c):read(c)for c in cpus}}))
elif cmd=='restore':
 with open(path)as f:before=json.load(f)
 assert set(before)==set(map(str,cpus))
 errors=[];after={}
 for c in cpus:
  try:write(c,before[str(c)])
  except OSError as e:errors.append(f'write cpu{c}: {e}')
 for c in cpus:
  try:after[str(c)]=read(c)
  except OSError as e:errors.append(f'read cpu{c}: {e}')
 if after!=before:errors.append('restored readback differs from snapshot')
 print(json.dumps({'restored':after,'errors':errors,'snapshot':path}),flush=True)
 if errors:raise SystemExit(1)
 os.unlink(path)
else:raise SystemExit('set/show/restore required')
