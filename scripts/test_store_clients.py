#!/usr/bin/env python3
"""Persistent REPL/IPC acceptance with exact responses and process barriers."""
import argparse,concurrent.futures,hashlib,json,math,os,queue,signal,socket,struct,subprocess,threading,time
from pathlib import Path
ROOT=Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser();p.add_argument('--output',type=Path,required=True);p.add_argument('--cq',type=Path,default=ROOT/'target/debug/cq');p.add_argument('--only',action='append',default=[]);a=p.parse_args();a.cq=a.cq.resolve();a.output.mkdir(parents=True,exist_ok=True)
ENV=os.environ|{'CQ_STORE_HOST':str(a.cq.with_name('cq-store-host'))}
LOG=a.output/'events.jsonl';cases=[];peers=[];initial_binary_sha=hashlib.sha256(a.cq.read_bytes()).hexdigest();store=a.output/'fixture/store'
def log(v):
 with LOG.open('a')as f:f.write(json.dumps(v,ensure_ascii=False)+'\n')
def check(ok,message):
 if not ok:raise AssertionError(message)
def cli(*args,ok=True):
 start=time.monotonic();r=subprocess.run([str(a.cq),*map(str,args),'--store',str(store)],capture_output=True,text=True,env=ENV,timeout=180);log({'cli':list(map(str,args)),'exit':r.returncode,'stdout':r.stdout,'stderr':r.stderr,'seconds':time.monotonic()-start})
 if ok:check(r.returncode==0,r.stderr)
 return json.loads(r.stdout)if r.returncode==0 else r

def rpc(v):
 path=Path('/tmp')/f'codequest-{os.geteuid()}-{hashlib.sha256(os.fsencode(store.resolve())).hexdigest()[:24]}.sock'
 with socket.socket(socket.AF_UNIX)as s:
  s.settimeout(180);s.connect(str(path));b=json.dumps(v,separators=(',',':')).encode();s.sendall(struct.pack('>I',len(b))+b)
  def read(n):
   out=b''
   while len(out)<n:
    part=s.recv(n-len(out))
    if not part:raise EOFError('host disconnected')
    out+=part
   return out
  result=json.loads(read(struct.unpack('>I',read(4))[0]));check('error'not in result,str(result));return result['result']
def waitfile(path,timeout=90):
 end=time.monotonic()+timeout
 while not path.exists():
  check(time.monotonic()<end,'barrier timed out: '+str(path));time.sleep(.01)

def publish(name):return cli('publish-snapshot','--snapshot',a.output/f'fixture/{name}.json')
class Peer:
 def __init__(self,project='paged-test'):
  self.p=subprocess.Popen([str(a.cq),'repl','--store',str(store),'--project',project],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1,env=ENV);self.q=queue.Queue();self.project=project;peers.append(self)
  def reader():
   for line in self.p.stdout:
    try:self.q.put(json.loads(line))
    except ValueError:self.q.put({'protocol_error':line})
  threading.Thread(target=reader,daemon=True).start()
 def send(self,line):self.p.stdin.write(line+'\n');self.p.stdin.flush();log({'repl_in':line,'pid':self.p.pid})
 def receive(self,timeout=120):
  result=self.q.get(timeout=timeout);log({'repl_out':result,'pid':self.p.pid});check('protocol_error'not in result,str(result));return result
 def cmd(self,line,error=None):
  self.send(line);r=self.receive();check(r.get('error',{}).get('code')==error,str(r));
  if r.get('execution_path')=='fixed_view_fact_reader':check(r['read_stats']['full_snapshot_decodes']==0,'unexpected full snapshot decode')
  return r
 def close(self):
  if self.p.stderr.closed:return
  if self.p.poll()is None:self.send('quit');self.p.stdin.close();self.p.wait(timeout=30)
  check(self.p.returncode==0,self.p.stderr.read());self.p.stdout.close();self.p.stderr.close()
def case(name,fn):
 if a.only and not any(name.startswith(prefix)for prefix in a.only):return
 start=time.monotonic()
 try:
  evidence=fn();cases.append({'case':name,'status':'passed','seconds':time.monotonic()-start,'evidence':evidence})
 except BaseException as e:
  cases.append({'case':name,'status':'failed','seconds':time.monotonic()-start,'error':repr(e)});raise

def main():
 fixture=a.output/'fixture';r=subprocess.run(['cargo','test','-p','cq-store','--test','paged_store','--locked','--offline','export_cli_acceptance_fixture','--','--ignored','--exact'],cwd=ROOT,env=ENV|{'CQ_PAGED_FIXTURE_OUTPUT':str(fixture.resolve())},capture_output=True,text=True,timeout=120);(a.output/'fixture-build.log').write_text(r.stdout+r.stderr);check(r.returncode==0,r.stderr)
 one=publish('R1');publish('Other');r1=one['revision'];r2=json.loads((fixture/'R2.json').read_text())['revision'];q=Peer();other=Peer();
 first=q.cmd('overview');check(first['revision']==r1,str(first));q.cmd('callees {"subject":"f0","limit":7}')
 # Start a fresh first page before publication; retain its exact opaque cursor.
 page=q.cmd('callees {"subject":"f0","limit":7}');original_cursor=page['next_cursor'];seen={row['entity']['id']for row in page['items']};publish('R2')
 def fixed():
  value=q.cmd('overview');check(value['revision']==r1 and value['notice']['new_head']==r2,str(value));return {'pinned':r1,'new_head':r2}
 case('Q01_fixed_view_after_external_publication',fixed)
 def pagination():
  # overview replaces the REPL's last query; replay the saved original cursor.
  value=q.cmd('callees '+json.dumps({'subject':'f0','limit':7,'cursor':original_cursor}));count=1
  while True:
   for row in value['items']:check(row['entity']['id']not in seen,'duplicate neighbor');seen.add(row['entity']['id'])
   check(value['revision']==r1,'mixed revision');count+=1
   if not value['next_cursor']:break
   value=q.cmd('next')
  check(seen=={f'f{i}'for i in range(1,1601)},'lost/foreign neighbors');q.cmd('next',error='NoContinuation');return {'neighbors':len(seen),'pages':count,'revision':r1}
 case('Q03_pagination_keeps_old_view',pagination)
 def local():
  found=q.cmd('search {"text":"^f0$","regex":true}');handle=next(iter(found['handles']));value=q.cmd('expand '+handle);check(value['items'][0]['entity']['id']=='f0',str(value));q.cmd('source '+handle);q.cmd('collapse '+handle);check(q.cmd('expand '+handle)['items'][0]['entity']['id']=='f0','lost facts');return {'handle':handle,'revision':r1}
 case('Q02_local_disclosure_and_frozen_source',local)
 def performance():
  samples=[];stats=[]
  for _ in range(30):
   start=time.monotonic();response=q.cmd('callees {"subject":"f0","limit":7}');samples.append((time.monotonic()-start)*1000);stats.append(response['read_stats']);check(len(response['items'])==7,'wrong page size')
  diagnostic=rpc({'op':'diagnostics','project':'paged-test','revision':r1});profile=diagnostic['profile'];check('PRIMARY_KEY_SCAN_NODE_TABLE' in profile,'missing primary-key plan evidence');(a.output/'page-profile.txt').write_text(profile)
  values=sorted(samples);return {'samples_ms':samples,'p50_ms':values[14],'p95_ms':values[math.ceil(.95*len(values))-1],'read_stats_samples':stats,'profile':'page-profile.txt','scope':'warm persistent REPL, resolved fixed revision, seven high-fanout neighbors; includes IPC and serialization'}
 case('P01_end_to_end_page_samples',performance)

 def failed():
  before=q.cmd('view')['view'];q.cmd('use missing',error='MissingProject');q.cmd('context nonexistent',error='MissingContext');q.cmd('revision '+'0'*64,error='RevisionUnavailable');check(q.cmd('view')['view']==before,'failed switch mutated state');return before
 case('Q05_failed_switch_preserves_session',failed)
 def switched():
  value=q.cmd('revision head');check(value['view']['revision']==r2 and value['view']['handles']=={},str(value));q.cmd('details @1',error='UnknownHandle');q.cmd('next',error='NoContinuation');q.cmd('context ctx-a');q.cmd('context all');q.cmd('revision '+r1);return q.cmd('view')['view']
 case('Q04_atomic_view_switch',switched)
 def cursor_constraints():
  q.cmd('callees '+json.dumps({'subject':'f0','cursor':original_cursor,'context':'ctx-b'}),error='StaleCursor');q.cmd('callees '+json.dumps({'subject':'f0','cursor':original_cursor,'limit':2}));
  bad=cli('query','--project','paged-other','--json',json.dumps({'operation':'next','cursor':original_cursor}),ok=False);check(bad.returncode!=0 and 'WrongProject'in bad.stderr,bad.stderr);return {'query_context_mismatch_rejected':True,'project_mismatch_rejected':True,'page_size_change_allowed':True}
 case('Q06_cursor_binding',cursor_constraints)
 def restart():
  q.cmd('callees '+json.dumps({'subject':'f0','cursor':original_cursor,'limit':2}))
  pid=rpc({'op':'ping'})['pid'];os.kill(pid,signal.SIGKILL);page=q.cmd('next');check(page['revision']==r1,str(page));new=rpc({'op':'ping'})['pid'];check(new!=pid,'host did not restart');return {'old_pid':pid,'new_pid':new,'revision':page['revision']}
 case('Q07_host_restart_preserves_cursor',restart)
 def cancellation():
  previous=q.cmd('view')['request'];q.send('trace {"subject":"f0","depth":4096,"limit":1000,"max_work":1000000,"max_bytes":1048576}');q.send('cancel '+previous);bad=q.receive();check(bad['error']['code']=='StaleRequest',str(bad));q.send('cancel');ack=q.receive();check(ack['state']=='cancellation_requested',str(ack));done=q.receive();check(done['error']['code']=='Cancelled',str(done));check(done['request']==ack['request'],'wrong request cancelled');check(other.cmd('overview')['revision']==r1,'other client affected');q.cmd('overview');return {'request':done['request'],'other_session_usable':True}
 case('Q08_targeted_cancel_and_connection_reuse',cancellation)
 def exit_peer():
  pid=rpc({'op':'ping'})['pid'];other.close();q.cmd('overview');check(rpc({'op':'ping'})['pid']==pid,'client exit killed host');return {'host_pid':pid}
 case('Q10_one_client_exit_preserves_others',exit_peer)
 def errors():
  q.cmd('details @999',error='UnknownHandle');q.cmd('search {bad',error='Json');q.cmd('search {"limit":0}',error='InvalidBudget');q.cmd('overview');return {'normal_command_after_each_error':True}
 case('Q11_errors_do_not_poison_session',errors)

 def refresh_jobs():
  source=a.output/'go-job-source';source.mkdir();(source/'go.mod').write_text('module example.test/storejob\ngo 1.25\n');file=source/'main.go';file.write_text('package storejob\nfunc Run() int { return 1 }\n')
  initial=cli('index','--root',source,'--language','go','--name','job-test');peer=Peer('job-test');old=initial['revision'];peer.cmd('overview')
  hooks=store/'format-2/test-hooks';hooks.mkdir(parents=True,exist_ok=True);(hooks/'before-publication').touch();file.write_text('package storejob\nfunc Run() int { return 2 }\n')
  job=peer.cmd('refresh')['job'];waitfile(hooks/'ready-before-publication');peer.cmd('cancel '+job)
  deadline=time.monotonic()+30
  while True:
   status=peer.cmd('job '+job)['status']
   if status['state'] not in ['running','watching']:break
   check(time.monotonic()<deadline,'cancelled build did not settle');time.sleep(.02)
  check(status['state']=='cancelled',str(status));check(cli('status','--project','job-test')['revision']==old,'cancel before publication advanced HEAD');(hooks/'before-publication').unlink();(hooks/'ready-before-publication').unlink()
  project=store/'format-2/projects'/hashlib.sha256(b'job-test').hexdigest();barrier=project/'test-hooks';barrier.mkdir(exist_ok=True);(barrier/'after-head').touch();file.write_text('package storejob\nfunc Run() int { return 3 }\n')
  job=peer.cmd('refresh')['job'];waitfile(barrier/'ready-after-head');peer.cmd('cancel '+job);pid=rpc({'op':'ping'})['pid'];os.kill(pid,signal.SIGKILL)
  deadline=time.monotonic()+60
  while True:
   status=peer.cmd('job '+job)['status']
   if status['state'] not in ['running','watching']:break
   check(time.monotonic()<deadline,'committed build did not settle');time.sleep(.02)
  check(status['state']=='complete',str(status));new=cli('status','--project','job-test')['revision'];check(new!=old,'committed build missing');check(peer.cmd('overview')['revision']==old,'refresh switched session');(barrier/'after-head').unlink()
  noop=cli('refresh','--project','job-test');check(noop['revision']==new and noop['update']['mode']=='checkpoint_unchanged',str(noop));check(noop['update']['fact_pages_written']==0 and noop['update']['projection_rows_submitted']==0,'noop wrote facts')
  peer.close();return {'before_commit':'cancelled; HEAD unchanged','after_commit':'complete despite late cancel and lost response','old_revision':old,'new_revision':new,'noop':noop['update']}
 case('Q09_real_build_refresh_and_commit_cancel',refresh_jobs)
 q.close()
 # Real process-death commit windows on a separate fresh fixture store.
 project_dir=store/'format-2/projects'/hashlib.sha256(b'paged-test').hexdigest()
 def crash(stage,target,expected_committed):
  hooks=project_dir/'test-hooks';hooks.mkdir(exist_ok=True);(hooks/stage).touch();ready=hooks/f'ready-{stage}';ready.unlink(missing_ok=True)
  with concurrent.futures.ThreadPoolExecutor()as executor:
   future=executor.submit(publish,target);waitfile(ready);pid=rpc({'op':'ping'})['pid'];os.kill(pid,signal.SIGKILL)
   result=future.result(timeout=120)
  (hooks/stage).unlink();head=json.loads((project_dir/'HEAD').read_text());target_id=json.loads((fixture/f'{target}.json').read_text())['revision'];check((head==target_id)==expected_committed,'wrong commit decision');return {'stage':stage,'head':head,'target':target_id,'cli_result':str(result)}
 # A rejected publication returns a nonzero CLI; invoke separately to keep its expected failure explicit.
 def before_head():
  hooks=project_dir/'test-hooks';hooks.mkdir(exist_ok=True);(hooks/'after-index').touch();ready=hooks/'ready-after-index';ready.unlink(missing_ok=True)
  with concurrent.futures.ThreadPoolExecutor()as executor:
   future=executor.submit(cli,'publish-snapshot','--snapshot',fixture/'R3.json',ok=False);waitfile(ready);pid=rpc({'op':'ping'})['pid'];os.kill(pid,signal.SIGKILL);result=future.result(timeout=120)
  (hooks/'after-index').unlink();check(isinstance(result,subprocess.CompletedProcess)and result.returncode!=0,'uncommitted candidate reported success');check(json.loads((project_dir/'HEAD').read_text())==r2,'HEAD advanced before commit');return {'old_head':r2,'result':result.stderr}
 case('C05_process_death_after_index_before_HEAD',before_head)
 case('C05_process_death_after_HEAD',lambda:crash('after-head','R3',True))
 # A fixed view must keep HEAD unchanged during an epoch rebuild/catch-up.
 def rebuild():
  publish('R3')
  hooks=project_dir/'test-hooks';(hooks/'rebuild-snapshot').touch();ready=hooks/'ready-rebuild-snapshot';ready.unlink(missing_ok=True)
  with concurrent.futures.ThreadPoolExecutor()as executor:
   future=executor.submit(cli,'rebuild-store','--project','paged-test');waitfile(ready);before=rpc({'op':'diagnostics','project':'paged-test'})['physical_counts'];new=publish('R4')['revision'];(hooks/'release-rebuild-snapshot').touch();result=future.result(timeout=120)
  (hooks/'rebuild-snapshot').unlink();check(json.loads((project_dir/'HEAD').read_text())==new,'maintenance lost concurrent HEAD');after=rpc({'op':'diagnostics','project':'paged-test'})['physical_counts'];check(after['pages']==before['pages'] and after['facts']==before['facts'] and after['links']==before['links'],'unchanged semantic data re-imported');check(cli('query','--project','paged-test','--revision',r1,'--json',json.dumps({'operation':'definition','subject':'f0'}))['revision']==r1,'history missing after catch-up');return {'epoch':result['epoch'],'head':new,'physical_before':before,'physical_after':after}
 case('C07_rebuild_keeps_analysis_HEAD',rebuild)
 # Cleanup is restricted to the daemon belonging to this fresh test store.
 pid=rpc({'op':'ping'})['pid'];os.kill(pid,signal.SIGTERM)

try:main()
finally:
 for peer in peers:
  try:peer.close()
  except Exception as e:log({'cleanup_error':repr(e)})
 result={'passed':sum(c['status']=='passed'for c in cases),'total':len(cases),'cases':cases,'binary_sha256':initial_binary_sha,'binary_unchanged':initial_binary_sha==hashlib.sha256(a.cq.read_bytes()).hexdigest(),'scope':'Controlled protocol fixtures for Q/C contracts; native language tests are recorded separately.'}
 (a.output/'results.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
 print(json.dumps({'passed':result['passed'],'total':result['total']}))
