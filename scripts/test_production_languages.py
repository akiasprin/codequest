#!/usr/bin/env python3
"""Real compiler changes versus independent full builds, one language per block."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parent.parent
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--cq', type=Path, default=ROOT/'target/debug/cq')
p.add_argument('--output', type=Path, required=True)
p.add_argument('--language', choices=['rust','go','c','cpp'], required=True)
p.add_argument('--only-deletion',action='store_true',help='Resume only the final unit-exit case on an existing fixture')
p.add_argument('--resume-from',choices=['S4_compiler_configuration','F_failure_partial_recovery','D_authoritative_unit_exit'],help='Keep preceding passed cases and resume on the same captured fixture')
a = p.parse_args(); a.cq = a.cq.resolve(); a.output = a.output.resolve(); a.output.mkdir(parents=True, exist_ok=True)
ENV = os.environ | {'CQ_STORE_HOST':str(a.cq.with_name('cq-store-host'))}
source=a.output/'source';store=a.output/'store';name='r4-'+a.language
cases=[];binary=hashlib.sha256(a.cq.read_bytes()).hexdigest()
order=['S1_no_change','S2_function_body','S3_public_interface','S4_compiler_configuration','F_failure_partial_recovery','D_authoritative_unit_exit']
if a.resume_from:
    previous=json.loads((a.output/'results.json').read_text())
    (a.output/('results-before-'+a.resume_from+'.json')).write_text(json.dumps(previous,indent=2)+'\n')
    cases=[c|{'binary_sha256':previous['binary_sha256'],'retained_from_previous_block':True} for c in previous['cases'] if c['status']=='passed' and order.index(c['case'])<order.index(a.resume_from)]

def check(ok, message):
    if not ok: raise AssertionError(message)

def cli(where,*args,ok=True):
    start=time.monotonic()
    params=list(map(str,args));at=params.index('--') if '--' in params else len(params);params[at:at]=['--store',str(where)]
    r=subprocess.run([str(a.cq),*params],env=ENV,capture_output=True,text=True,timeout=600)
    with (a.output/'events.jsonl').open('a') as f:
        f.write(json.dumps({'args':list(map(str,args)),'store':str(where),'exit':r.returncode,'stdout':r.stdout,'stderr':r.stderr,'seconds':time.monotonic()-start})+'\n')
    if ok: check(r.returncode==0,r.stderr)
    return json.loads(r.stdout) if r.returncode==0 else r

def case(case_name,fn):
    if a.only_deletion and not case_name.startswith('D_'):return
    if a.resume_from and order.index(case_name)<order.index(a.resume_from):return
    start=time.monotonic()
    try: evidence=fn(); cases.append({'case':case_name,'status':'passed','seconds':time.monotonic()-start,'evidence':evidence,'binary_sha256':binary})
    except BaseException as e:
        cases.append({'case':case_name,'status':'failed','seconds':time.monotonic()-start,'error':repr(e)}); raise
    finally: save()

def save():
    (a.output/'results.json').write_text(json.dumps({'language':a.language,'passed':sum(c['status']=='passed' for c in cases),'total':len(cases),'cases':cases,'binary_sha256':binary,'binary_unchanged':binary==hashlib.sha256(a.cq.read_bytes()).hexdigest()},indent=2)+'\n')

def setup():
    source.mkdir()
    if a.language=='rust':
        (source/'Cargo.toml').write_text('[workspace]\nmembers=["api","app","control"]\nresolver="3"\n')
        for crate in ['api','app','control']:
            (source/crate/'src').mkdir(parents=True)
            text=f'[package]\nname="{crate}"\nversion="0.1.0"\nedition="2024"\n'
            if crate=='app': text+='[dependencies]\napi={path="../api"}\n[features]\nalternate=[]\n'
            (source/crate/'Cargo.toml').write_text(text)
        (source/'api/src/lib.rs').write_text('pub fn target()->i32 {1}\npub fn other()->i32 {2}\n')
        (source/'app/src/lib.rs').write_text('pub fn run()->i32 {api::target()}\n#[cfg(feature="alternate")] pub fn alternate()->i32 {3}\n')
        (source/'control/src/lib.rs').write_text('pub fn control()->i32 {4}\n')
    elif a.language=='go':
        (source/'go.mod').write_text('module example.test/r4\ngo 1.25\n')
        for package in ['api','app','control']: (source/package).mkdir()
        (source/'api/api.go').write_text('package api\nfunc Target() int {return 1}\nfunc Other() int {return 2}\n')
        (source/'app/app.go').write_text('package app\nimport "example.test/r4/api"\nfunc Run() int {return api.Target()}\n')
        (source/'app/alternate.go').write_text('//go:build alt\npackage app\nfunc Alternate() int {return 3}\n')
        (source/'control/control.go').write_text('package control\nfunc Control() int {return 4}\n')
    else:
        ext='c' if a.language=='c' else 'cpp'
        (source/'api.h').write_text('int Target(void);\nint Other(void);\n')
        (source/f'api.{ext}').write_text('#include "api.h"\nint Target(void){return 1;}\nint Other(void){return 2;}\n')
        (source/f'app.{ext}').write_text('#include "api.h"\nint Run(void){return Target();}\n#ifdef ALT\nint Alternate(void){return 3;}\n#endif\n')
        (source/f'control.{ext}').write_text('int Control(void){return 4;}\n')
        compdb(False)

def compdb(alternate,control=True):
    ext='c' if a.language=='c' else 'cpp'; compiler='clang' if a.language=='c' else 'clang++'
    rows=[]
    for file in ['api','app']+(['control'] if control else []):
        args=[compiler,'-std=c11' if ext=='c' else '-std=c++17','-fsyntax-only',str(source/f'{file}.{ext}')]
        if alternate: args.append('-DALT=1')
        rows.append({'directory':str(source),'file':str(source/f'{file}.{ext}'),'arguments':args})
    (source/'compile_commands.json').write_text(json.dumps(rows))

def index(where=store,alternate=False,partial=False,ok=True):
    args=['index','--root',source,'--name',name,'--language',a.language,'--no-docs']
    if partial:args+=['--allow-partial']
    if a.language in ['c','cpp']:args+=['--compdb',source/'compile_commands.json']
    build=[]
    if a.language=='rust':build=['check','--workspace','--offline']+(['--features','app/alternate'] if alternate else [])
    elif a.language=='go':build=['build','-a']+(['-tags=alt'] if alternate else [])+['./...']
    # Store must precede the compiler separator.
    return cli(where,*args,*(['--',*build] if build else []),ok=ok)

def snapshot(revision,where=store,label='snapshot'):
    path=a.output/(label+'-'+revision+'.json');cli(where,'export-snapshot','--project',name,'--revision',revision,'--output',path)
    return json.loads(path.read_text())

def graph(s):return {family:sorted(s[family],key=lambda v:v['id']) for family in ['entities','bodies','occurrences','relations','documents']}

def oracle(label,alternate=False):
    current=cli(store,'status','--project',name)['revision'];actual=snapshot(current,label=label+'-incremental')
    where=a.output/('oracle-'+label);full=index(where,alternate);expected=snapshot(full['revision'],where,label+'-full')
    left,right=graph(actual),graph(expected)
    if left!=right:
        diffs={k:{'incremental_count':len(left[k]),'full_count':len(right[k]),'only_incremental':[v for v in left[k] if v not in right[k]][:8],'only_full':[v for v in right[k] if v not in left[k]][:8]} for k in left if left[k]!=right[k]}
        (a.output/(label+'-difference.json')).write_text(json.dumps(diffs,indent=2))
        raise AssertionError('incremental facts differ from independent full build: '+label)
    return {'revision':current,'full_revision':full['revision'],'family_counts':{k:len(v) for k,v in left.items()},'identity_unmatched':actual['production']['identity_unmatched']}

def object_bytes(reference):
    path=store/'format-2/objects'/reference['hash'][:2]/reference['hash'];value=path.read_bytes()
    check(len(value)==reference['bytes'] and hashlib.sha256(value).hexdigest()==reference['hash'],'bad manifest object');return value

def fact_refs(revision):
    project=store/'format-2/projects'/hashlib.sha256(name.encode()).hexdigest();manifest=json.loads((project/'commits'/revision).read_text());out={}
    def visit(root):
        if root is None:return
        node=json.loads(object_bytes(root))
        if 'Leaf' in node:
            for key,value in node['Leaf'].items():out[key.split(':',1)[1]]=value
        else:
            for child in node['Branch'].values():visit(child)
    visit(manifest['roots']['facts']);return out

def app_path():return source/('app/src/lib.rs' if a.language=='rust' else 'app/app.go' if a.language=='go' else 'app.'+('c' if a.language=='c' else 'cpp'))

def query(operation,**kwargs):return cli(store,'query','--project',name,'--json',json.dumps({'operation':operation,'limit':100,'max_work':100000,'timeout_ms':30000,**kwargs}))

def main():
    if not source.exists():setup()
    initial=cli(store,'status','--project',name) if a.only_deletion or a.resume_from else index();first=initial['revision'];before=snapshot(first,label='initial-resume' if a.resume_from else 'initial')
    check(before['production']['scopes'],'missing native scope publication')
    def noop():
        r=cli(store,'refresh','--project',name);check(r['revision']==first and r['update']['mode']=='checkpoint_unchanged',str(r));check(r['update']['fact_pages_written']==0 and r['update']['projection_rows_submitted']==0,str(r));return r['update']
    case('S1_no_change',noop)
    def body():
        path=app_path();path.write_text(path.read_text().replace('target()','other()').replace('Target()','Other()'))
        changed=index();new=snapshot(changed['revision'],label='body');old_refs,new_refs=fact_refs(first),fact_refs(changed['revision'])
        controls={e['id'] for e in before['entities'] if 'control' in e['qualified_name'].lower() or e['name'].lower()=='control'}
        control_units={e['unit'] for e in before['entities'] if e['id'] in controls};controls={e['id'] for e in before['entities'] if e['unit'] in control_units}
        keys=['entity:'+e['id'] for e in before['entities'] if e['id'] in controls]+['body:'+b['id'] for b in before['bodies'] if b['unit'] in control_units]+['occurrence:'+o['id'] for o in before['occurrences'] if o['unit'] in control_units]+['relation:'+r['id'] for r in before['relations'] if r['from'] in controls]
        check(keys,'missing independent control facts');check(all(old_refs[k]==new_refs.get(k) for k in keys),'unrelated control payload page/offset changed')
        run=next(e for e in new['entities'] if e['name'].lower()=='run' and e['kind']=='Function');callees=query('callees',subject=run['id'])
        names={row['entity']['name'].lower() for row in callees['items']};check('other' in names and 'target' not in names,str(names))
        result=oracle('body');result['control_fact_refs_reused']=len(keys);return result
    case('S2_function_body',body)
    def interface():
        if a.language=='rust':
            path=source/'api/src/lib.rs';path.write_text(path.read_text().replace('target()->i32 {1}','target(value:i32)->i32 {value}'));app_path().write_text(app_path().read_text().replace('api::other()','api::target(7)'))
        elif a.language=='go':
            path=source/'api/api.go';path.write_text(path.read_text().replace('Target() int {return 1}','Target(value int) int {return value}'));app_path().write_text(app_path().read_text().replace('api.Other()','api.Target(7)'))
        else:
            path=source/'api.h';path.write_text(path.read_text().replace('Target(void)','Target(int value)'));path=source/('api.c' if a.language=='c' else 'api.cpp');path.write_text(path.read_text().replace('Target(void){return 1;}','Target(int value){return value;}'));app_path().write_text(app_path().read_text().replace('Other()','Target(7)'))
        index();return oracle('interface')
    case('S3_public_interface',interface)
    def configuration():
        check(not any(e['name'].lower()=='alternate' for e in snapshot(cli(store,'status','--project',name)['revision'],label='before-config')['entities']),'alternate present before selection')
        if a.language in ['c','cpp']:compdb(True)
        r=index(alternate=True);new=snapshot(r['revision'],label='config');check(any(e['name'].lower()=='alternate' for e in new['entities']),'alternate missing after config change');return oracle('configuration',True)
    case('S4_compiler_configuration',configuration)
    def failure():
        old=cli(store,'status','--project',name)['revision'];path=app_path();good=path.read_text();path.write_text(good+'\nTHIS IS INVALID SOURCE\n')
        rejected=index(alternate=True,ok=False);check(isinstance(rejected,subprocess.CompletedProcess) and rejected.returncode!=0,'strict build falsely succeeded');check(cli(store,'status','--project',name)['revision']==old,'failed build advanced HEAD')
        partial=index(alternate=True,partial=True);check(partial['production']['partial'],'partial state missing');check(not any(e['name'].lower()=='run' for e in snapshot(partial['revision'],label='partial')['entities']),'failed scope retained stale run')
        answer=query('search',text='^run$',regex=True);check(answer['state']=='unknown_due_to_incomplete_compilation' and any(g['code']=='IncompleteKnowledge' for g in answer['gaps']),str(answer))
        path.write_text(good);index(alternate=True);result=oracle('recovery',True);result['strict_old_head']=old;result['partial_revision']=partial['revision'];return result
    case('F_failure_partial_recovery',failure)
    def deletion():
        if a.language=='rust':
            (source/'Cargo.toml').write_text('[workspace]\nmembers=["api","app"]\nexclude=["control"]\nresolver="3"\n')
            # Prepare the complete requested input change before capture. Cargo
            # must not rewrite an already frozen lockfile during observation.
            locked=subprocess.run(['cargo','generate-lockfile','--offline'],cwd=source,env=ENV,capture_output=True,text=True,timeout=30)
            check(locked.returncode==0,locked.stderr)
        elif a.language=='go':(source/'control/control.go').unlink()
        else:compdb(True,False)
        r=index(alternate=True);new=snapshot(r['revision'],label='deleted');check(not any(e['name'].lower()=='control' for e in new['entities']),'exited unit retained');return oracle('deletion',True)
    case('D_authoritative_unit_exit',deletion)

try:main()
finally:save()
