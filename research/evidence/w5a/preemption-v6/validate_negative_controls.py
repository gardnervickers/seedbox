import json,pathlib,shutil,subprocess,tempfile
root=pathlib.Path(__file__).parent.resolve();tmp=pathlib.Path(tempfile.mkdtemp(prefix='w5a-matrix-controls-',dir='/private/tmp'));work=tmp/'evidence';shutil.copytree(root,work)
cmd=['python3',str(root/'preempt_linux_validate.py'),str(work),'--plan',str(work/'expected-matrix.json')]
def check():
 p=subprocess.run(cmd,capture_output=True,text=True);v=json.loads(p.stdout);return {'exitcode':p.returncode,'accepted':v['accepted'],'measured':v['measured'],'not_executed':sum(x['state']=='not_executed'for x in v['attempt_status']),'failures':v['failures']}
plan=json.loads((work/'expected-matrix.json').read_text());first=plan['attempts'][0];results={'actual_baseline':check(),'mutations':{}}
p=work/first['receipt'];data=p.read_bytes();p.unlink();results['mutations']['missing_one_receipt']=check();p.write_bytes(data)
ledger=[json.loads(l)for l in (work/(first['batch']+'-ledger.jsonl')).read_text().splitlines()];entry=next(x for x in ledger if x['attempt_id']==first['id'] and x['state']=='finished');run=pathlib.Path(entry['evidence'][0]).name
p=work/run/'stdout.txt';data=p.read_bytes();p.unlink();results['mutations']['missing_one_result']=check();p.write_bytes(data)
p=work/(first['batch']+'-host-after.json');data=p.read_bytes();v=json.loads(data);v['profile']['cpu2_microcode']='changed';p.write_text(json.dumps(v));results['mutations']['host_drift_despite_true_boolean']=check();p.write_bytes(data)
p=work/'expected-matrix.json';data=p.read_bytes();v=json.loads(data);v['attempts']=[x for x in v['attempts'] if x['batch']==first['batch']];v['batches']=v['batches'][:1];p.write_text(json.dumps(v));results['mutations']['incomplete_declared_matrix']=check();p.write_bytes(data)
# Simulate a whole batch never executed; don't convert its expected attempts into passes.
loaded=plan['batches'][1];(work/loaded['ledger']).unlink()
for spec in plan['attempts']:
 if spec['batch']!=loaded['id']:continue
 (work/spec['receipt']).unlink()
 for d in work.glob('run.*'):
  if (d/'stderr.txt').exists() and spec['receipt'] in (d/'stderr.txt').read_text():shutil.rmtree(d)
results['mutations']['whole_loaded_batch_not_executed']=check()
(root/'validator-negative-controls.json').write_text(json.dumps(results,indent=2)+'\n');shutil.rmtree(tmp)
assert results['actual_baseline']['accepted'] and all(v['exitcode']==1 and not v['accepted'] for v in results['mutations'].values())
assert results['mutations']['whole_loaded_batch_not_executed']['not_executed']==22
print(json.dumps({'actual_baseline_pass':True,'all_five_corruptions_rejected':True,'omitted_batch_nonexecuted':22}))
