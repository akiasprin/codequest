#!/usr/bin/env python3
"""Measure workspace coverage including real-compiler black-box queries.

--branches uses rustc's experimental coverage-options on the selected toolchain;
RUSTC_BOOTSTRAP is scoped to these subprocesses. It does not change toolchains or
persistent configuration. External rust-analyzer and Go workers are outside LLVM
workspace coverage. Repeated protocol polls are not semantic test counts.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess

ROOT=Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser(description=__doc__)
p.add_argument("--output",type=Path,required=True)
p.add_argument("--branches",action="store_true")
p.add_argument("--real-corpus",action="store_true")
a=p.parse_args();a.output=a.output.resolve();a.output.mkdir(parents=True,exist_ok=True)
environment=os.environ.copy()
if a.branches: environment["RUSTC_BOOTSTRAP"]="1"
records=[]
def run(words,env=environment):
    result=subprocess.run(words,cwd=ROOT,env=env,capture_output=True,text=True,timeout=1800)
    records.append({"command":list(map(str,words)),"exit":result.returncode,"stdout":result.stdout,"stderr":result.stderr})
    (a.output/"commands.json").write_text(json.dumps(records,indent=2))
    if result.returncode: raise RuntimeError(f"coverage command failed: {words}; see commands.json")
    print("completed:"," ".join(map(str,words)),flush=True)
    return result.stdout
branch=["--branch"] if a.branches else []
run(["cargo","llvm-cov","clean","--workspace","--profraw-only"])
run(["cargo","llvm-cov","--workspace","--offline","--no-report",*branch])
profile_env=environment.copy();profile_env["CARGO_TARGET_DIR"]=str(ROOT/"target/llvm-cov-target")
result=subprocess.run(["cargo","llvm-cov","show-env","--sh",*branch],cwd=ROOT,env=profile_env,capture_output=True,text=True,check=True)
for line in result.stdout.splitlines():
    words=shlex.split(line)
    if len(words)==2 and words[0]=="export":
        key,value=words[1].split("=",1);profile_env[key]=value
run(["cargo","build","--workspace","--offline"],profile_env)
# Explicit provider paths avoid assuming the instrumented target directory has
# the ordinary target/{debug,release} installation layout.
profile_env.setdefault("CQ_RUST_HIR",str(ROOT/".codequest-tools/rust-hir/bin/cq-rust-hir"))
profile_env.setdefault("CQ_GO_TYPES",str(ROOT/".codequest-tools/go-types/bin/cq-go-types"))
profile_env.setdefault("CQ_CLANG",str(ROOT/"target/llvm-cov-target/debug/cq-clang"))
cq=ROOT/"target/llvm-cov-target/debug/cq"
for script,name in [("test_foundations.py","foundations"),("test_language_structures.py","structures"),("acceptance_native.py","acceptance")]:
    run(["python3",str(ROOT/"scripts"/script),"--cq",str(cq),"--output",str(a.output/name)],profile_env)
if a.real_corpus:
    run(["python3",str(ROOT/"scripts/test_real_corpus.py"),"--cq",str(cq),"--output",str(a.output/"real")],profile_env)
run(["cargo","llvm-cov","report","--html","--output-dir",str(a.output),*branch])
run(["cargo","llvm-cov","report","--json","--summary-only","--output-path",str(a.output/"summary.json"),*branch])
(a.output/"scope.json").write_text(json.dumps({"branches":a.branches,"experimental_rustc_coverage":a.branches,
    "includes":["workspace Rust tests","instrumented cq CLI/MCP/recorder/clang with real compiler fixtures"],
    "real_corpus":a.real_corpus,"excludes":["external rust-analyzer producer internals","Go worker","LLVM/libclang internals"],
    "notice":"Coverage describes executed code, not semantic completeness or cross-project parity."},indent=2))
