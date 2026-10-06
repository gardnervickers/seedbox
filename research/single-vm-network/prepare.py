#!/usr/bin/env python3
# Current research worlds reuse the ordinary supported fixture; old raw inputs stay historical.
import argparse, json
from pathlib import Path
ap=argparse.ArgumentParser(); ap.add_argument('--root',required=True); a=ap.parse_args()
r=Path(a.root).resolve()
for dest, source in [('single','single-vm'),('multi','cluster-fixed'),('controls','single-vm-controls'),('negative','single-vm-sanity'),('fault','single-vm-fault')]:
 w=json.loads((r/'sim/worlds'/f'{source}.json').read_text())
 w['kernel']=str(r/'artifacts/bzImage')
 for machine in w['machines']:machine['initrd']=str(r/'artifacts'/Path(machine['initrd']).name)
 (r/'research/single-vm-network'/f'{dest}.json').write_text(json.dumps(w,indent=2)+'\n')
