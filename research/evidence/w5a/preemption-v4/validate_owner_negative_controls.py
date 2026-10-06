import json,pathlib,shutil,subprocess,tempfile,sys
root=pathlib.Path(sys.argv[1]) if len(sys.argv)>1 else pathlib.Path(__file__).parent;validator=root/'preempt_validate_v4_owner_interpretation.py'
base=pathlib.Path(tempfile.mkdtemp(prefix='w5a-validator-',dir='/private/tmp'))
for p in root.glob('*.json'):shutil.copy2(p,base/p.name)
for p in root.glob('run.*'):
 if not p.is_dir():continue
 (base/p.name).mkdir()
 for n in ['stdout.txt','stderr.txt','exit-code.txt']:
  if (p/n).exists():shutil.copy2(p/n,base/p.name/n)
cmd=['python3',str(validator),str(base),'--probe','1f2876f62bfd91804b6c20fb03cad141c665ecba8ad8ef96fcdfa2848a1f36f4','--guest','d276166e822b2de6eb9eb4c7658f63d72300e1c08882dab5104ccd0cef9b9eae']
def check():
 p=subprocess.run(cmd,text=True,capture_output=True);v=json.loads(p.stdout);return {'exitcode':p.returncode,'accepted':v['accepted'],'failures':v['failures']}
results={'actual_baseline':check(),'mutations':{}}
for name,run,mutate in [
 ('missed_count','run.usKaoQ',lambda v:v.update(final_count=v['target']+1)),
 ('bad_REP_prefix','run.dvBPYp',lambda v:v.update(rep_bytes_valid=False)),
 ('executed_following_canary','run.ZxRDVj',lambda v:v['pio_witness']['following_instruction'].update(canary=v['pio_witness']['canary_before']+2)),
 ('changed_terminal_RAM','run.usKaoQ',lambda v:v['terminal'].update(ram_sha256='0'*64)),
 ('overshoot_flag','run.usKaoQ',lambda v:v.update(overshoot=True)),
]:
 p=base/run/'stdout.txt';original=p.read_text();v=json.loads(original);mutate(v);p.write_text(json.dumps(v)+'\n');results['mutations'][name]=check();p.write_text(original)
p=root/'preempt-owner-validator-negative-controls.json';p.write_text(json.dumps(results,indent=2)+'\n');print(json.dumps({'baseline_pass':results['actual_baseline']['accepted'],'all_mutations_rejected':all(v['exitcode']==1 and not v['accepted'] for v in results['mutations'].values())}));shutil.rmtree(base)
