#!/usr/bin/env python3
"""Off-host source smoke with already-available JDK; records exact local toolchain.
Full benchmark pin is package.nix from the coordinator's shared environment.
"""
import argparse, hashlib, json, pathlib, shutil, subprocess

def main():
    ap=argparse.ArgumentParser(); ap.add_argument('--output',type=pathlib.Path,required=True); args=ap.parse_args()
    root=pathlib.Path(__file__).resolve().parents[1]
    args.output.mkdir(parents=True,exist_ok=True)
    lock=json.loads((root/'artifacts.lock.json').read_text())
    jars=[]
    for item in lock['artifacts']:
        path=args.output/'deps'/f"{item['artifact']}-{item['version']}.jar"
        if not path.exists() or hashlib.sha256(path.read_bytes()).hexdigest()!=item['sha256']:
            raise ValueError('run fetch_artifacts.py first: '+str(path))
        jars.append(str(path.resolve()))
    classes=args.output/'classes';classes.mkdir(exist_ok=True)
    source=root/'src/main/java/seedbox/benchmark/RedisJedisBenchmark.java'
    cp=':'.join(jars)
    subprocess.run(['javac','--release','21','-cp',cp,'-d',str(classes),str(source)],check=True)
    target=args.output/'benchmark.jar'
    subprocess.run(['jar','--create','--date=2020-01-01T00:00:00Z','--file',str(target),'-C',str(classes),'.'],check=True)
    java=subprocess.run(['java','-version'],capture_output=True,text=True,check=True)
    result={'qualification':'off-host compile; no guest/JIT/snapshot/performance qualification',
            'java_path':str(pathlib.Path(shutil.which('java')).resolve()),
            'javac_path':str(pathlib.Path(shutil.which('javac')).resolve()),
            'java_version':java.stderr.strip(), 'source_sha256':hashlib.sha256(source.read_bytes()).hexdigest(),
            'jar_sha256':hashlib.sha256(target.read_bytes()).hexdigest(),
            'classpath':str(target.resolve())+':'+cp}
    (args.output/'build.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result,indent=2))

if __name__=='__main__':main()
