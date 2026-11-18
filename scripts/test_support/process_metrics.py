"""Linux process metrics for task-owned compiler/store acceptance workloads."""
import hashlib,json,os,socket,struct,subprocess,time
from pathlib import Path

def measured_run(command,*,store=None,timeout=2400,env=None):
    proc=subprocess.Popen(command,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,env=env)
    known={proc.pid};roles={};samples=0;tree_peak=0;host_baseline={};started=time.monotonic();phases=[]
    project=command[command.index('--name')+1] if '--name' in command else None
    progress_path=Path(store)/'progress'/(hashlib.sha256(str(project).encode()).hexdigest()+'.json') if store and project else None
    def discover_host():
        if store is None:return
        runtime_env=env or os.environ
        scratch=runtime_env.get('CQ_TEMP_DIR',runtime_env.get('TMPDIR','/tmp'))
        address=str(Path(scratch)/('codequest-%s-%s.sock'%(os.geteuid(),hashlib.sha256(os.fsencode(Path(store).resolve())).hexdigest()[:24])))
        try:
            with socket.socket(socket.AF_UNIX)as s:
                s.settimeout(.05);s.connect(address);data=b'{"op":"ping"}';s.sendall(struct.pack('>I',len(data))+data)
                def read(n):
                    b=b''
                    while len(b)<n:
                        v=s.recv(n-len(b))
                        if not v:raise OSError('EOF')
                        b+=v
                    return b
                pid=json.loads(read(struct.unpack('>I',read(4))[0]))['result']['pid'];known.add(pid)
        except (OSError,KeyError,ValueError):pass
    discover_host()
    def sample():
        nonlocal tree_peak,samples
        if progress_path:
            try:
                v=json.loads(progress_path.read_text());key=(v.get('phase'),v.get('phase_started_ms'))
                if not phases or (phases[-1].get('phase'),phases[-1].get('phase_started_ms'))!=key:
                    phases.append({**v,'first_observed_seconds':time.monotonic()-started})
                else:phases[-1].update({k:v[k] for k in ['completed','total','state','updated_ms'] if k in v})
            except(OSError,ValueError):pass
        pending=list(known);total=0;seen=set()
        while pending:
            pid=pending.pop()
            if pid in seen:continue
            seen.add(pid);base=Path('/proc')/str(pid)
            try:
                args=(base/'cmdline').read_bytes().split(b'\0');name=Path(os.fsdecode(args[0])).name
                if not name:continue
                status={k:v.strip()for k,v in(line.split(':',1)for line in(base/'status').read_text().splitlines()if ':'in line)}
                rss=int(status.get('VmRSS','0 kB').split()[0])*1024;high=int(status.get('VmHWM','0 kB').split()[0])*1024
                role='store_host'if name=='cq-store-host'else'name:'+name
                item=roles.setdefault(str(pid),{'role':role,'max_rss_bytes':0,'lifetime_hwm_bytes':0,'io_first':{},'io_last':{}});item['max_rss_bytes']=max(item['max_rss_bytes'],rss);item['lifetime_hwm_bytes']=max(item['lifetime_hwm_bytes'],high);total+=rss
                try:
                    io={k:int(v.strip())for k,v in(line.split(':',1)for line in(base/'io').read_text().splitlines())}
                    if not item['io_first']:item['io_first']=io
                    item['io_last']=io
                except OSError:pass
                for task in(base/'task').iterdir():
                    try:
                        for child in(task/'children').read_text().split():known.add(int(child));pending.append(int(child))
                    except OSError:pass
            except(OSError,ValueError,IndexError):pass
        tree_peak=max(tree_peak,total);samples+=1
    while True:
        sample()
        if samples%4==0:discover_host()
        left=timeout-(time.monotonic()-started)
        if left<=0:proc.kill();out,err=proc.communicate();raise subprocess.TimeoutExpired(command,timeout,output=out,stderr=err)
        try:out,err=proc.communicate(timeout=min(.05,left));break
        except subprocess.TimeoutExpired:pass
    sample()
    metrics={'seconds':time.monotonic()-started,'sample_interval_seconds':.05,'sampled_tree_peak_rss_bytes':tree_peak,'processes':roles,'samples':samples,'phases':phases,'scope':'sampled task process tree plus its store daemon; native allocator included in RSS, exited short processes may escape sampling; lifetime HWM is separate; progress phases shorter than sampling interval may be missed'}
    return subprocess.CompletedProcess(command,proc.returncode,out,err),metrics
