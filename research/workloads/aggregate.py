#!/usr/bin/env python3
import sys, pathlib, re, json, collections, csv, hashlib

directory=pathlib.Path(sys.argv[1])
groups={
    'redis':['redis-primary','redis-replica','redis-recovered'],
    'etcd':['etcd-server','etcd-recovered'],
    'postgres':['postgres-primary','postgres-basebackup','postgres-replica','postgres-recovered'],
    'tokio-current':['tokio-current'], 'tokio-multi':['tokio-multi'],
}
historical_names={
    'read','write','open','close','stat','fstat','lstat','poll','lseek','mmap','mprotect','munmap','brk',
    'rt_sigaction','rt_sigprocmask','ioctl','readv','writev','access','pipe','sched_yield','nanosleep',
    'getitimer','setitimer','getpid','socket','connect','accept','sendto','recvfrom','bind','listen',
    'getsockname','getpeername','setsockopt','getsockopt','clone','exit','uname','fcntl','fsync','getcwd','chdir',
    'umask','gettimeofday','getrlimit','getrusage','sysinfo','getuid','getgid','geteuid','getegid','getppid',
    'prctl','arch_prctl','gettid','time','futex','sched_getaffinity','set_tid_address','clock_gettime','clock_getres',
    'exit_group','epoll_wait','epoll_ctl','openat','newfstatat','epoll_pwait','accept4','epoll_create','epoll_create1',
    'pipe2','prlimit64','getrandom',
}
# Names present in the archived source only indicate dispatch, never conformance.
# execve is the outer native launch; static ELFs were loaded by the prototype host.
def score(name):
    if name=='execve':return 'native-launch-or-process-exec; prototype host loads static ELF, no general exec syscall'
    if name in ('rt_sigaction','rt_sigprocmask','setitimer','getitimer'):
        return 'partial: signal metadata/disabled interval timers; active delivery absent'
    if name=='mmap':return 'partial: anonymous only; file-backed/shared-file mappings absent'
    if name=='clone':return 'partial: limited shared-address-space thread flags; fork semantics absent'
    if name in ('epoll_ctl','epoll_pwait','epoll_wait','setsockopt','getsockopt','fcntl','prctl','futex','open','openat','clock_gettime'):
        return 'partial: restricted flags/options/clocks; consult archived implementation'
    if name=='fsync':return 'partial: volatile-byte copy to durable image; crash/torn-write fidelity absent'
    if name in historical_names:return 'modeled-dispatch-present; exercised flags/semantics not certified'
    return 'not modeled by historical ABI; native failure may be an optional fallback probe'

line_re=re.compile(r'^\d+\.\d+\s+\[\s*(\d+)\]\s+(\w+)\(')
flag_re=re.compile(r'\b(?:MAP_|PROT_|CLONE_|SIG[A-Z]|SA_|EPOLL|FUTEX_|O_|F_|SO_|SOL_|TCP_|MSG_|GRND_|MADV_|PR_|ARCH_|RWF_|IORING_|CLOCK_|AF_|SOCK_|AT_|EFD_|SFD_|SHM_|IPC_)[A-Z0-9_]*\b')
matrix={};rows=[]
results=json.loads((directory/'results.json').read_text())
samples=json.loads((directory/'process-samples.json').read_text())
meta=json.loads((directory/'metadata.json').read_text())
for workload,prefixes in groups.items():
    calls={};signals=collections.Counter();signal_examples={};files=[];feature_examples=collections.defaultdict(list)
    for prefix in prefixes:
        for f in sorted(directory.glob(prefix+'.*')):
            if not f.suffix[1:].isdigit():continue
            files.append(f.name)
            for n,line in enumerate(f.open(errors='replace'),1):
                m=line_re.match(line)
                if m:
                    number,name=m.groups();item=calls.setdefault(name,{'number':int(number),'calls':0,'flags':collections.Counter(),'errors':collections.Counter(),'examples':[],'historical_abi':score(name)})
                    item['calls']+=1
                    for flag in sorted(set(flag_re.findall(line))):item['flags'][flag]+=1
                    error=re.search(r'= (?:-1|\?) (E\w+)',line)
                    if error:item['errors'][error.group(1)]+=1
                    if len(item['examples'])<3 and name!='execve':item['examples'].append({'file':f.name,'line':n,'text':line.strip()})
                    features=[]
                    if name=='mmap' and 'MAP_ANONYMOUS' not in line:features.append('file_backed_mmap')
                    if name=='mmap' and 'MAP_SHARED' in line:features.append('map_shared')
                    if name in ('fork','vfork') or (name in ('clone','clone3') and 'CLONE_VM' not in line):features.append('fork_process')
                    if name=='epoll_ctl' and 'EPOLLET' in line:features.append('edge_triggered_epoll')
                    if name.startswith('io_uring'):features.append('io_uring')
                    if name in ('shmget','shmat'):features.append('sysv_shm')
                    for feature in features:
                        if len(feature_examples[feature])<8:feature_examples[feature].append({'file':f.name,'line':n,'text':line.strip()})
                elif '--- SIG' in line:
                    sig=re.search(r'--- (SIG\w+)',line).group(1);signals[sig]+=1
                    signal_examples.setdefault(sig,{'file':f.name,'line':n,'text':line.strip()})
                # Return errors on resumed calls are counted in a separate counter
                # to avoid inventing a complete invocation from interleaved fragments.
                if '<... ' in line and ' resumed>' in line:
                    resumed=re.search(r'<\.\.\. (\w+) resumed>',line);err=re.search(r'= -1 (E\w+)',line)
                    if resumed and err:
                        item=calls.get(resumed.group(1))
                        if item:item['errors'][err.group(1)]+=1
    for name,item in sorted(calls.items(),key=lambda x:x[1]['number']):
        item['flags']=dict(item['flags']);item['errors']=dict(item['errors'])
        rows.append({'workload':workload,'number':item['number'],'name':name,'calls':item['calls'],
                     'flags':json.dumps(item['flags'],sort_keys=True),'errors':json.dumps(item['errors'],sort_keys=True),'historical_abi':item['historical_abi']})
    binary={'redis':'redis-server','etcd':'etcd','postgres':'postgres','tokio-current':'tokio-service','tokio-multi':'tokio-service'}[workload]
    elf=meta['binaries'][binary]['elf']
    matrix[workload]={
        'result':results[workload], 'unique_syscalls':len(calls),'syscall_invocations':sum(x['calls'] for x in calls.values()),
        'trace_files':files,'syscalls':calls,'signals':dict(signals),'signal_examples':signal_examples,
        'features':{feature:{'observed':bool(feature_examples[feature]),'examples':feature_examples[feature]}
                    for feature in ['file_backed_mmap','map_shared','fork_process','sysv_shm','edge_triggered_epoll','io_uring']},
        'dynamic_linking':{'pt_interp_observed':'INTERP' in elf,'dt_needed_observed':'(NEEDED)' in elf,'evidence':'metadata.json binaries.'+binary+'.elf'},
        'process_samples':{p:samples.get(p) for p in prefixes},
        'unknowns':['strace cannot locate/quantify workload vDSO clock reads, direct TSC/RNG instructions, or syscall-free spinning',
                    'No packet-loss retransmission/congestion-control experiment; TCP reconnect only for Redis/Postgres',
                    'No general syscall conformance or simulator acceptance inferred from dispatch/names']}
output=directory/'matrix.json';output.write_text(json.dumps({'schema':1,'native_host':meta['versions'],'workloads':matrix,
    'analysis_scope':'startup plus finite exercised workload; clients mostly untraced; Postgres initdb not traced; taskset one allowed CPU; trace overhead changes schedules',
    'io_uring_note':'not observed in these paths; not a claim of unsupported or unnecessary io_uring',
    'signals_note':'includes harness shutdown SIGTERM and Postgres fast shutdown signals; examples retain si_code origin',
    'sampling_note':'10 ms /proc sample maxima are lower bounds; clone flags and trace file counts are primary evidence'},indent=2))
with (directory/'matrix.csv').open('w',newline='') as f:
    w=csv.DictWriter(f,fieldnames=['workload','number','name','calls','flags','errors','historical_abi']);w.writeheader();w.writerows(rows)
print(json.dumps({k:{'unique':v['unique_syscalls'],'invocations':v['syscall_invocations'],'signals':v['signals'],
    'features':{n:x['observed'] for n,x in v['features'].items()},'dynamic':v['dynamic_linking'],
    'syscalls_absent_from_historical_dispatch':[n for n in v['syscalls'] if n not in historical_names and n!='execve']} for k,v in matrix.items()},indent=2))
