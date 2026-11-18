#!/usr/bin/env python3
"""Repeated normalization replays of fixed real compiler exports; fresh stores."""
import argparse, hashlib, json, os, statistics, subprocess, time
from pathlib import Path

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--cases',type=Path,required=True)
p.add_argument('--binary',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
p.add_argument('--repeats',type=int,default=3)
p.add_argument('--workers',type=int,nargs='+',default=[1,8,48])
a=p.parse_args()
assert a.repeats>0 and all(1<=n<=48 for n in a.workers)
a.output.mkdir(parents=True,exist_ok=True)
cases=json.loads(a.cases.read_text());samples=[]
env=os.environ|{'TMPDIR':str(a.cases.parent/'.tmp'),'CQ_PROGRESS':'off'}
Path(env['TMPDIR']).mkdir(exist_ok=True)
for case in cases:
    for repeat in range(a.repeats):
        order=a.workers[repeat%len(a.workers):]+a.workers[:repeat%len(a.workers)]
        for workers in order:
            directory=a.output/f"{case['name']}-w{workers}-r{repeat+1}"
            directory.mkdir()
            command=[str(a.binary.resolve()),case['case'],str(directory/'store')]
            resources=directory/'resources.json'
            wrapped=['/usr/bin/time','-f','{"wall_seconds":%e,"user_seconds":%U,"system_seconds":%S,"peak_rss_kib":%M}','-o',str(resources),*command]
            start=time.monotonic();peak=0;peak_phase=0
            with (directory/'stdout').open('w') as out,(directory/'stderr').open('w') as err:
                proc=subprocess.Popen(wrapped,stdout=out,stderr=err,env=env|{'CQ_NORMALIZE_WORKERS':str(workers)})
                progress=directory/'store/progress'/(hashlib.sha256(case['name'].encode()).hexdigest()+'.json')
                while proc.poll() is None:
                    try:
                        child=int(Path(f'/proc/{proc.pid}/task/{proc.pid}/children').read_text().split()[0])
                        status=dict(s.split(':',1) for s in Path(f'/proc/{child}/status').read_text().splitlines() if ':' in s)
                        rss=int(status.get('VmRSS','0 kB').split()[0]);peak=max(peak,rss)
                        v=json.loads(progress.read_text())
                        if v['completed']<v['total']:peak_phase=max(peak_phase,rss)
                    except (OSError,ValueError,KeyError,IndexError):pass
                    time.sleep(.05)
            sample={'project':case['name'],'workers':workers,'repeat':repeat+1,'command':command,'exit':proc.returncode,'wall_seconds':time.monotonic()-start,'resources':json.loads(resources.read_text()),'sampled_process_peak_kib':peak,'sampled_normalize_peak_kib':peak_phase}
            if proc.returncode==0:
                sample['result']=json.loads((directory/'stdout').read_text())
                print(f"{case['name']} workers={workers} repeat={repeat+1}: normalize={sample['result']['normalize_seconds']:.3f}s CPU={sample['result']['average_cpu_cores']:.2f} cores",flush=True)
            else:
                sample['error']=(directory/'stderr').read_text()[-4000:]
                print(f"FAILED {case['name']} workers={workers} repeat={repeat+1}: {sample['error']}",flush=True)
            samples.append(sample)
            (a.output/'samples.json').write_text(json.dumps(samples,ensure_ascii=False,indent=2)+'\n')
summaries=[]
for case in cases:
    group=[s for s in samples if s['project']==case['name']]
    valid=[s for s in group if s['exit']==0]
    fingerprints={(s['result']['record_fingerprint'],s['result']['source_fingerprint']) for s in valid}
    rows=[]
    for workers in a.workers:
        runs=[s for s in valid if s['workers']==workers]
        if not runs:continue
        rows.append({'workers':workers,'runs':len(runs),'median_seconds':statistics.median(s['result']['normalize_seconds'] for s in runs),'min_seconds':min(s['result']['normalize_seconds'] for s in runs),'max_seconds':max(s['result']['normalize_seconds'] for s in runs),'median_cpu_cores':statistics.median(s['result']['average_cpu_cores'] for s in runs),'max_sampled_normalize_rss_kib':max(s['sampled_normalize_peak_kib'] for s in runs),'max_process_rss_kib':max(s['resources']['peak_rss_kib'] for s in runs)})
    base=next((r['median_seconds'] for r in rows if r['workers']==1),None)
    for r in rows:r['speedup_vs_one_worker']=base/r['median_seconds'] if base else None
    summaries.append({'project':case['name'],'scope':case['benchmark_scope'],'success':len(valid)==len(group),'fingerprints_match':len(fingerprints)==1,'frames':valid[0]['result']['file_frames'] if valid else None,'unique_paths':valid[0]['result']['unique_paths'] if valid else None,'records':valid[0]['result']['record_counts'] if valid else None,'rows':rows})
(a.output/'summary.json').write_text(json.dumps({'summaries':summaries,'samples':len(samples),'failures':sum(s['exit']!=0 for s in samples),'scope':'retained real compiler-export normalization only; warm/mixed OS caches; fresh content/journal each run; mapping reconstructed from exported unit tags; CPU includes all threads of benchmark process, not background workloads; sampled normalization RSS excludes completed verification'},ensure_ascii=False,indent=2)+'\n')
print(json.dumps(summaries,ensure_ascii=False,indent=2),flush=True)
