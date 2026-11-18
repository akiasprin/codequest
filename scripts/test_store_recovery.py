#!/usr/bin/env python3
"""Real native fsync failures and legacy migration, isolated task-owned stores."""
import argparse, concurrent.futures, hashlib, json, os, socket, struct, subprocess, time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--output', type=Path, required=True)
p.add_argument('--cq', type=Path, default=ROOT/'target/debug/cq')
p.add_argument('--only', choices=['fsync', 'migration'], action='append')
p.add_argument('--fault-mode', choices=['directory-once','directory-repeat','head-file'], action='append')
a = p.parse_args(); a.cq = a.cq.resolve(); a.output.mkdir(parents=True, exist_ok=True)
env = os.environ | {'CQ_STORE_HOST': str(a.cq.with_name('cq-store-host'))}
cases = []; hosts = []
def check(condition, message):
    if not condition: raise AssertionError(message)
def run(command, **kwargs):
    r = subprocess.run(list(map(str, command)), capture_output=True, text=True, timeout=180, **kwargs)
    with (a.output/'events.jsonl').open('a') as f:
        f.write(json.dumps({'command':list(map(str,command)), 'exit':r.returncode, 'stdout':r.stdout, 'stderr':r.stderr})+'\n')
    return r
def cli(store, *args, ok=True):
    r = run([a.cq, *args, '--store', store], env=env)
    if ok: check(r.returncode == 0, r.stderr)
    return json.loads(r.stdout) if r.returncode == 0 else r
def rpc(store, value):
    path = '/tmp/codequest-%s-%s.sock' % (os.geteuid(), hashlib.sha256(os.fsencode(store.resolve())).hexdigest()[:24])
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(60); s.connect(path); b=json.dumps(value).encode(); s.sendall(struct.pack('>I',len(b))+b)
        def read(n):
            out=b''
            while len(out)<n:
                part=s.recv(n-len(out)); check(bool(part),'IPC EOF'); out+=part
            return out
        return json.loads(read(struct.unpack('>I',read(4))[0]))
def waitfor(path):
    end=time.monotonic()+60
    while not path.exists():
        check(time.monotonic()<end,'barrier timeout '+str(path)); time.sleep(.01)
def generator(name, variable, path):
    r=run(['cargo','test','-p','cq-store','--test','paged_store','--locked','--offline',name,'--','--ignored','--exact'],cwd=ROOT,env=env|{variable:str(path.resolve())})
    check(r.returncode==0,r.stderr)
def case(name, fn):
    start=time.monotonic()
    try:
        evidence=fn(); cases.append({'case':name,'status':'passed','seconds':time.monotonic()-start,'evidence':evidence})
    except BaseException as e:
        cases.append({'case':name,'status':'failed','error':repr(e)}); raise
def faults(mode):
    fixture=a.output/mode; generator('export_cli_acceptance_fixture','CQ_PAGED_FIXTURE_OUTPUT',fixture)
    store=fixture/'store'; project=store/'format-2/projects'/hashlib.sha256(b'paged-test').hexdigest()
    project.mkdir(parents=True,exist_ok=True)
    stderr=(fixture/'host.stderr').open('w')
    host=subprocess.Popen([str(a.cq.with_name('cq-store-host')),'--store',str(store)],env=env|{'LD_PRELOAD':str(a.output/'fsync_fault.so'),'CQ_STORE_FAULT_ROOT':str(project.resolve()),'CQ_STORE_FAULT_MODE':mode},stdout=subprocess.DEVNULL,stderr=stderr); hosts.append(host)
    # Wait for the manually instrumented daemon; do not let CLI autostart another.
    end=time.monotonic()+30
    while True:
        try:
            check(rpc(store,{'op':'ping'})['result']['pid']==host.pid,'wrong daemon'); break
        except (OSError, KeyError):
            check(host.poll() is None and time.monotonic()<end,'host startup failed'); time.sleep(.02)
    first=cli(store,'publish-snapshot','--snapshot',fixture/'R1.json')['revision']
    held=rpc(store,{'op':'open','project':'paged-test','revision':first})['result']
    hooks=project/'test-hooks'; hooks.mkdir(); (hooks/'after-index').touch()
    with concurrent.futures.ThreadPoolExecutor() as executor:
        future=executor.submit(cli,store,'publish-snapshot','--snapshot',fixture/'R2.json',ok=False)
        waitfor(hooks/'ready-after-index'); (project/'INJECT_FSYNC').touch(); (hooks/'release-after-index').touch(); result=future.result(timeout=120)
    (hooks/'after-index').unlink(); second=json.loads((fixture/'R2.json').read_text())['revision']
    if mode=='directory-repeat':
        check(isinstance(result,subprocess.CompletedProcess) and 'CommitUnknown' in result.stderr,str(result))
        blocked=rpc(store,{'op':'open','project':'paged-test'}); check(blocked.get('error',{}).get('code')=='RecoveryRequired',str(blocked))
        # A read admitted before the uncertain commit remains available.
        row=rpc(store,{'op':'get','view':held['view'],'family':'entity','id':'f0'}); check('error' not in row and row['result']['value']['value']['id']=='f0',str(row))
    elif mode=='head-file':
        check(isinstance(result,subprocess.CompletedProcess) and 'PublishRejected' in result.stderr,str(result))
        check(json.loads((project/'HEAD').read_text())==first,'failed HEAD-file fsync committed')
    else: check(isinstance(result,dict),'single directory error was not resolved')
    (project/'INJECT_FSYNC').unlink()
    decision=cli(store,'resolve-store','--project','paged-test','--revision',second)
    check(decision['outcome']==('NotCommitted' if mode=='head-file' else 'Committed'),str(decision))
    cli(store,'publish-snapshot','--snapshot',fixture/'R2.json')
    check(json.loads((project/'HEAD').read_text())==second,'retry failed')
    rpc(store,{'op':'release','view':held['view']}); host.terminate(); host.wait(timeout=30); stderr.close()
    evidence=(fixture/'host.stderr').read_text(); check('STORE_FAULT fsync EIO' in evidence,'injection not exercised')
    return {'mode':mode,'resolved':decision,'injection_count':evidence.count('STORE_FAULT fsync EIO'),'head_after_retry':second}
def migration():
    store=a.output/'legacy-store'; generator('export_legacy_acceptance_fixture','CQ_LEGACY_FIXTURE_OUTPUT',store)
    ids=json.loads((store/'legacy-ids.json').read_text()); artifacts={str(f.relative_to(store)):hashlib.sha256(f.read_bytes()).hexdigest() for f in [*store.glob('revisions/*/*'),*store.glob('projects/*/annotations/*')] if f.is_file()}
    result=cli(store,'migrate-store','--project','paged-test'); check(result['head']==ids[-1] and len(result['revisions'])==2,str(result))
    for rev in ids:
        output=a.output/('export-'+rev+'.json'); cli(store,'export-snapshot','--project','paged-test','--revision',rev,'--output',output)
        check(json.loads(output.read_text())==json.loads((store/'revisions'/rev/'snapshot.json').read_text()),'migration changed facts/order')
        q=cli(store,'query','--project','paged-test','--revision',rev,'--json',json.dumps({'operation':'callees','subject':'f0'})); check(q['revision']==rev and len(q['items'])==1,str(q))
    check(all((store/path).is_file() and hashlib.sha256((store/path).read_bytes()).hexdigest()==sha for path,sha in artifacts.items()),'legacy artifacts mutated')
    notes=cli(store,'annotations','--project','paged-test'); check('fixture responsibility' in json.dumps(notes),'annotation missing after migration')
    check(cli(store,'migrate-store','--project','paged-test')['state']=='already_migrated','migration retry')
    pid=rpc(store,{'op':'ping'})['result']['pid']; os.kill(pid,15)
    return {'revisions':ids,'legacy_artifacts_preserved':len(artifacts),'exact_roundtrip':True}
try:
    if not a.only or 'fsync' in a.only:
        r=run(['cc','-shared','-fPIC','-O2',ROOT/'crates/cq-store/tests/fsync_fault.c','-ldl','-o',a.output/'fsync_fault.so']); check(r.returncode==0,r.stderr)
        for mode in a.fault_mode or ['directory-once','directory-repeat','head-file']: case('F_'+mode,lambda mode=mode:faults(mode))
    if not a.only or 'migration' in a.only: case('M_legacy_migration',migration)
finally:
    for host in hosts:
        if host.poll() is None: host.terminate(); host.wait(timeout=30)
    result={'passed':sum(c['status']=='passed' for c in cases),'total':len(cases),'cases':cases}
    (a.output/'results.json').write_text(json.dumps(result,indent=2)+'\n'); print(json.dumps(result))
