#!/usr/bin/env python3
"""Crash/corruption/cancellation counterexamples against the real CLI/store host."""
import argparse,json,os,signal,subprocess,time,hashlib
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--cq',type=Path,required=True);p.add_argument('--output',type=Path,required=True);a=p.parse_args();a.cq=a.cq.resolve();a.output=a.output.resolve();a.output.mkdir(parents=True,exist_ok=True);source=a.output/'source';(source).mkdir(exist_ok=True);store=a.output/'store';hooks=store/'format-2/test-hooks';hooks.mkdir(parents=True,exist_ok=True);events=[];cases=[]
root=Path(__file__).resolve().parent.parent
env=os.environ|{'CQ_WORKERS':'4','CQ_PROGRESS':'0','CQ_STORE_HOST':str(a.cq.with_name('cq-store-host')),'CQ_GO_TYPES':str(root/'.codequest-tools/go-types/bin/cq-go-types'),'TMPDIR':'/tmp','CQ_LADYBUG_BUFFER_BYTES':str(128*1024*1024)}
(source/'go.mod').write_text('module example.test/crash\ngo 1.25\n');(source/'main.go').write_text('package crash\n// Run is the entry.\nfunc Run() int { return helper() }\n');(source/'helper.go').write_text('package crash\nfunc helper() int { return 7 }\n');(source/'README.md').write_text('# Crash fixture\nRun delegates to helper.\n')
# More than one normalization checkpoint interval: the kill must happen
# strictly inside the phase, with later files still outstanding.
for n in range(78): (source/f'token_{n:03}.go').write_text(f'package crash\nvar Token{n} = {n}\n')
def call(*args,expect=True,environment=None):
 start=time.monotonic();r=subprocess.run([str(a.cq),*map(str,args),'--store',str(store)],env=environment or env,capture_output=True,text=True,timeout=180)
 events.append({'args':list(map(str,args)),'exit':r.returncode,'seconds':time.monotonic()-start,'stdout':r.stdout,'stderr':r.stderr});(a.output/'events.json').write_text(json.dumps(events,indent=2))
 if expect:assert r.returncode==0,events[-1]
 return json.loads(r.stdout) if not r.returncode else r
# Kill after an actual record batch and metadata cursor are durable, before the
# stage can be marked complete. No destructors or graceful cleanup may help.
(hooks/'normalize-checkpoint').write_text('hold')
child=subprocess.Popen([str(a.cq),'index','--store',str(store),'--root',str(source),'--name','fixture','--language','go'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
start=time.monotonic()
while not (hooks/'ready-normalize-checkpoint').exists():
 if child.poll() is not None:raise AssertionError(child.communicate())
 if time.monotonic()-start>120:child.kill();raise TimeoutError('normalization checkpoint hook')
 time.sleep(.05)
run=next((store/'executions').iterdir()).name;state=json.loads((store/'executions'/run/'state.json').read_text());point=state['run']['stages']['normalize'][-1]['checkpoint'];assert 0<point['completed']<point['total'] and point['cursor']['frames']==point['completed'];child.kill();child.communicate(timeout=10);(hooks/'normalize-checkpoint').unlink()
resumed=call('resume','--run',run);revision=resumed['revision'];assert resumed['production']['transport']['native_exports']==1
snapshot=a.output/'snapshot.json';call('export-snapshot','--project','fixture','--output',snapshot);s=json.loads(snapshot.read_text());byid={e['id']:e for e in s['entities']};assert any(r['kind']=='Calls' and byid[r['from']]['name']=='Run' and byid[r['to']]['name']=='helper' for r in s['relations']);cases.append({'case':'SIGKILL_after_durable_normalization_batch','passed':True,'completed':point['completed'],'total':point['total']})
# Force a clean StoreHost reopen; the completed graph receipt and HEAD must
# agree before any stage is replayed.
log=store/'store-host.log';host=[json.loads(l)['pid'] for l in log.read_text().splitlines() if json.loads(l).get('event')=='host_ready'][-1];os.kill(host,signal.SIGKILL);assert call('status','--project','fixture')['revision']==revision;cases.append({'case':'host_death_after_publish_reopens_same_revision','passed':True})
# Corrupt a retained immutable segment. Resume must reject it while the old
# admitted HEAD remains queryable; restoring exact bytes enables recovery.
call('rerun-stage','--run',run,'--stage','documents','--until-stage','documents');state=json.loads((store/'executions'/run/'state.json').read_text());point=state['run']['stages']['normalize'][-1]['checkpoint'];output=json.loads((store/'executions'/run/point['artifacts'][0]['path']).read_text());payload=Path(output['records']['segments'][0]['payload']['path']);original=payload.read_bytes();bad=bytearray(original);bad[len(bad)//2]^=1;payload.write_bytes(bad);failure=call('resume','--run',run,expect=False);assert failure.returncode!=0 and 'DigestMismatch' in failure.stderr;assert call('status','--project','fixture')['revision']==revision;payload.write_bytes(original);assert call('resume','--run',run)['revision']==revision;cases.append({'case':'corrupt_segment_rejected_without_changing_HEAD','passed':True})
# Graceful interruption must cancel the backend task at a safe boundary,
# rather than leaving an unbounded import running after the terminal exits.
project=store/'format-2/projects'/hashlib.sha256(b'fixture').hexdigest();backend_hooks=project/'test-hooks';backend_hooks.mkdir(exist_ok=True);name='stage-audit_inventory-started';(backend_hooks/name).write_text('hold')
child=subprocess.Popen([str(a.cq),'rerun-stage','--run',run,'--stage','audit_inventory','--store',str(store)],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True);start=time.monotonic()
while not (backend_hooks/('ready-'+name)).exists():
 if child.poll() is not None:raise AssertionError(child.communicate())
 if time.monotonic()-start>60:child.kill();raise TimeoutError('backend stage hook')
 time.sleep(.05)
child.send_signal(signal.SIGINT);stdout,stderr=child.communicate(timeout=20);assert child.returncode!=0 and 'Cancelled' in stderr,(stdout,stderr)
(backend_hooks/('release-'+name)).write_text('continue');before=call('run-status','--run',run);attempt=next(s['attempt']['number'] for s in before['stages'] if s['stage']=='audit_inventory');assert call('resume','--run',run)['revision']==revision;after=call('run-status','--run',run);assert next(s['attempt']['number'] for s in after['stages'] if s['stage']=='audit_inventory')==attempt;cases.append({'case':'SIGINT_cancels_backend_and_resume_reuses_attempt','passed':True})
# A changed provider may not consume old checkpoints, even if the file remains
# a runnable executable. No stage should be admitted under a false tool binding.
changed=a.output/'changed-provider';changed.write_bytes(Path(env['CQ_GO_TYPES']).read_bytes()+b'changed');changed.chmod(0o700);failure=call('rerun-stage','--run',run,'--stage','native_export','--until-stage','native_export',expect=False,environment=env|{'CQ_GO_TYPES':str(changed)});assert failure.returncode!=0 and 'ExecutionToolChanged' in failure.stderr;cases.append({'case':'changed_provider_rejected','passed':True})
summary={'passed':True,'cases':cases,'scope':'real small Go compiler fixture and actual Ladybug; no full Brocade run'};(a.output/'summary.json').write_text(json.dumps(summary,indent=2));print(json.dumps(summary))
