#!/usr/bin/env python3
"""Small real-compiler CLI blocks; never launches a repository-wide benchmark."""
import argparse,json,os,subprocess,time
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--cq',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--continue-run',action='store_true');p.add_argument('--repair-from');p.add_argument('--language',choices=['go','rust','c','cpp'],default='go');a=p.parse_args();a.cq=a.cq.resolve();a.output=a.output.resolve();a.output.mkdir(parents=True,exist_ok=True)
source=a.output/'source';source.mkdir(exist_ok=True);store=a.output/'store';results=[]
env=os.environ|{'CQ_WORKERS':'4','CQ_PROGRESS':'0','CQ_STORE_HOST':str(a.cq.with_name('cq-store-host')),'CQ_RUST_HIR':str(Path(__file__).resolve().parent.parent/'.codequest-tools/rust-hir/bin/cq-rust-hir'),'CQ_GO_TYPES':str(Path(__file__).resolve().parent.parent/'.codequest-tools/go-types/bin/cq-go-types'),'CQ_CLANG':str(a.cq.with_name('cq-clang')),'TMPDIR':'/tmp','CQ_LADYBUG_BUFFER_BYTES':str(128*1024*1024)}
if a.language=='go':
 (source/'go.mod').write_text('module example.test/resume\ngo 1.25\n');(source/'main.go').write_text('package resume\n// Run is the public entry.\nfunc Run() int { return helper() }\n');(source/'helper.go').write_text('package resume\nfunc helper() int { return 7 }\n');build=[]
elif a.language=='rust':
 (source/'src').mkdir(exist_ok=True);(source/'Cargo.toml').write_text('[package]\nname="resume-fixture"\nversion="0.1.0"\nedition="2024"\n[workspace]\n');(source/'src/lib.rs').write_text('mod helper;\n/// Public entry.\npub fn run()->usize{helper::value()}\n');(source/'src/helper.rs').write_text('pub fn value()->usize{7}\n');subprocess.run(['cargo','generate-lockfile','--offline'],cwd=source,check=True,capture_output=True);build=['--','cargo','check','--lib','--locked']
else:
 ext=a.language;(source/'api.h').write_text('int helper(void);\n');(source/f'entry.{ext}').write_text('#include "api.h"\nint run(void){return helper();}\n');(source/f'helper.{ext}').write_text('int helper(void){return 7;}\n');compiler='clang' if a.language=='c' else 'clang++';(source/'compile_commands.json').write_text(json.dumps([{'directory':str(source),'file':f'{name}.{ext}','arguments':[compiler,'-c',f'{name}.{ext}','-o',f'{name}.o']} for name in ['entry','helper']]));build=[]
(source/'README.md').write_text('# Resume fixture\nThe public entry delegates to the helper.\n')
def call(*args,expect=True):
 t=time.monotonic();r=subprocess.run([str(a.cq),*map(str,args),'--store',str(store)],env=env,text=True,capture_output=True,timeout=300)
 row={'args':list(map(str,args)),'exit':r.returncode,'seconds':time.monotonic()-t,'stdout':r.stdout,'stderr':r.stderr};results.append(row);(a.output/'events.json').write_text(json.dumps(results,ensure_ascii=False,indent=2))
 if expect and r.returncode:raise AssertionError(row)
 return json.loads(r.stdout) if not r.returncode else r
if a.continue_run:
 results=json.loads((a.output/'events.json').read_text());runs=call('runs');assert len(runs)==1,runs;run=runs[0]['execution']
 if a.repair_from:
  originals=[json.loads(e['stdout'])['revision'] for e in results if e.get('exit')==0 and e.get('stdout','').lstrip().startswith('{') and 'revision' in json.loads(e['stdout']) and e['args'][0]=='resume']
  call('rerun-stage','--run',run,'--stage',a.repair_from,'--until-stage',a.repair_from);repaired=call('resume','--run',run)
  if originals: assert repaired['revision']==originals[0],repaired
 stages=call('run-status','--run',run)['stages'];assert all(v['attempt']['state']=='completed' for v in stages)
else:
 params=['index','--root',str(source),'--name','fixture','--language',a.language,'--until-stage','capture','--store',str(store),*build]
 t=time.monotonic();r=subprocess.run([str(a.cq),*params],env=env,text=True,capture_output=True,timeout=300);results.append({'args':params,'exit':r.returncode,'seconds':time.monotonic()-t,'stdout':r.stdout,'stderr':r.stderr});(a.output/'events.json').write_text(json.dumps(results,ensure_ascii=False,indent=2));assert r.returncode==0,r.stderr
 first=json.loads(r.stdout);assert first['state']=='paused',first;run=first['execution']
 stages=call('run-status','--run',run)['stages'];assert stages[0]['attempt']['state']=='completed';assert all(v['attempt'] is None for v in stages[1:])
 for value in stages[1:]:
  stage=value['stage'];value=call('resume','--run',run,'--until-stage',stage);assert value['state']=='paused',(stage,value)
  status=call('run-status','--run',run)
  for entry in status['stages']:
   if entry['stage']==stage: assert entry['attempt']['state']=='completed'
  assert all(entry['history'] is None or len(entry['history'])==1 for entry in status['stages']),status
completed=call('resume','--run',run);revision=completed['revision'];assert completed.get('state') in ['complete','complete_with_warnings'];original=call('status','--project','fixture');assert original['revision']==revision
# Directory replay must reuse the recorded encoder, not rerun the compiler.
before_counts={e['stage']:len(e['history']) for e in call('run-status','--run',run)['stages']}
replay=call('rerun-stage','--run',run,'--stage','directories','--until-stage','directories');assert replay['state']=='paused';status=call('run-status','--run',run)
for entry in status['stages']:
 if entry['stage'] in ['capture','native_export','normalize','encode']:assert len(entry['history'])==before_counts[entry['stage']],entry
assert call('status','--project','fixture')['revision']==revision
completed=call('resume','--run',run);assert completed['revision']==revision,completed
# A document-only replay must keep every prior compiler-derived stage intact.
call('rerun-stage','--run',run,'--stage','documents','--until-stage','documents');status=call('run-status','--run',run)
for entry in status['stages']:
 if entry['stage'] in ['capture','materialize','native_export','archive','normalize']:assert len(entry['history'])==before_counts[entry['stage']],entry
completed=call('resume','--run',run);assert completed['revision']==revision
# A changed source may never silently consume an old execution's checkpoints.
f=source/'README.md';before=f.read_text();f.write_text(before+'changed\n');failure=call('resume','--run',run,expect=False);assert isinstance(failure,subprocess.CompletedProcess) and failure.returncode!=0;assert 'ExecutionInputChanged' in failure.stderr;f.write_text(before)
summary={'language':a.language,'execution':run,'revision':revision,'all_stage_boundaries':len(stages),'commands':len(results),'passed':True,'scope':'small real compiler fixture; no full Brocade run'};(a.output/'summary.json').write_text(json.dumps(summary,indent=2));print(json.dumps(summary))
