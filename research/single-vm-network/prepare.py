#!/usr/bin/env python3
import argparse,json
from pathlib import Path
ap=argparse.ArgumentParser();ap.add_argument('--root',required=True);ap.add_argument('--mem-mb',type=int,default=1024);a=ap.parse_args()
r=Path(a.root).resolve();p=r/'research/single-vm-network'
common={'kernel':str(r/'artifacts/bzImage'),'mem_mb':a.mem_mb,'duration_ns':20000000000,'stop_tolerance_ns':2000000,'timer_jitter_ns':0}
args='seedbox.delay=1.5 seedbox.load=10s seedbox.concurrency=2 seedbox.writes=64'
single={**common,'end_on_exit':'whole','machines':[{'name':'whole','initrd':str(r/'artifacts/single.cpio'),'cmdline':args}]}
multi={**common,'end_on_exit':'client','network':{'latency_ns':1000000,'jitter_ns':0},'machines':[{'name':n,'initrd':str(r/'artifacts/multi.cpio'),**({'cmdline':args}if n=='client' else{})}for n in ('n1','n2','n3','client')]}
controls=json.loads(json.dumps(single));controls['machines'][0]['cmdline']='seedbox.mode=controls'
negative=json.loads(json.dumps(single));negative['machines'][0]['cmdline']+=' seedbox.plant=missing'
fault=json.loads(json.dumps(single));fault['machines'][0]['cmdline']='seedbox.delay=1.5 seedbox.load=3s seedbox.concurrency=2 seedbox.writes=0'
fault['scenario']=[{'machine':'whole','at_ns':2200000000,'serial':'partition n1\n'},{'machine':'whole','at_ns':2900000000,'serial':'heal n1\n'},{'machine':'whole','at_ns':3300000000,'serial':'stop n2\n'},{'machine':'whole','at_ns':3900000000,'serial':'start n2\n'}]
debug=json.loads(json.dumps(single));debug['duration_ns']=60000000000
debug['scenario']=[{'machine':'whole','at_ns':8000000000,'serial':'log n1\nlog n2\nlog n3\nnet n1\n'}]
resources=json.loads(json.dumps(debug));resources['duration_ns']=12000000000;resources['scenario']=[{'machine':'whole','at_ns':5000000000,'serial':'log n1\n'}]
for name,w in [('resources',resources),('debug',debug),('single',single),('multi',multi),('controls',controls),('negative',negative),('fault',fault)]:
 (p/(name+'.json')).write_text(json.dumps(w,indent=2)+'\n')
