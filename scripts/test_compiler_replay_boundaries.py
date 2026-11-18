#!/usr/bin/env python3
"""Independent compiler oracles and failure injection for replay boundaries."""
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
a = p.parse_args()
a.cq = a.cq.resolve()
a.output = a.output.resolve()
a.output.mkdir(parents=True, exist_ok=True)
env = os.environ | {'CQ_STORE_HOST': str(a.cq.with_name('cq-store-host'))}
binary = hashlib.sha256(a.cq.read_bytes()).hexdigest()
cases = []

def run(args, environment=env, expected=0):
    start = time.monotonic()
    r = subprocess.run(list(map(str,args)), env=environment, capture_output=True, text=True, timeout=300)
    with (a.output/'events.jsonl').open('a') as f:
        f.write(json.dumps({'args': list(map(str,args)), 'exit':r.returncode, 'stdout':r.stdout,
            'stderr':r.stderr, 'seconds':time.monotonic()-start})+'\n')
    if expected == 0:
        assert r.returncode == 0, r.stderr
    else:
        assert r.returncode != 0, r.stdout
    return r

def cli(store, *args, environment=env, expected=0):
    r = run([a.cq, *args, '--store', store], environment, expected)
    return json.loads(r.stdout) if r.returncode == 0 else r

def index(root, store, partial=False, environment=env, expected=0):
    return cli(store, 'index', '--root', root, '--name', 'boundary', '--language', 'cpp',
        '--compdb', root/'compile_commands.json', '--no-docs',
        *(['--allow-partial'] if partial else []), environment=environment, expected=expected)

def export(store, label):
    target = a.output/(label+'.json')
    cli(store, 'export-snapshot', '--project', 'boundary', '--output', target)
    return json.loads(target.read_text())

def search(store, text, kind=None):
    return cli(store, 'query', '--project', 'boundary', '--json', json.dumps({
        'operation':'search', 'text':'^'+text+'$', 'regex':True, 'limit':100,
        'max_work':100000, 'timeout_ms':30000, **({'kind':kind} if kind else {})}))

def case(name, f):
    start=time.monotonic()
    try:
        evidence=f()
        cases.append({'case':name, 'status':'passed', 'seconds':time.monotonic()-start, 'evidence':evidence})
    except BaseException as error:
        cases.append({'case':name, 'status':'failed', 'seconds':time.monotonic()-start, 'error':repr(error)})
        raise
    finally:
        (a.output/'results.json').write_text(json.dumps({'passed':sum(c['status']=='passed' for c in cases),
            'total':len(cases), 'cases':cases, 'binary_sha256':binary,
            'binary_unchanged':binary==hashlib.sha256(a.cq.read_bytes()).hexdigest()}, indent=2)+'\n')

def driver_languages():
    evidence=[]
    for label, driver, flags, extension, language in [
        ('driver-cxx-c', 'clang++', [], 'c', 'Cpp'),
        ('explicit-cxx', 'clang', ['-x','c++'], 'c', 'Cpp'),
        ('explicit-c', 'clang++', ['-x','c'], 'cpp', 'C'),
    ]:
        root=a.output/label;root.mkdir(exist_ok=True);store=a.output/(label+'-store');file=root/('main.'+extension)
        file.write_text('class Widget {public: int get(){return 1;}};\nint run(){Widget w; return w.get();}\n' if language=='Cpp' else 'struct Widget {int value;};\nint run(void){struct Widget w={1}; return w.value;}\n')
        args=[driver,*flags,'-fsyntax-only',str(file)]
        ast=run([*args,'-Xclang','-ast-dump=json'])
        tree=json.loads(ast.stdout)
        def nodes(n):
            yield n
            for child in n.get('inner',[]): yield from nodes(child)
        kinds={n.get('kind') for n in nodes(tree) if n.get('name')=='Widget'}
        assert ('CXXRecordDecl' in kinds) == (language=='Cpp'), kinds
        (root/'compile_commands.json').write_text(json.dumps([{'directory':str(root),'file':str(file),'arguments':args}]))
        saved=a.output/(label+'-snapshot.json')
        if saved.exists():
            snapshot=json.loads(saved.read_text());result={'revision':snapshot['revision'],'production':snapshot['production']}
        else:
            result=index(root,store);snapshot=export(store,label+'-snapshot')
        assert snapshot['units'][0]['language']==language, snapshot['units']
        rows=search(store,'Widget','Class' if language=='Cpp' else 'Struct')['items']
        assert rows, (language,rows)
        assert not result['production']['partial'], result
        if language=='Cpp': assert search(store,'get','Method')['items']
        evidence.append({'label':label,'real_ast_kinds':sorted(kinds),'effective_language':language,'revision':result['revision'],'entity_kind':rows[0]['kind']})
    return evidence

def directory_contexts():
    root=a.output/'directories';root.mkdir();file=root/'main.cpp'
    file.write_text('#include "config.h"\n#if SELECT == 1\nint first(){return 1;}\n#else\nint second(){return 2;}\n#endif\n')
    entries=[];oracles=[]
    for label,value in [('a',1),('b',2)]:
        cwd=root/label;(cwd/'include').mkdir(parents=True);(cwd/'include/config.h').write_text(f'#define SELECT {value}\n')
        args=['clang++','-std=c++17','-Iinclude','-fsyntax-only',str(file)]
        r=subprocess.run([*args,'-Xclang','-ast-dump=json'],cwd=cwd,capture_output=True,text=True,timeout=30)
        assert r.returncode==0,r.stderr
        ast=json.loads(r.stdout);names=[n.get('name') for n in ast.get('inner',[]) if n.get('kind')=='FunctionDecl']
        assert names==(['first'] if value==1 else ['second']),names
        oracles.append({'directory':str(cwd),'expected_function':names[0],'compiler_exit':r.returncode})
        entries.append({'directory':str(cwd),'file':str(file),'arguments':args})
    (root/'compile_commands.json').write_text(json.dumps(entries));store=a.output/'directory-store';result=index(root,store);snapshot=export(store,'directory-snapshot')
    units={u['id']:u for u in snapshot['units']};contexts={c['entry_unit']:c['id'] for c in snapshot['contexts']}
    assert len(units)==len(contexts)==2,(units,contexts)
    for expected in oracles:
        rows=search(store,expected['expected_function'],'Function')['items'];assert len(rows)==1,rows
        unit=units[rows[0]['unit']];cfg=unit['configuration']['observations']
        assert cfg['working_directory']['display']==expected['directory'],cfg
        expected.update({'unit':unit['id'],'context':contexts[unit['id']]})
    return {'revision':result['revision'],'oracles':oracles}

def diagnostic_quarantine():
    root=a.output/'diagnostics';root.mkdir();file=root/'main.cpp';file.write_text('int healthy(){return 1;}\n')
    (root/'compile_commands.json').write_text(json.dumps([{'directory':str(root),'file':str(file),'arguments':['clang++','-fsyntax-only',str(file)]}]))
    marker=a.output/'diagnostic-mode';proxy=a.output/'clang-proxy.py';worker=Path(env.get('CQ_CLANG',str(a.cq.with_name('cq-clang')))).resolve()
    proxy.write_text('#!/usr/bin/env python3\nimport sys,subprocess,json\nfrom pathlib import Path\nworker='+repr(str(worker))+'\nmarker=Path('+repr(str(marker))+')\nr=subprocess.run([worker,*sys.argv[1:]])\nif r.returncode==0 and len(sys.argv)>2 and marker.exists():\n p=Path(sys.argv[2]);v=json.loads(p.read_text());v.setdefault("diagnostics",[]).append({"severity":marker.read_text().strip(),"message":"injected replay diagnostic"});p.write_text(json.dumps(v))\nsys.exit(r.returncode)\n')
    proxy.chmod(0o755);local=env|{'CQ_CLANG':str(proxy)};store=a.output/'diagnostic-store';baseline=index(root,store,environment=local)['revision']
    marker.write_text('error');file.write_text('int healthy(){return 2;}\n')
    refused=index(root,store,environment=local,expected=1);assert 'NativeAnalysisFailed' in refused.stderr,refused.stderr
    assert cli(store,'status','--project','boundary')['revision']==baseline
    partial=index(root,store,partial=True,environment=local);assert partial['production']['partial'],partial
    assert not search(store,'healthy','Function')['items']
    incomplete=export(store,'diagnostic-partial');assert incomplete['production']['failed_units']
    assert all(s['rows']==0 for s in incomplete['production']['scopes'] if s['key']['unit'] in incomplete['production']['failed_units'])
    marker.write_text('warning');file.write_text('int healthy(){return 3;}\n');warning=index(root,store,environment=local);assert not warning['production']['partial'];assert search(store,'healthy','Function')['items']
    marker.write_text('unknown');file.write_text('int healthy(){return 4;}\n');unknown=index(root,store,environment=local,expected=1);assert 'InvalidDiagnostic' in unknown.stderr,unknown.stderr
    assert cli(store,'status','--project','boundary')['revision']==warning['revision']
    marker.unlink();recovered=index(root,store,environment=local);assert not recovered['production']['partial'];assert search(store,'healthy','Function')['items']
    return {'baseline':baseline,'strict_error_head_unchanged':True,'partial_revision':partial['revision'],'partial_failed_units':list(incomplete['production']['failed_units']),'warning_accepted':True,'unknown_severity_refused':True,'recovered':recovered['revision']}

def response_inputs():
    root=a.output/'responses';root.mkdir();file=root/'main.c';file.write_text('#if MODE == 1\nclass First {};\n#else\nclass Second {};\n#endif\n')
    response=root/'flags.rsp';response.write_text('-x c++ -DMODE=1')
    (root/'compile_commands.json').write_text(json.dumps([{'directory':str(root),'file':str(file),'arguments':['clang','@flags.rsp','-fsyntax-only',str(file)]}]))
    store=a.output/'response-store';first=index(root,store);assert search(store,'First','Class')['items'];response.write_text('-x c++ -DMODE=2')
    changed=cli(store,'refresh','--project','boundary');assert changed['revision']!=first['revision'];assert search(store,'Second','Class')['items'];assert not search(store,'First','Class')['items']
    snapshot=export(store,'response-snapshot');assert any(f['logical_path']['display']==str(response) for f in snapshot['analysis_inputs'])
    return {'first':first['revision'],'changed':changed['revision'],'response_file_retained':True}

case('driver_and_x_language_match_real_clang_ast',driver_languages)
case('same_argv_distinct_cwd_relative_headers_and_contexts',directory_contexts)
case('replay_error_strict_partial_warning_unknown_recovery',diagnostic_quarantine)
case('response_input_change_refreshes_selected_language_and_facts',response_inputs)
print(json.dumps({'passed':len(cases),'total':len(cases)}))
