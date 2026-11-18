#!/usr/bin/env python3
"""Black-box compiler/graph/CLI/MCP acceptance; never silently skips a provider."""
import argparse,json,pathlib,subprocess,tempfile,shutil,time,hashlib,threading,queue
root=pathlib.Path(__file__).resolve().parent.parent
parser=argparse.ArgumentParser();parser.add_argument('--cq',type=pathlib.Path,default=root/'target/debug/cq');parser.add_argument('--output',type=pathlib.Path,required=True);args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=True)
records=[];checks=[]
import atexit
atexit.register(lambda:(args.output/'partial.json').write_text(json.dumps({'checks':checks,'records':records},ensure_ascii=False,indent=2)))
def check(name,value):
    checks.append({'name':name,'passed':bool(value)})
    if not value: raise AssertionError(name)
with tempfile.TemporaryDirectory(prefix='cq-native-') as tmp:
    tmp=pathlib.Path(tmp);store=tmp/'store'
    def cli(*words,ok=True):
        command=[str(args.cq),str(words[0]),'--store',str(store),*map(str,words[1:])];start=time.monotonic();p=subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=600);records.append({'command':command,'seconds':time.monotonic()-start,'exit':p.returncode,'stdout':p.stdout.decode(errors='replace'),'stderr':p.stderr.decode(errors='replace')})
        if ok and p.returncode: raise AssertionError(records[-1])
        return json.loads(p.stdout) if p.returncode==0 else p
    def snapshot(name):
        info=json.loads((store/'projects'/hashlib.sha256(name.encode()).hexdigest()/'project.json').read_text());return json.loads((store/'revisions'/info['head']/'snapshot.json').read_text())
    def q(project,operation,**kw):return cli('query','--project',project,'--json',json.dumps({'operation':operation,**kw}))
    snapshots={};projects={}
    for language,fixture in [('rust','understanding-rust'),('go','understanding-go'),('cpp','understanding-cpp')]:
        project=tmp/fixture;shutil.copytree(root/'fixtures'/fixture,project,ignore=shutil.ignore_patterns('target'));projects[language]=project;extra=[]
        if language=='cpp':
            compdb=tmp/'compile_commands.json';compdb.write_text(json.dumps([{'directory':str(project),'file':str(project/f),'arguments':['clang++' if f.endswith('cpp') else 'clang','-c',str(project/f),'-o',str(tmp/(f+'.o'))]} for f in ['events.cpp','plain.c']]));extra=['--compdb',str(compdb)]
        if language=='rust':
            (project/'guide.md').write_text('# Guide\nSee [wire](#wire-format).\n\n## Wire format\nMessages preserve byte values.\n')
            # Minimal text-layer PDF with a valid xref, independent of optional Python packages.
            objects=[b'<< /Type /Catalog /Pages 2 0 R >>',b'<< /Type /Pages /Kids [3 0 R] /Count 1 >>',b'<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>',b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>']
            stream=b'BT /F1 12 Tf 30 250 Td (Frozen protocol guide) Tj ET';objects.append(b'<< /Length '+str(len(stream)).encode()+b' >>\nstream\n'+stream+b'\nendstream');pdf=b'%PDF-1.4\n';offsets=[0]
            for i,obj in enumerate(objects,1):offsets.append(len(pdf));pdf+=str(i).encode()+b' 0 obj\n'+obj+b'\nendobj\n'
            xref=len(pdf);pdf+=b'xref\n0 6\n0000000000 65535 f \n'+b''.join(f'{off:010d} 00000 n \n'.encode() for off in offsets[1:])+b'trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n'+str(xref).encode()+b'\n%%EOF\n';(project/'protocol.pdf').write_bytes(pdf)
        summary=cli('index','--root',project,'--name',language,*extra);d=snapshot(language);snapshots[language]=d;(args.output/(language+'-snapshot.json')).write_text(json.dumps(d,ensure_ascii=False,indent=2))
        check(language+' real successful build',all(b['outcome']=='Succeeded' for b in d['builds']))
        check(language+' actual process evidence',any(e['event']['kind']=='process_started' for b in d['builds'] for e in b['events']))
        check(language+' frozen source',len(d['sources']['files'])>0)
        check(language+' native type navigation',any(e.get('type_navigation') for e in d['entities']))
        check(language+' argument to parameter binding',any(a['parameter']['state']=='Resolved' for o in d['occurrences'] for a in o.get('argument_bindings',[])))
        check(language+' parameter flow is separately derived',any(r['kind']=='PassesValue' and d['provenance_records'][r['provenance']]['authority']=='RuleDerived' for r in d['relations']))
        entities={e['id']:e for e in d['entities']}
        src='Run' if language=='go' else 'run';dst='Emit' if language=='go' else 'emit'
        calls=[r for r in d['relations'] if r['kind']=='Calls' and entities[r['from']]['name']==src and entities[r['to']]['name']==dst]
        check(language+' two distinct direct-call occurrences',len(calls)==2 and len({r['source']['start'] for r in calls})==2)
        check(language+' closure remains separate',any(r['kind']=='Calls' and entities[r['from']]['kind']=='Body' and entities[r['to']]['name']==dst for r in d['relations']))
        check(language+' interface or virtual dispatch is not direct CALLS',any(r['kind']=='Dispatches' and entities[r['to']]['name'] in ['Write','write'] for r in d['relations']))
        check(language+' callback uncertainty retained',any(o['kind']=='Calls' and o['executable']['state']=='Unknown' for o in d['occurrences']))
        for op in ['overview','schema','contexts','coverage','entrypoints','hotspots','cycles','clusters']:
            check(language+' '+op,q(language,op)['revision']==d['revision'])
        check(language+' docs linked to graph',len(d['documents'])>0 and any(r['kind']=='Documents' for r in d['relations']))
    rust=snapshots['rust'];ent={e['id']:e for e in rust['entities']};run=next(e for e in ent.values() if e['name']=='run' and e['kind']=='Function');emit=next(e for e in ent.values() if e['name']=='emit' and e['kind']=='Function')
    check('PDF text layer frozen and indexed',any(d['kind']=='PdfPage' and 'Frozen protocol guide' in d['text'] for d in rust['documents']))
    check('Markdown section anchor resolves to section',any(link['destination']=='#wire-format' and link['target']['state']=='Resolved' for d in rust['documents'] for link in d['links']))
    check('Rust const/static initializer boundaries',all(any(b['kind']==kind and b['evaluation']=='Constant' for b in rust['bodies']) for kind in ['ConstInitializer','StaticInitializer']))
    check('Rust async body deferred',any(b['kind']=='AsyncBlock' and b['evaluation']=='Deferred' for b in rust['bodies']))
    check('Rust cfg inactive function absent',not any(e['name']=='inactive' for e in ent.values()))
    check('Rust associated function has direct callable identity',any(r['kind']=='Calls' and ent[r['from']]['name']=='run' and ent[r['to']]['name']=='new' and ent[r['to']]['kind']=='Method' for r in rust['relations']))
    check('Rust self receiver is a parameter',any(e['name']=='self' and e['kind']=='Parameter' for e in ent.values()))
    check('Rust native implementations',any(r['kind']=='Implements' for r in rust['relations']))
    check('crate inner rustdoc retained with native link',any(d['kind']=='Rustdoc' and 'Event output lifecycle' in d['text'] and any(link['target']['state']=='Resolved' for link in d['links']) for d in rust['documents']))
    check('block rustdoc is not duplicated as ordinary comment',any(d['kind']=='Rustdoc' and 'A block document' in d['text'] for d in rust['documents']) and not any(d['kind']=='CodeComment' and 'A block document' in d['text'] for d in rust['documents']))
    check('inherited trait documentation is not falsely attributed',not any(d['kind']=='Rustdoc' and 'sink protocol' in d['text'] and 'impl::' in ent.get(d.get('owner'),{}).get('qualified_name','') for d in rust['documents']))
    check('rustdoc native resolution',any(link['authority']=='SemanticResolution' and link['target']['state']=='Resolved' for d in rust['documents'] for link in d['links']))
    check('author inline comment preserved',any(d['kind']=='CodeComment' and 'on purpose' in d['text'] for d in rust['documents']))
    details=q('rust','details',subject=run['id']);check('details discloses counts and followups',details['items'][0]['counts']['bodies']>=3 and bool(details['items'][0]['hidden']))
    check('graph tour expands ranked native seeds',bool(q('rust','tour',text='emit')['items']))
    original=q('rust','source',subject=emit['id']);old_revision=rust['revision'];check('source starts at symbol',any('pub fn emit' in r['text'] for r in original['items']))
    page=q('rust','search',text='',limit=1);count=0
    while True:
        count+=len(page['items'])
        if not page.get('next_cursor'):break
        page=q('rust','search',text='',limit=1,cursor=page['next_cursor'])
    check('search pages are exhaustive',count>7)
    a=cli('annotate','--project','rust','--json',json.dumps({'kind':'AuthorRationale','text':'The writer is independent of lifecycle management.','entities':[emit['id']],'evidence':[emit['extent']]}))
    path=projects['rust']/'src/events.rs';path.write_text(path.read_text().replace('_value: usize','_changed: usize'))
    (projects['rust']/'new-note.md').write_text('# New note\nAn additional explanation.\n')
    changes=q('rust','changes');check('changes notices new and modified files',changes['stale'] and len(changes['added'])==1 and bool(changes['modified']))
    check('old source stays frozen',q('rust','source',subject=emit['id'])['items']==original['items'])
    fresh=cli('refresh','--project','rust');check('refresh publishes a new revision',fresh['revision']!=old_revision)
    annotations=q('rust','annotations');check('dependent interpretation becomes stale',any(x['id']==a['id'] and x['status']=='Stale' for x in annotations))
    comparison=q('rust','compare',base=old_revision);check('revision comparison has changed files',bool(comparison['modified_files']))
    same=q('rust','compare',base=old_revision,target=old_revision);check('comparison honors explicit target revision',not same['modified_files'] and not same['added_files'] and not same['removed_files'])
    repl=subprocess.run([str(args.cq),'repl','--store',str(store)],input=f'use rust\nsearch emit\nnext\nexpand {next(e["id"] for e in snapshot("rust")["entities"] if e["name"]=="emit" and e["kind"]=="Function")}\ncollapse {emit["id"]}\nquit\n',text=True,capture_output=True,timeout=30);check('REPL interactive command session',repl.returncode==0 and 'collapsed' in repl.stdout and 'hidden' in repl.stdout)
    (args.output/'repl.txt').write_text(repl.stdout)
    peer=subprocess.Popen([str(args.cq),'mcp','--store',str(store)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1);responses=queue.Queue();threading.Thread(target=lambda:[responses.put(json.loads(line)) for line in peer.stdout],daemon=True).start();seq=0
    def rpc(method,params):
        global seq
        seq+=1;peer.stdin.write(json.dumps({'jsonrpc':'2.0','id':seq,'method':method,'params':params})+'\n');peer.stdin.flush();r=responses.get(timeout=60);check('MCP response '+str(seq),r.get('id')==seq and 'result' in r);records.append({'mcp_request':{'method':method,'params':params},'response':r});return r['result']
    rpc('initialize',{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'native-acceptance','version':'1'}});tools=rpc('tools/list',{})['tools'];check('MCP includes both Code KG capability surfaces',len(tools)>=19)
    def tool(tool_name,**kw):
        r=rpc('tools/call',{'name':tool_name,'arguments':kw});check('MCP '+tool_name+' succeeds',not r.get('isError'));return json.loads(r['content'][0]['text'])
    tool('list_projects');tool('search_graph',project='rust',text='emit');tool('get_graph_schema',project='rust');tool('get_architecture',project='rust');tool('check_index_coverage',project='rust');tool('detect_changes',project='rust');tool('documents',project='rust');tool('manage_adr',project='rust',action='list');tool('inspect_code_graph',project='rust',request={'type':'entrypoints'})
    graph=tool('query_graph',project='rust',text='MATCH (a:Function) RETURN a.name ORDER BY a.name',limit=1);check('Cypher continuation available',bool(graph['next_cursor']));second=tool('query_graph',project='rust',text='MATCH (a:Function) RETURN a.name ORDER BY a.name',limit=1,cursor=graph['next_cursor']);check('Cypher next page differs',graph['rows']!=second['rows'])
    page=tool('query_codequest',project='rust',operation='search',text='',limit=1)
    continued=tool('query_codequest',project='rust',operation='next',cursor=page['next_cursor'],limit=1)
    check('generic next retains original scope',page['revision']==continued['revision'] and page['items']!=continued['items'])
    job=tool('index_repository',root=str(projects['go']),name='go-async')
    deadline=time.monotonic()+90
    while True:
        state=tool('index_status',job=job['job'])['status']
        if state['state']!='running':break
        if time.monotonic()>deadline:raise AssertionError('background indexing deadline')
        time.sleep(0.5)
    check('background indexing completes with native graph',state['state']=='complete' and state['result']['entities']>0)
    cancelled=tool('index_repository',root=str(projects['go']),name='go-cancel')
    tool('index_status',job=cancelled['job'],cancel=True)
    deadline=time.monotonic()+30
    while True:
        state=tool('index_status',job=cancelled['job'])['status']
        if state['state']!='running':break
        if time.monotonic()>deadline:raise AssertionError('job cancellation deadline')
        time.sleep(0.1)
    check('compiler job cancellation reaches terminal state',state['state']=='cancelled')
    check('cancelled index never publishes head',not (store/'projects'/hashlib.sha256(b'go-cancel').hexdigest()/'project.json').exists())
    current=snapshot('rust');run2=next(e for e in current['entities'] if e['name']=='run' and e['kind']=='Function');emit2=next(e for e in current['entities'] if e['name']=='emit' and e['kind']=='Function')
    runtime=tool('ingest_traces',project='rust',source='controlled-acceptance',events=[{'from':run2['id'],'to':emit2['id'],'count':2}])
    check('runtime evidence does not replace static calls',runtime['static_calls']==sum(r['kind']=='Calls' for r in current['relations']) and runtime['runtime_traces']==1)
    runtime_page=tool('ingest_traces',project='rust');check('runtime evidence can be queried',len(runtime_page['items'])==1)
    peer.stdin.close();peer.wait(timeout=15);check('MCP clean protocol exit',peer.returncode==0 and responses.empty())
    multi=tmp/'compile-workspace';shutil.copytree(root/'fixtures/compile-workspace',multi,ignore=shutil.ignore_patterns('target'))
    cli('index','--root',multi,'--name','multi','--','cargo','check','--workspace','--all-targets','--offline')
    md=snapshot('multi');(args.output/'multi-snapshot.json').write_text(json.dumps(md,ensure_ascii=False,indent=2));files={f['id']:f for f in md['sources']['files']}
    values=[e for e in md['entities'] if e['name']=='value' and e.get('source') and files[e['source']['file_revision']]['logical_path']['display'].endswith('/shared.rs')]
    check('same source distinct compilation identities',len({e['unit'] for e in values})>=2 and len({e['id'] for e in values})>=2)
    check('generated source retained',any('generated.rs' in f['logical_path']['display'] for f in md['sources']['files']))
    summary={'schema_version':1,'checks':checks,'records':records,'source_commit':subprocess.check_output(['git','-C',str(root),'rev-parse','HEAD'],text=True).strip(),'note':'Tests use temporary repository copies and real installed compiler providers. Sources are removed after validation; transcripts retain results.'};(args.output/'acceptance.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2));print(json.dumps({'checks':len(checks),'passed':all(c['passed'] for c in checks),'output':str(args.output)}))
