#!/usr/bin/env python3
"""Rebuild native indexes, then replay frozen real-compiler structural queries.
Expected semantic rows were independently checked by test_language_structures.
No compiler or live source files are used by this storage-regression block.
"""
import argparse,copy,hashlib,json,os,subprocess,time
from pathlib import Path
ROOT=Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--cq',type=Path,default=ROOT/'target/release/cq')
p.add_argument('--source',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
a=p.parse_args();a.cq=a.cq.resolve();a.output.mkdir(parents=True,exist_ok=True)
env=os.environ|{'CQ_STORE_HOST':str(a.cq.with_name('cq-store-host'))}
cases=[]
def page(response):
    r=response.get('result',{})
    try:return json.loads(r['content'][0]['text'])
    except (KeyError,IndexError,ValueError):return r

def meaning(value):
    # Latency/cache counters and fresh cursor IDs are observations, not rows.
    return {k:v for k,v in value.items() if k not in ['elapsed_ms','stats','read_stats','next_cursor','new_head','freshness']}

for directory in sorted(a.source.iterdir()):
    record=directory/'index.json';queries=directory/'queries.jsonl';store=directory/'store'
    if not (record.exists()and queries.exists()and store.exists()):continue
    summary=json.loads(json.loads(record.read_text())['stdout']);name=summary['project'];cursor_map={}
    now=time.monotonic()
    rebuild=subprocess.run([str(a.cq),'rebuild-store','--store',str(store),'--project',name],env=env,capture_output=True,text=True,timeout=600)
    (a.output/(directory.name+'-rebuild.json')).write_text(json.dumps({'exit':rebuild.returncode,'stdout':rebuild.stdout,'stderr':rebuild.stderr,'seconds':time.monotonic()-now},indent=2))
    if rebuild.returncode:raise AssertionError(rebuild.stderr)
    errors=(a.output/(directory.name+'.stderr')).open('w')
    peer=subprocess.Popen([str(a.cq),'mcp','--store',str(store)],env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=errors,text=True,bufsize=1)
    sequence=0
    def rpc(method,params):
        global sequence
        sequence+=1;peer.stdin.write(json.dumps({'jsonrpc':'2.0','id':sequence,'method':method,'params':params})+'\n');peer.stdin.flush()
        response=json.loads(peer.stdout.readline());assert response['id']==sequence;return response
    try:
        rpc('initialize',{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'frozen-query-replay','version':'1'}})
        for line_number,line in enumerate(queries.open(),1):
            old=json.loads(line);request=old['request']
            if request['method']!='tools/call' or request['params']['name']!='query_codequest':continue
            expected=page(old['response']);params=copy.deepcopy(request['params']);args=params['arguments']
            if args.get('operation')in ['overview','status','freshness']:continue
            if isinstance(expected,dict) and expected.get('revision'):args['revision']=expected['revision']
            if args.get('cursor')in cursor_map:args['cursor']=cursor_map[args['cursor']]
            actual=page(rpc('tools/call',params));left,right=meaning(expected),meaning(actual)
            ok=left==right and bool(expected.get('next_cursor'))==bool(actual.get('next_cursor'))
            if expected.get('next_cursor')and actual.get('next_cursor'):cursor_map[expected['next_cursor']]=actual['next_cursor']
            case={'fixture':directory.name,'line':line_number,'operation':args.get('operation'),'passed':ok,'revision':actual.get('revision')}
            cases.append(case)
            if not ok:
                (a.output/'difference.json').write_text(json.dumps({'request':params,'expected':left,'actual':right},indent=2))
                raise AssertionError(f'{directory.name}:{line_number}: semantic output differs')
            if any(actual.get(k,{}).get('full_snapshot_decodes',0) for k in ['stats','read_stats']):raise AssertionError('full snapshot decode')
    finally:
        peer.stdin.close();peer.wait(timeout=30);peer.stdout.close();errors.close()
        (a.output/'results.json').write_text(json.dumps({'passed':sum(c['passed']for c in cases),'total':len(cases),'cases':cases,'scope':'Actual new Ladybug rebuild plus exact semantic query replay from previously independently asserted compiler fixtures; no compilation, no live source fallback'},indent=2))
print(json.dumps({'passed':len(cases),'fixtures':len(set(c['fixture']for c in cases))}))
