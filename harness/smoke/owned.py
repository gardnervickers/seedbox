"""Reviewed batch46 session ownership, extracted without new supervision policy.

From frozen offline42-adapted.py revision3: owned_session through run_owned_replay.
Only adaptation: cwd is explicit instead of the historical ROOT global.
Linux-only; no device access or host mutation. Unresolved cleanup refuses success.
"""
import json
import os
from pathlib import Path as P
import signal
import subprocess
import sys
import time

def owned_session(sid):
    """Linux ownership oracle: include the VMM's separate process group."""
    members=[]; errors=[]
    for entry in P('/proc').iterdir():
        if not entry.name.isdecimal(): continue
        try:
            fields=(entry/'stat').read_text().rsplit(')',1)[1].split()
            if int(fields[3])==sid:
                members.append({'pid':int(entry.name),'start_ticks':int(fields[19]),'pgid':int(fields[2]),'sid':int(fields[3]),'state':fields[0]})
        except (FileNotFoundError,ProcessLookupError): pass
        except Exception as e: errors.append({'pid':entry.name,'error':str(e)})
    return {'members':members,'errors':errors}

def process_identity(pid):
    fields=(P('/proc')/str(pid)/'stat').read_text().rsplit(')',1)[1].split()
    return {'pid':pid,'start_ticks':int(fields[19]),'pgid':int(fields[2]),'sid':int(fields[3]),'state':fields[0]}

def same_identity(a,b):
    return all(a[key]==b[key] for key in ('pid','start_ticks','pgid','sid'))

def exited_unreaped(child):
    # Doctor pattern: never poll/reap the anchor before the last possible signal.
    if child.returncode is not None: raise RuntimeError('owned leader was unexpectedly reaped')
    return os.waitid(os.P_PID,child.pid,os.WEXITED|os.WNOHANG|os.WNOWAIT) is not None

def signal_owned_member(member,leader,child):
    if child.returncode is not None: raise RuntimeError('refuse signal after leader reaping')
    if not same_identity(process_identity(child.pid),leader): raise RuntimeError('leader identity lost; refuse signal')
    current=process_identity(member['pid'])
    if not same_identity(current,member) or current['sid']!=leader['sid'] or current['start_ticks']<leader['start_ticks']:
        raise RuntimeError('tracked member identity stale; refuse signal')
    # Necessary separate-group adaptation: pidfd pins this exact task, unlike a
    # sampled numeric killpg. Re-read identity after opening; never fall back.
    fd=os.pidfd_open(member['pid'],0)
    try:
        if not same_identity(process_identity(member['pid']),member): raise RuntimeError('member changed while opening pidfd')
        signal.pidfd_send_signal(fd,signal.SIGTERM,None,0)
    finally: os.close(fd)

def run_owned_replay(cmd,env,out,err,stop_at,allocation_deadline,on_spawn,tick,journal,cleanup_grace=10,*,cwd):
    child=None; leader=None; history=[]; spawning=False; stopping=False; pending=[]
    receipt={'pid':None,'sid':None,'exit':None,'failure':None,'signals':[],'cleanup_errors':[],'leader_reaped':False,'owned_session_gone':False,'manager_handoff':False}
    old_handlers={sig:signal.getsignal(sig) for sig in (signal.SIGINT,signal.SIGTERM,signal.SIGALRM)}
    def interrupted(sig,frame):
        receipt['signals'].append(sig)
        if spawning or stopping:
            pending.append(sig)
            if stopping: receipt['failure']=receipt['failure'] or 'catchable cancellation during cleanup'
            return
        raise KeyboardInterrupt('catchable wrapper signal '+str(sig))
    def sample():
        if child.returncode is not None: raise RuntimeError('owned leader reaped before ownership completion')
        if not same_identity(process_identity(child.pid),leader): raise RuntimeError('owned leader identity lost')
        value=owned_session(child.pid); history.append(value)
        if value['errors']: raise RuntimeError('owned session inventory unreadable')
        if not any(same_identity(member,leader) for member in value['members']): raise RuntimeError('unreaped leader anchor missing')
        return value
    try:
        for sig in old_handlers: signal.signal(sig,interrupted)
        # Reviewed doctor spawn deferral: register before delivering cancellation.
        spawning=True
        try:
            child=subprocess.Popen(cmd,env=env,cwd=cwd,stdout=out,stderr=err,start_new_session=True)
            receipt['pid']=child.pid; receipt['sid']=child.pid
            leader=process_identity(child.pid); receipt['leader_identity']=leader
            if leader['sid']!=child.pid or leader['pgid']!=child.pid: raise RuntimeError('new owned session not established')
        finally: spawning=False
        if pending: raise KeyboardInterrupt('deferred spawn cancellation '+str(pending))
        on_spawn(child)
        while not exited_unreaped(child):
            sample(); tick()
            if time.monotonic()>=stop_at: raise TimeoutError('owned command allocation deadline')
            time.sleep(.02)
        value=sample()
        if any(m['pid']!=child.pid for m in value['members']): raise RuntimeError('owned work remains after leader exit')
        receipt['exit']=child.wait(timeout=0); receipt['leader_reaped']=True; receipt['owned_session_gone']=True
    except BaseException as e:
        receipt['failure']=repr(e); receipt['failure_type']=type(e).__name__
    finally:
        stopping=True
        if child is None:
            receipt['leader_reaped']=True; receipt['owned_session_gone']=True  # Popen spawn error; no returned owned handle.
        # Early failures receive only ten seconds, never the distant allocation.
        cleanup_until=min(allocation_deadline() if callable(allocation_deadline) else allocation_deadline,time.monotonic()+cleanup_grace)
        receipt['cleanup_deadline_monotonic']=cleanup_until
        if child is not None and not receipt['owned_session_gone']:
            try:
                if leader is None: raise RuntimeError('missing registered leader identity; cleanup uncertain')
                signaled=set()
                while time.monotonic()<cleanup_until:
                    value=sample()
                    done=exited_unreaped(child)
                    others=[m for m in value['members'] if m['pid']!=child.pid]
                    if done and not others:
                        receipt['exit']=child.wait(timeout=0); receipt['leader_reaped']=True; receipt['owned_session_gone']=True; break
                    for member in value['members']:
                        key=(member['pid'],member['start_ticks'],member['sid'],member['pgid'])
                        if member['state']=='Z' or key in signaled: continue
                        try: signal_owned_member(member,leader,child); signaled.add(key)
                        except ProcessLookupError: pass  # next anchored inventory must prove absence
                    time.sleep(.02)
                if not receipt['owned_session_gone']: receipt['cleanup_errors'].append('cleanup grace expired; owned work unresolved')
            except BaseException as e: receipt['cleanup_errors'].append('cleanup identity/termination: '+repr(e))
        receipt['ownership_samples']=history
        receipt['manager_handoff']=not(receipt['leader_reaped'] and receipt['owned_session_gone']) or bool(receipt['cleanup_errors'])
        try: journal(receipt)
        except BaseException as e:
            receipt['cleanup_errors'].append('receipt write: '+repr(e)); receipt['manager_handoff']=True; receipt['failure']=receipt['failure'] or 'owned lifecycle receipt write failed'
        if receipt['manager_handoff']:
            print('manager handoff: FAILED/UNCERTAIN; stop later steps; '+json.dumps(receipt),file=sys.stderr,flush=True)
        for sig,handler in old_handlers.items(): signal.signal(sig,handler)
    return receipt

