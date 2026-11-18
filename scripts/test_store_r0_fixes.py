#!/usr/bin/env python3
"""Run the R0 review regressions, including real fsync faults and view privacy checks."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    out=args.output.resolve()
    if out.exists():parser.error('fresh output directory required')
    out.mkdir(parents=True)
    repo=Path(__file__).resolve().parents[1]
    records=[]

    def run(name,command,environment=None,expected=0):
        start=time.monotonic()
        with (out/(name+'.stdout')).open('w') as stdout,(out/(name+'.stderr')).open('w') as stderr:
            result=subprocess.run(command,cwd=repo,env=environment,stdout=stdout,stderr=stderr)
        record={'name':name,'command':command,'exit_code':result.returncode,'expected_exit_code':expected,'seconds':time.monotonic()-start}
        records.append(record)
        (out/'steps.json').write_text(json.dumps(records,indent=2))
        print(name,result.returncode,flush=True)
        if result.returncode!=expected:
            print((out/(name+'.stderr')).read_text()[-6000:]);raise RuntimeError(name+' failed')
        return record

    run('core',['python3','scripts/run_store_r0.py','--output',str(out/'core')])
    core=json.loads((out/'core/results.json').read_text())
    if core['passed']!=core['total'] or core['total']<25:raise RuntimeError('incomplete core suite')
    shim=out/'fsync_fault.so'
    run('compile-fault-shim',['cc','-shared','-fPIC','-O2','crates/cq-store/examples/store_r0/fsync_fault.c','-ldl','-o',str(shim)])
    faults=[]
    for mode,suffix in [('directory-once','directory_once'),('directory-repeat','directory_repeat'),('head-file','head_file')]:
        case='commit_fault_'+suffix
        directory=out/suffix
        env=os.environ.copy()
        env['LD_PRELOAD']=str(shim)
        env['CQ_R0_REVIEW_FSYNC_ROOT']=str(directory/case)
        env['CQ_R0_FAULT_MODE']=mode
        run(suffix,['python3','scripts/run_store_r0.py','--output',str(directory),'--case',case],env)
        result=json.loads((directory/'results.json').read_text())
        stderr=(directory.with_name(directory.name+'-logs')/'stderr.log').read_text()
        if result['passed']!=1 or 'R0_FAULT injected fsync EIO' not in stderr:raise RuntimeError('fault not exercised: '+mode)
        faults.append(result['cases'][0])

    # A compile-fail contract checks both the private field and read-only getter.
    crate=out/'privacy-check';(crate/'src').mkdir(parents=True)
    manifest='[package]\nname="store-r0-view-privacy-check"\nversion="0.1.0"\nedition="2024"\n[workspace]\n[dependencies]\n'
    manifest+='cq-core={path='+json.dumps(str(repo/'crates/cq-core'))+'}\n'
    manifest+='cq-capture-protocol={path='+json.dumps(str(repo/'crates/cq-capture-protocol'))+'}\n'
    manifest+='lbug="=0.22.3"\nserde={version="1",features=["derive"]}\nserde_json="1"\ncsv="1"\ntempfile="3"\nlibc="0.2"\n'
    (crate/'Cargo.toml').write_text(manifest)
    shutil.copyfile(repo/'Cargo.lock',crate/'Cargo.lock')
    modules='#![allow(dead_code)]\n'+''.join('#[path='+json.dumps(str(repo/'crates/cq-store/examples/store_r0'/f'{name}.rs'))+']\nmod '+name+';\n' for name in ['model','projection','host'])
    positive=modules+'fn inspect(read:&host::Read){let _=read.view().clone();}\nfn main(){}\n'
    target=['cargo','check','--manifest-path',str(crate/'Cargo.toml'),'--target-dir',str(repo/'target'),'--offline']
    (crate/'src/main.rs').write_text(positive)
    run('view-positive',target)
    for name,body,code in [('private-field','read.view.sequence+=1;','E0616'),('readonly-getter','read.view().sequence+=1;','E0594')]:
        source=modules+'fn forbidden(read:&mut host::Read){'+body+'}\nfn main(){}\n'
        (crate/(name+'.rs')).write_text(source)
        (crate/'src/main.rs').write_text(source)
        run(name,target+['--locked'],expected=101)
        if code not in (out/(name+'.stderr')).read_text():raise RuntimeError('unexpected compile failure: '+name)
    (crate/'src/main.rs').write_text(positive)

    run('format',['rustfmt','--check','--edition','2024','crates/cq-store/examples/store_r0/main.rs'])
    run('clippy',['cargo','clippy','-p','cq-store','--example','store_r0','--locked','--offline','--','-D','warnings'])
    run('store-tests',['cargo','test','-p','cq-store','--locked','--offline'])
    run('boundaries',['python3','scripts/check_boundaries.py'])
    run('python-syntax',['python3','-m','py_compile','scripts/run_store_r0.py','scripts/test_store_r0_fixes.py'])
    result={'core':{'passed':core['passed'],'total':core['total']},'fault_cases':faults,'privacy_checks':{'positive_compiles':True,'private_field_rejected':'E0616','readonly_getter_rejected':'E0594'},'steps':records,'gate':'eligible_for_isolated_R1_shadow_format_only','production_store_cutover':False,'shim_sha256':hashlib.sha256(shim.read_bytes()).hexdigest()}
    (out/'verification.json').write_text(json.dumps(result,ensure_ascii=False,indent=2))
    print('R0 FIX VERIFICATION COMPLETE',flush=True)
    return 0


if __name__=='__main__':
    sys.exit(main())
