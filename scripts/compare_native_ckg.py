#!/usr/bin/env python3
"""Compare independently specified invariants in actual native CKG/CQ dumps.

Input CodeQuest stores come from test_language_structures; CKG dumps must name
strict native providers. Dumps from static/LSP fallback are rejected.
"""
import argparse
import json
from pathlib import Path

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--ckg',type=Path,required=True)
p.add_argument('--ours',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
a=p.parse_args()

def ours(directory):
    store=directory/'store'
    infos=list((store/'projects').glob('*/project.json'))
    if len(infos)!=1: raise ValueError('one fixture project expected')
    head=json.loads(infos[0].read_text())['head']
    d=json.loads((store/'revisions'/head/'snapshot.json').read_text())
    files={f['id']:f['logical_path']['display'] for f in d['sources']['files']}
    nodes={e['id']:{'id':e['id'],'name':e['name'],'file':files.get((e.get('source') or {}).get('file_revision'),''),'qualifiedName':e['qualified_name'],'kind':e['kind']} for e in d['entities']}
    return nodes,[{'from':e['from'],'to':e['to'],'kind':e['kind'].lower()} for e in d['relations']]

def upstream(file,provider):
    d=json.loads(file.read_text())
    if not any(p['provider']==provider for p in d['provenance']): raise ValueError('native provider provenance required')
    return {e['id']:e for e in d['nodes']},d['edges']

results=[]
for language,cls,provider,file in [('rust','RustStructures','samchon-rust-analyzer-hir','rust.json'),('go','GoStructures','samchon-graph-go','go-native.json')]:
    left=upstream(a.ckg/file,provider);right=ours(a.ours/cls)
    def neighbors(graph,caller,file_tail=None):
        ns,es=graph
        callers=[n for n in ns.values() if n['name']==caller and (file_tail is None or n.get('file','').endswith(file_tail))]
        if len(callers)!=1:raise ValueError(f'{language}: ambiguous caller {caller}')
        return [ns[e['to']] for e in es if e['from']==callers[0]['id'] and e['kind'] in ['calls','dispatches']]
    scenarios= [
        ('alias call stays in left.rs','run',lambda rows:any(n['name']=='dispatch' and n.get('file','').endswith('left.rs') for n in rows)),
        ('same-name right dispatch is absent','run',lambda rows:not any(n['name']=='dispatch' and n.get('file','').endswith('right.rs') for n in rows)),
        ('function value is not call','function_value',lambda rows:not any(n['name']=='dispatch' for n in rows)),
        ('closure/async work is not immediate parent call','deferred',lambda rows:not any(n['name']=='dispatch' for n in rows)),
    ] if language=='rust' else [
        ('both package dispatch identities retained','Run',lambda rows:len({n['id'] for n in rows if n['name']=='Dispatch'})==2),
        ('promoted and own receiver method distinct','Run',lambda rows:len({n['id'] for n in rows if n['name']=='Handle'})==2),
        ('method value is not call','MethodValue',lambda rows:not any(n['name']=='Handle' for n in rows)),
        ('paren and indexed call forms bind targets','WrappedCalls',lambda rows:{n['name'] for n in rows}=={'Dispatch','Generic'} and len({n['id'] for n in rows if n['name']=='Dispatch'})==2),
        ('type conversion is not call','TypeConversion',lambda rows:not rows),
    ]
    for label,caller,predicate in scenarios:
        l,r=neighbors(left,caller),neighbors(right,caller)
        results.append({'language':language,'invariant':label,'caller':caller,'ckg':bool(predicate(l)),'codequest':bool(predicate(r)),
            'ckg_targets':[{'name':n['name'],'file':n.get('file'),'qualified':n.get('qualifiedName')} for n in l],
            'codequest_targets':[{'name':n['name'],'file':n.get('file'),'qualified':n.get('qualifiedName')} for n in r]})
a.output.write_text(json.dumps({'notice':'Exact listed invariants on the same controlled source; not graph-wide recall/precision or proof of author intent. Native C++ comparison unavailable.','results':results},indent=2))
print(json.dumps([(r['invariant'],r['ckg'],r['codequest']) for r in results]))
