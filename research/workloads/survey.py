#!/usr/bin/env python3
"""Isolated native workload paths. Native observations are not deterministic acceptance."""
import os, sys, json, time, subprocess as sp, pathlib, hashlib, socket, signal, threading

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / 'runs/w4' / ('native.' + time.strftime('%Y%m%dT%H%M%S'))
OUT.mkdir(parents=True)
PROCS = []
RESULTS = {}
COMMANDS = []
STOP = threading.Event()
SAMPLES = {}
CPU = min(os.sched_getaffinity(0))
BASE_ENV = {k:os.environ[k] for k in ['PATH','HOME','LANG','LC_ALL','TZ','TMPDIR'] if k in os.environ}

def call(args, *, env=None, data=None, check=True, timeout=30):
    COMMANDS.append({'argv':list(map(str,args)), 'env_overrides':env or {}})
    r = sp.run(list(map(str,args)), input=data, stdout=sp.PIPE, stderr=sp.PIPE,
               env={**BASE_ENV, **(env or {})}, timeout=timeout)
    with (OUT/'commands.jsonl').open('a') as f:
        f.write(json.dumps({**COMMANDS[-1], 'returncode':r.returncode,
                            'stdout':r.stdout.decode(errors='replace'), 'stderr':r.stderr.decode(errors='replace')})+'\n')
    if check and r.returncode: raise RuntimeError(str(args)+': '+r.stderr.decode(errors='replace'))
    return r.stdout.decode(errors='replace').strip()

def traced(name, args, env=None):
    args = ['taskset','-c',str(CPU),'strace','-ff','-n','-ttt','-T','-yy','-v','-s','256','-o',str(OUT/name)] + list(map(str,args))
    COMMANDS.append({'argv':args,'env_overrides':env or {}})
    stdout = (OUT/(name+'.stdout')).open('wb'); stderr=(OUT/(name+'.stderr')).open('wb')
    p=sp.Popen(args,stdout=stdout,stderr=stderr,env={**BASE_ENV,**(env or {})},start_new_session=True)
    p.label=name; p.logfiles=(stdout,stderr); PROCS.append(p)
    return p

def until(fn, timeout=20):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        try:
            value=fn()
            if value: return value
        except (RuntimeError,sp.TimeoutExpired,OSError): pass
        time.sleep(.05)
    raise RuntimeError('readiness/condition timed out')

def stop(p, sig=signal.SIGTERM):
    if p.poll() is None:
        os.killpg(p.pid,sig)
        try:p.wait(10)
        except sp.TimeoutExpired:
            os.killpg(p.pid,signal.SIGKILL);p.wait(10)
    for f in p.logfiles:f.close()

def sampler():
    while not STOP.wait(.01):
        parents={}
        for d in pathlib.Path('/proc').iterdir():
            if not d.name.isdigit():continue
            try:
                fields=(d/'stat').read_text().rsplit(')',1)[1].split()
                parents[int(d.name)]=int(fields[1])
            except (OSError,IndexError):pass
        for p in PROCS:
            children={p.pid}
            for _ in range(8):
                children|={pid for pid,ppid in parents.items() if ppid in children}
            tasks=0
            for pid in children-{p.pid}:
                try:tasks+=len(list(pathlib.Path(f'/proc/{pid}/task').iterdir()))
                except OSError:pass
            entry=SAMPLES.setdefault(p.label,{'max_descendant_processes':0,'max_descendant_threads':0,'samples':0})
            entry['max_descendant_processes']=max(entry['max_descendant_processes'],len(children)-1)
            entry['max_descendant_threads']=max(entry['max_descendant_threads'],tasks)
            entry['samples']+=1

def redis():
    d=OUT/'redis-data'; d.mkdir(); rd=OUT/'redis-replica-data';rd.mkdir()
    def cmd(port,*args):return call(['redis-cli','-p',port,*args])
    def start(label,port,directory,replica=False):
        args=['redis-server','--port',port,'--bind','127.0.0.1','--protected-mode','yes','--dir',directory,
              '--save','','--appendonly','yes','--appendfsync','always','--auto-aof-rewrite-percentage','0',
              '--repl-diskless-sync-delay','0']
        if replica:args+=['--replicaof','127.0.0.1','19379']
        p=traced(label,args);until(lambda:cmd(port,'PING')=='PONG');return p
    primary=start('redis-primary',19379,d)
    payload=b''.join(f'*3\r\n$3\r\nSET\r\n${len(str(i))+1}\r\nk{i}\r\n$100\r\n'.encode()+b'x'*100+b'\r\n' for i in range(1000))
    pipe=call(['redis-cli','-p','19379','--pipe'],data=payload)
    replica=start('redis-replica',19380,rd,True)
    until(lambda:cmd(19380,'DBSIZE')=='1000',30)
    rewritten=cmd(19379,'BGREWRITEAOF')
    until(lambda:'aof_rewrite_in_progress:0' in cmd(19379,'INFO','persistence') and 'aof_last_bgrewrite_status:ok' in cmd(19379,'INFO','persistence'))
    saved=cmd(19379,'BGSAVE');until(lambda:'rdb_bgsave_in_progress:0' in cmd(19379,'INFO','persistence'))
    killed=cmd(19379,'CLIENT','KILL','TYPE','replica')
    cmd(19379,'SET','after-reconnect','ok');until(lambda:cmd(19380,'GET','after-reconnect')=='ok',30)
    wait=cmd(19379,'WAIT','1','5000')
    before=cmd(19379,'DBSIZE');info=cmd(19379,'INFO','all')
    cmd(19380,'SHUTDOWN','NOSAVE');replica.wait(10)
    cmd(19379,'SHUTDOWN','NOSAVE');primary.wait(10)
    recovered=start('redis-recovered',19379,d)
    after=cmd(19379,'DBSIZE');value=cmd(19379,'GET','after-reconnect')
    cmd(19379,'SHUTDOWN','NOSAVE');recovered.wait(10)
    assert before==after=='1001' and value=='ok' and wait=='1' and killed=='1'
    (OUT/'redis-info.txt').write_text(info)
    return {'keys_before':before,'keys_after_aof_restart':after,'pipe':pipe,'rewrite':rewritten,'rdb_save':saved,
            'replica_tcp_disconnect_count':killed,'replica_recovered_value':'ok','wait_replicas':wait,
            'scope':'2 native loopback processes; graceful AOF restart; induced client disconnect, no packet-loss/power-failure simulation'}

def etcd():
    d=OUT/'etcd-data'
    env={'GOMAXPROCS':'1'}
    args=['etcd','--name','w4','--data-dir',d,'--listen-client-urls','http://127.0.0.1:19390',
          '--advertise-client-urls','http://127.0.0.1:19390','--listen-peer-urls','http://127.0.0.1:19391',
          '--initial-advertise-peer-urls','http://127.0.0.1:19391','--initial-cluster','w4=http://127.0.0.1:19391',
          '--initial-cluster-token','seedbox-w4-seed-42','--snapshot-count','20','--log-level','warn']
    def ctl(*x):return call(['etcdctl','--endpoints=http://127.0.0.1:19390',*x],env=env)
    p=traced('etcd-server',args,env);until(lambda:'healthy' in ctl('endpoint','health'))
    for i in range(40):ctl('put',f'/w4/{i}',f'seed42-{i}')
    count=json.loads(ctl('get','/w4/','--prefix','-w','json'))['count'];assert count==40
    rev=json.loads(ctl('endpoint','status','-w','json'))[0]['Status']['header']['revision']
    ctl('compact',str(rev));ctl('defrag');snapshot=OUT/'etcd.snapshot';ctl('snapshot','save',snapshot)
    stop(p)
    p=traced('etcd-recovered',args,env);until(lambda:'healthy' in ctl('endpoint','health'))
    count2=json.loads(ctl('get','/w4/','--prefix','-w','json'))['count'];assert count2==40;stop(p)
    return {'written_keys':40,'keys_after_graceful_restart':count2,'revision_compacted':rev,
            'snapshot_sha256':hashlib.sha256(snapshot.read_bytes()).hexdigest(),'scope':'one member; no Raft network partition/cluster recovery'}

def postgres():
    d=OUT/'postgres-data';r=OUT/'postgres-replica-data';sock=OUT/'pgsocket';sock.mkdir()
    call(['initdb','-D',d,'--no-locale','--encoding=UTF8','--auth=trust'])
    with (d/'postgresql.conf').open('a') as f:f.write(f"\nlisten_addresses='127.0.0.1'\nport=19410\nunix_socket_directories='{sock}'\nwal_level=replica\nmax_wal_senders=3\nfsync=on\n")
    def sql(port,text):return call(['psql','-X','-h','127.0.0.1','-p',port,'-d','postgres','-At','-v','ON_ERROR_STOP=1','-c',text])
    p=traced('postgres-primary',['postgres','-D',d]);until(lambda:sql(19410,'select 1')=='1')
    sql(19410,"create table w4(k bigint primary key,v text); insert into w4 select n, repeat('x',100) from generate_series(1,1000)n; checkpoint")
    base=traced('postgres-basebackup',['pg_basebackup','-h','127.0.0.1','-p','19410','-D',r,'-R','-X','stream']);assert base.wait(30)==0
    with (r/'postgresql.conf').open('a') as f:f.write('\nport=19411\n')
    rp=traced('postgres-replica',['postgres','-D',r]);until(lambda:sql(19411,'select count(*) from w4')=='1000')
    sql(19410,"insert into w4 values(1001,'after-reconnect')")
    until(lambda:sql(19411,'select count(*) from w4')=='1001')
    # Terminate only this primary's WAL sender through its owned SQL connection;
    # standby reconnects without any host networking/config change.
    sender=sql(19410,'select pg_terminate_backend(pid) from pg_stat_replication');assert 't' in sender
    sql(19410,"insert into w4 values(1002,'recovered')")
    until(lambda:sql(19411,'select count(*) from w4')=='1002')
    recovery=sql(19411,'select pg_is_in_recovery()');assert recovery=='t'
    checkpoint=sql(19410,'checkpoint')
    call(['pg_ctl','-D',r,'stop','-m','fast']);rp.wait(15)
    call(['pg_ctl','-D',d,'stop','-m','fast']);p.wait(15)
    p=traced('postgres-recovered',['postgres','-D',d]);until(lambda:sql(19410,'select count(*) from w4')=='1002')
    call(['pg_ctl','-D',d,'stop','-m','fast']);p.wait(15)
    return {'rows_primary_and_replica':1002,'replica_in_recovery':recovery,'terminated_wal_sender':sender,
            'rows_after_graceful_restart':1002,'checkpoint':checkpoint,
            'scope':'2 native loopback clusters; streamed basebackup and WAL sender termination/reconnect; no promotion, power failure or packet loss'}

def tokio(mode):
    binary=ROOT/'workloads/tokio-service/target/release/seedbox-w4-tokio-service'
    p=traced('tokio-'+mode,[binary,mode]);values=[]
    for i in range(64):
        def connect():return socket.create_connection(('127.0.0.1',19400),timeout=2)
        conn=until(connect)
        with conn:
            conn.sendall(i.to_bytes(8,'little'));data=b''
            while len(data)<8:
                chunk=conn.recv(8-len(data))
                if not chunk:raise RuntimeError('short response')
                data+=chunk
            value=int.from_bytes(data,'little');assert value==i+1;values.append(value)
    assert p.wait(10)==0
    return {'requests':64,'responses_verified':64,'sum':sum(values),'timer_per_request_ms':1,
            'scope':mode+' runtime, finite arithmetic, TCP echo increment, no persistence/signals/fork'}

def metadata():
    binaries={name:call(['which',name]) for name in ['redis-server','etcd','postgres','strace','rustc','cargo','python3']}
    binaries['tokio-service']=str(ROOT/'workloads/tokio-service/target/release/seedbox-w4-tokio-service')
    values={}
    for name,path in binaries.items():
        actual=pathlib.Path(path).resolve()
        values[name]={'path':path,'resolved_path':str(actual),'sha256':hashlib.sha256(actual.read_bytes()).hexdigest()}
        values[name]['elf']=call(['readelf','-l','-d',str(actual)],check=False)
    versions={name:call(args,check=False) for name,args in {
        'uname':['uname','-a'],'redis':['redis-server','--version'],'etcd':['etcd','--version'],
        'postgres':['postgres','--version'],'strace':['strace','--version'],'rust':['rustc','--version'],
        'cargo':['cargo','--version'],'python':['python3','--version'],'go':['go','version']}.items()}
    return {'binaries':values,'versions':versions,'native_seed':'fixed workload values seed42; host entropy/clocks uncontrolled',
            'cpu_affinity':CPU,'GOMAXPROCS':1,'nix_lock_sha256':hashlib.sha256((ROOT/'env/flake.lock').read_bytes()).hexdigest(),
            'trace_options':['-ff','-n','-ttt','-T','-yy','-v','-s','256'],'environment':{k:os.environ.get(k) for k in ['TZ','LANG','LC_ALL']}}

if __name__=='__main__':
    sample=threading.Thread(target=sampler,daemon=True);sample.start()
    try:
        (OUT/'metadata.json').write_text(json.dumps(metadata(),indent=2))
        for name,fn in [('redis',redis),('etcd',etcd),('postgres',postgres),('tokio-current',lambda:tokio('current')),('tokio-multi',lambda:tokio('multi'))]:
            try:RESULTS[name]={'status':'passed',**fn()}
            except Exception as e:RESULTS[name]={'status':'failed','error':str(e)}
            (OUT/'results.json').write_text(json.dumps(RESULTS,indent=2))
            print(name,RESULTS[name],flush=True)
    finally:
        for p in reversed(PROCS):stop(p)
        STOP.set();sample.join(1)
        (OUT/'process-samples.json').write_text(json.dumps(SAMPLES,indent=2))
        (OUT/'launch-commands.json').write_text(json.dumps(COMMANDS,indent=2))
        print('ARTIFACT_DIR='+str(OUT),flush=True)
    if any(r['status']!='passed' for r in RESULTS.values()):sys.exit(1)
