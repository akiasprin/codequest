#!/usr/bin/env python3
"""Store-only R5: actual Ladybug histories, multi-project queues, maintenance,
process crashes, and 10x independent facts. Raw samples are always retained.
Compiler/export costs are deliberately measured by separate native blocks.
"""
import argparse, concurrent.futures as cf, copy, hashlib, json, math, os, shutil, signal, socket, struct, subprocess, threading, time
from pathlib import Path
ROOT=Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--cq',type=Path,default=ROOT/'target/debug/cq');p.add_argument('--output',type=Path,required=True);p.add_argument('--only',choices=['matrix','maintenance','scale','migration'],action='append');a=p.parse_args();a.cq=a.cq.resolve();a.output=a.output.resolve();a.output.mkdir(parents=True,exist_ok=True)
env=os.environ|{'CQ_STORE_HOST':str(a.cq.with_name('cq-store-host'))};cases=[];hosts=set();peaks={};stop=threading.Event()
# The legacy writer runs as a Cargo test executable under debug/deps, while the
# other writer is cq under debug. Bind both to the same embedding model rather
# than letting installation-relative discovery select different models.
if 'CQ_TOKEN_VECTORS' not in env and (ROOT/'.codequest-tools/embeddings/code_vectors.bin').is_file():
 env['CQ_TOKEN_VECTORS']=str(ROOT/'.codequest-tools/embeddings')
def check(ok,why):
 if not ok:raise AssertionError(why)
def encoded(v):return json.dumps(v,ensure_ascii=False,separators=(',',':')).encode()
def seal(s):
 for family in ['entities','relations','occurrences','bodies','documents']:s[family].sort(key=lambda v:v['id'])
 s['revision']='';s['revision']=hashlib.sha256(encoded(s)).hexdigest();return s
def socketpath(store):return '/tmp/codequest-%s-%s.sock'%(os.geteuid(),hashlib.sha256(os.fsencode(store.resolve())).hexdigest()[:24])
def rpc(store,v):
 with socket.socket(socket.AF_UNIX) as s:
  s.settimeout(120);s.connect(socketpath(store));b=encoded(v|{'_client_pid':os.getpid()});s.sendall(struct.pack('>I',len(b))+b)
  def read(n):
   out=b''
   while len(out)<n:
    part=s.recv(n-len(out));check(bool(part),'IPC EOF');out+=part
   return out
  data=json.loads(read(struct.unpack('>I',read(4))[0]));check('error'not in data,str(data));return data['result']
def cli(store,*args,ok=True,environment=None):
 start=time.monotonic();r=subprocess.run([str(a.cq),*map(str,args),'--store',str(store)],env=environment or env,capture_output=True,text=True,timeout=600)
 with (a.output/'events.jsonl').open('a') as f:f.write(json.dumps({'args':list(map(str,args)),'store':str(store),'exit':r.returncode,'seconds':time.monotonic()-start,'stdout':r.stdout,'stderr':r.stderr})+'\n')
 if ok:check(r.returncode==0,r.stderr)
 return json.loads(r.stdout) if r.returncode==0 else r
fixture=a.output/'fixture'
def generator():
 r=subprocess.run(['cargo','test','-p','cq-store','--test','paged_store','--locked','--offline','export_cli_acceptance_fixture','--','--ignored','--exact'],cwd=ROOT,env=env|{'CQ_PAGED_FIXTURE_OUTPUT':str(fixture)},capture_output=True,text=True,timeout=120);(a.output/'fixture.log').write_text(r.stdout+r.stderr);check(r.returncode==0,r.stderr)
 generator.pid=None
seed=None
def newstore(name):
 store=a.output/name/'store';store.mkdir(parents=True);shutil.copytree(fixture/'store/blobs',store/'blobs');return store

def publish(store,s):
 target=store.parent/'candidate.json';target.write_bytes(encoded(seal(s)));result=cli(store,'publish-snapshot','--snapshot',target);hosts.add(rpc(store,{'op':'ping'})['pid']);return result

def sample(store,name,revision=None):
 start=time.perf_counter();view=rpc(store,{'op':'open','project':name,'revision':revision,'session':True})
 try:
  response=rpc(store,{'op':'scan','view':view['view'],'group':'out:f0','limit':7,'timeout_ms':30000});rows=response['value']['rows'];check(rows and len(rows)<=7,'empty/wrong task page');return {'ms':(time.perf_counter()-start)*1000,'stats':response['stats'],'rows':len(rows),'revision':view['revision']}
 finally:rpc(store,{'op':'release','view':view['view']})
def stats(rows):
 v=sorted(x['ms'] for x in rows);return {'samples':rows,'p50_ms':v[(len(v)-1)//2],'p95_ms':v[math.ceil(.95*len(v))-1]}
def case(name,fn):
 start=time.monotonic()
 try:result=fn();cases.append({'case':name,'status':'passed','seconds':time.monotonic()-start,'evidence':result})
 except BaseException as e:cases.append({'case':name,'status':'failed','seconds':time.monotonic()-start,'error':repr(e)});raise
 finally:save()
def save():
 (a.output/'results.json').write_text(json.dumps({'cases':cases,'passed':sum(c['status']=='passed'for c in cases),'host_rss_peak_bytes':peaks,'scope':'actual Ladybug through CLI/IPC; synthetic immutable snapshots, not compiler benchmarks; warm OS cache, includes fixed-view admission and first local page','power_loss_tested':False},indent=2)+'\n')
def pollrss():
 while not stop.wait(.02):
  for pid in list(hosts):
   try:
    kb=int(next(x for x in Path(f'/proc/{pid}/status').read_text().splitlines()if x.startswith('VmRSS:')).split()[1]);peaks[str(pid)]=max(peaks.get(str(pid),0),kb*1024)
   except (OSError,StopIteration):pass

def matrix():
 store=newstore('matrix');matrix=[];updates=[];names=['matrix-a','matrix-b','matrix-c'];states={}
 for name in names:
  s=copy.deepcopy(seed);s['project']=name;s['entities']=[e for e in s['entities']if e['id']in {'f0','f1'}];s['relations']=[r for r in s['relations']if r['from']=='f0'and r['to']=='f1'];s['contexts']=[];s['previous']=None;states[name]=s
 for history in range(1,65):
  for name,s in states.items():
   s['entities'][1]['signature']=f'fn(version={history})';s['created_ms']+=1;start=time.perf_counter();result=publish(store,s);updates.append({'ms':(time.perf_counter()-start)*1000,'result':result});revision=s['revision'];rpc(store,{'op':'pin_cursor','project':name,'cursor':f'{name}-{history}','revision':revision});s['previous']=revision
  if history in [1,8,64]:
   for projects in [1,3]:
    for concurrency in [1,8]:
     with cf.ThreadPoolExecutor(max_workers=concurrency) as ex:rows=list(ex.map(lambda i:sample(store,names[i%projects]),range(32)))
     matrix.append({'projects':projects,'concurrency':concurrency,'retained_revisions':history,**stats(rows)})
 for name in names:check(not rpc(store,{'op':'retention_preview','project':name,'keep':1})['revisions'],'explicit version protections not honored')
 diagnostic=rpc(store,{'op':'diagnostics','project':names[0]});check(diagnostic['residency']['ordinary_databases']<=1,'ordinary budget');check(diagnostic['residency']['maintenance_databases']==0,'idle maintenance retained')
 return {'matrix':matrix,'updates':stats(updates),'diagnostics':diagnostic,'synthetic_entities_per_project':2}

def scale():
 rows=[]
 for factor in [1,10]:
  store=newstore('scale-'+str(factor));s=copy.deepcopy(seed);s['project']='scale';s['previous']=None
  s['entities']=[e for e in s['entities']if e['id']in {'f0','f1'}];s['relations']=[r for r in s['relations']if r['from']=='f0'and r['to']=='f1'];s['contexts']=[]
  template=copy.deepcopy(s['entities'][-1])
  for i in range(128*factor):
   e=copy.deepcopy(template);e['id']=f'independent-{i}';e['provider_id']=e['id'];e['name']=e['id'];e['qualified_name']=e['id'];e['unit']='unrelated';s['entities'].append(e)
  result=publish(store,s);samples=[sample(store,'scale')for _ in range(32)];check(all(x['stats']['full_snapshot_decodes']==0 for x in samples),'full snapshot loaded');rows.append({'factor':factor,'independent_entities':128*factor,'entities':len(s['entities']),**stats(samples),'publication':result})
 check(rows[1]['p95_ms']<=rows[0]['p95_ms']*1.5+100,'10x local latency regressed');return rows

def waitfor(path):
 end=time.monotonic()+90
 while not path.exists():check(time.monotonic()<end,'barrier timeout '+str(path));time.sleep(.01)
def maintenance():
 store=newstore('maintenance');s=copy.deepcopy(seed);s['entities']=[e for e in s['entities']if e['id']in {'f0','f1'}];s['relations']=[r for r in s['relations']if r['from']=='f0'and r['to']=='f1'];s['contexts']=[];s['previous']=None;s['project']='maintenance';publish(store,s);first=s['revision'];held=rpc(store,{'op':'open','project':'maintenance','revision':first,'session':True});project=store/'format-2/projects'/hashlib.sha256(b'maintenance').hexdigest();hooks=project/'test-hooks';hooks.mkdir();(hooks/'rebuild-snapshot').touch()
 with cf.ThreadPoolExecutor() as ex:
  f=ex.submit(cli,store,'compact-store','--project','maintenance');waitfor(hooks/'ready-rebuild-snapshot');s['previous']=first;s['created_ms']+=1;s['entities'][1]['signature']='changed';publish(store,s);second=s['revision'];old=rpc(store,{'op':'get','view':held['view'],'family':'entity','id':'f0'});check(old['value']['value']['id']=='f0','old view damaged');(hooks/'release-rebuild-snapshot').touch();result=f.result(timeout=120)
 check(cli(store,'status','--project','maintenance')['revision']==second,'maintenance moved HEAD');check(sample(store,'maintenance',first)['revision']==first,'old history admission');rpc(store,{'op':'release','view':held['view']});hooks.rename(project/'completed-hooks')
 crashes=[]
 for stage in ['rebuild-before-route','rebuild-after-route']:
  hooks.mkdir(exist_ok=True);(hooks/stage).touch()
  with cf.ThreadPoolExecutor() as ex:
   f=ex.submit(cli,store,'compact-store','--project','maintenance',ok=False);waitfor(hooks/('ready-'+stage));pid=rpc(store,{'op':'ping'})['pid'];os.kill(pid,signal.SIGKILL);f.result(timeout=120)
  hooks.rename(project/('completed-'+stage));check(cli(store,'status','--project','maintenance')['revision']==second,'crash changed HEAD');check(sample(store,'maintenance',first)['revision']==first,'crash lost old history');hosts.add(rpc(store,{'op':'ping'})['pid']);crashes.append(stage)
 # New maintenance must recover/reclaim the failed operation's reservation.
 final=cli(store,'compact-store','--project','maintenance');return {'concurrent_publish':second,'initial_revision':first,'compaction':result,'process_kill_barriers':crashes,'retry':final}
def migration():
 store=a.output/'migration/store'
 r=subprocess.run(['cargo','test','-p','cq-store','--test','paged_store','--locked','--offline','export_legacy_acceptance_fixture','--','--ignored','--exact'],cwd=ROOT,env=env|{'CQ_LEGACY_FIXTURE_OUTPUT':str(store)},capture_output=True,text=True,timeout=120);check(r.returncode==0,r.stderr)
 ids=json.loads((store/'legacy-ids.json').read_text());snap=json.loads((store/'revisions'/ids[-1]/'snapshot.json').read_text());snap['previous']=ids[-1];snap['created_ms']+=1;snap['entities'][0]['signature']='after migration';target=store.parent/'after.json';target.write_bytes(encoded(seal(snap)))
 project=store/'projects'/hashlib.sha256(b'paged-test').hexdigest();hooks=project/'test-hooks';hooks.mkdir();(hooks/'migration-snapshot').touch();(hooks/'legacy-before-lock').touch()
 with cf.ThreadPoolExecutor() as ex:
  migrating=ex.submit(cli,store,'migrate-store','--project','paged-test');waitfor(hooks/'ready-migration-snapshot')
  # A second migration shares the legacy publication lock. Both must finish,
  # with one actual migration and one idempotent no-op.
  second=ex.submit(cli,store,'migrate-store','--project','paged-test')
  def writer():
   r=subprocess.run(['cargo','test','-p','cq-store','--test','paged_store','--locked','--offline','legacy_writer_during_migration','--','--ignored','--exact'],cwd=ROOT,env=env|{'CQ_LEGACY_WRITER_STORE':str(store),'CQ_LEGACY_WRITER_SNAPSHOT':str(target)},capture_output=True,text=True,timeout=120);(store.parent/'writer.log').write_text(r.stdout+r.stderr);check(r.returncode==0,r.stderr)
  writing=ex.submit(writer);waitfor(hooks/'ready-legacy-before-lock');(hooks/'release-legacy-before-lock').touch();(hooks/'release-migration-snapshot').touch();first=migrating.result(timeout=120);other=second.result(timeout=120);writing.result(timeout=120)
 check(first['state']=='migrated'and other['state']=='already_migrated','migration race was not serialized')
 result=cli(store,'status','--project','paged-test');check(result['revision']==snap['revision'],'racing legacy writer lost')
 hosts.add(rpc(store,{'op':'ping'})['pid']);check(sample(store,'paged-test',ids[0])['revision']==ids[0],'migration lost old history')
 notes=cli(store,'annotations','--project','paged-test');check('fixture responsibility'in json.dumps(notes),'migration lost feedback')
 return {'first':first,'second':other,'new_head':result['revision'],'old_versions_preserved':ids}

try:
 generator();seed=json.loads((fixture/'R1.json').read_text());thread=threading.Thread(target=pollrss,daemon=True);thread.start()
 for name,fn in [('maintenance',maintenance),('migration',migration),('matrix',matrix),('scale',scale)]:
  if not a.only or name in a.only:case(name,fn)
finally:
 stop.set()
 for pid in hosts:
  try:os.kill(pid,signal.SIGTERM)
  except ProcessLookupError:pass
 save()
