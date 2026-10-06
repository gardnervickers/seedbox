#!/usr/bin/env python3
"""Off-host application/control smoke against a RESP stub, NOT Redis or a world.
Only validates Java startup/barrier/oracles; no performance/admission inference.
"""
import argparse, copy, json, pathlib, socketserver, subprocess, threading, time

class Store:
    def __init__(self):
        self.primary={};self.replica={};self.pause=False;self.lock=threading.Lock()
        self.paused_writes=0;self.rewrites=0

class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        state=self.server.store;replica=self.server.replica
        while True:
            first=self.rfile.readline()
            if not first:return
            if not first.startswith(b'*'):raise ValueError('expectedRESP array')
            args=[]
            for _ in range(int(first[1:])):
                size=self.rfile.readline()
                if not size.startswith(b'$'):raise ValueError('expectedRESP bulk')
                value=self.rfile.read(int(size[1:]));assert self.rfile.read(2)==b'\r\n';args.append(value)
            command=args[0].upper()
            with state.lock:
                if command==b'SET' and not replica:
                    state.primary[args[1]]=args[2]
                    if not state.pause:state.replica[args[1]]=args[2]
                    else:state.paused_writes+=1
                    response=b'+OK\r\n'
                elif command==b'GET':
                    value=(state.replica if replica else state.primary).get(args[1])
                    response=b'$-1\r\n' if value is None else b'$'+str(len(value)).encode()+b'\r\n'+value+b'\r\n'
                elif command==b'WAIT':response=b':'+(b'0' if state.pause else b'1')+b'\r\n'
                elif command==b'BGREWRITEAOF':
                    state.rewrites+=1;response=b'+Background append only file rewriting started\r\n'
                elif command==b'INFO':
                    value=b'aof_rewrite_in_progress:0\r\naof_last_bgrewrite_status:ok\r\n'
                    response=b'$'+str(len(value)).encode()+b'\r\n'+value+b'\r\n'
                elif command in (b'PING',b'CLIENT',b'SELECT'):response=b'+OK\r\n'
                elif command==b'QUIT':response=b'+OK\r\n'
                else:raise ValueError('unimplemented mockcommand '+repr(command))
            self.wfile.write(response);self.wfile.flush()
            if command==b'QUIT':return

def main():
    ap=argparse.ArgumentParser();ap.add_argument('--build',type=pathlib.Path,required=True);ap.add_argument('--output',type=pathlib.Path,required=True);args=ap.parse_args()
    root=pathlib.Path(__file__).resolve().parents[1]
    args.output.mkdir(parents=True,exist_ok=True)
    profile=json.loads((root/'profile.json').read_text())
    profile['profile_id']='redis-jedis-local-resp-mock-smoke'
    profile['qualification']='RESPmock only; distinct from60s decidingprofile'
    profile['workload'].update(duration_ns=2_000_000_000,operations=200,key_count=32,warmup_operations=200,aof_rewrite_at_ns=1_300_000_000)
    profile['faults'][0]['at_ns']=500_000_000
    profile['faults'][1]['at_ns']=1_000_000_000
    path=args.output/'profile.json';path.write_text(json.dumps(profile,indent=2)+'\n')
    cp=json.loads((args.build/'build.json').read_text())['classpath']
    store=Store();servers=[]
    for replica in (False,True):
        server=socketserver.ThreadingTCPServer(('127.0.0.1',0),Handler)
        server.daemon_threads=True;server.store=store;server.replica=replica
        threading.Thread(target=server.serve_forever,daemon=True).start();servers.append(server)
    command=['java',*profile['jvm']['flags'],'-cp',cp,'seedbox.benchmark.RedisJedisBenchmark',
             '--profile',str(path),'--primary','127.0.0.1','--replica','127.0.0.1',
             '--primary-port',str(servers[0].server_address[1]),'--replica-port',str(servers[1].server_address[1]),'--barrier','stdin']
    results={}
    def run(label,negative):
        p=subprocess.Popen(command+['--negative-control',str(negative).lower()],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        lines=[]
        ready=json.loads(p.stdout.readline());lines.append(ready);assert ready['phase']=='ready'
        p.stdin.write('START 0\n');p.stdin.flush()
        started=json.loads(p.stdout.readline());lines.append(started);assert started['phase']=='started'
        fault=None
        if not negative:
            def pause_and_heal():
                time.sleep(.5)
                with store.lock:store.pause=True
                time.sleep(.5)
                with store.lock:store.replica=dict(store.primary);store.pause=False
            fault=threading.Thread(target=pause_and_heal);fault.start()
        stdout,stderr=p.communicate(timeout=15)
        lines.extend(json.loads(line) for line in stdout.splitlines() if line)
        if fault:fault.join()
        (args.output/(label+'.json')).write_text(json.dumps({'argv':command,'returncode':p.returncode,'messages':lines,'stderr':stderr},indent=2)+'\n')
        final=lines[-1]
        if negative:
            assert p.returncode==1 and final['phase']=='failed' and 'read mismatch' in final['message']
            return {'status':'sensitive oraclecontrol rejected corruptedexpectedvalue','exit':1}
        assert p.returncode==0 and final['phase']=='complete'
        assert final['canonical']['reads']==160 and final['canonical']['acknowledged_writes']==40
        assert final['canonical']['replica_keys_verified']==32 and final['canonical']['aof_rewrite_completed'] is True
        assert store.paused_writes>0 and store.rewrites==1
        return {'status':'passed','canonical':final['canonical'],'mock_paused_writes':store.paused_writes}
    try:
        results['positive']=run('positive',False)
        results['negative']=run('negative',True)
        results['qualification']='local JVM/RESPmock smoke; no Redis, VM, packetfault, AOFdurability, snapshots or performance acceptance'
        (args.output/'summary.json').write_text(json.dumps(results,indent=2)+'\n')
        print(json.dumps(results,indent=2))
    finally:
        for server in servers:server.shutdown();server.server_close()

if __name__=='__main__':main()
