#!/usr/bin/env python3
"""Full native gate: workspace checks, real language fixtures and locked corpora."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

ROOT=Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser(description=__doc__)
p.add_argument("--output",type=Path,required=True)
p.add_argument("--fetch",action="store_true",help="permit fetching locked corpus and build dependencies")
a=p.parse_args();a.output=a.output.resolve();a.output.mkdir(parents=True,exist_ok=True)
tracked=subprocess.check_output(["git","ls-files","--cached","--others","--exclude-standard","-z"],cwd=ROOT).split(b"\0")
fingerprint=hashlib.sha256()
for name in sorted(set(tracked)):
    if not name: continue
    file=ROOT/name.decode()
    if file.is_file(): fingerprint.update(name+b"\0"+hashlib.sha256(file.read_bytes()).digest())
meta={"source_commit":subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip(),
    "source_tree_fingerprint":fingerprint.hexdigest(),"dirty":bool(subprocess.check_output(["git","status","--porcelain"],cwd=ROOT)),
    "fetch_permitted":a.fetch,"commands":[]}
(a.output/"gate.json").write_text(json.dumps(meta,indent=2))
commands=[["bash","scripts/check.sh"],["cargo","build","--workspace","--release","--offline"]]
cq=str(ROOT/"target/release/cq")
for script,directory in [("test_foundations.py","foundations"),("test_language_structures.py","structures"),("acceptance_native.py","acceptance"),("test_real_corpus.py","real")]:
    words=[sys.executable,"scripts/"+script,"--cq",cq,"--output",str(a.output/directory)]
    if script=="test_real_corpus.py" and a.fetch: words.append("--fetch")
    commands.append(words)
for index,command in enumerate(commands):
    log=a.output/f"{index:02d}.log"
    with log.open("w") as stream: result=subprocess.run(command,cwd=ROOT,stdout=stream,stderr=subprocess.STDOUT)
    meta["commands"].append({"command":command,"exit":result.returncode,"log":log.name})
    meta["passed"]=result.returncode==0 and len(meta["commands"])==len(commands)
    (a.output/"gate.json").write_text(json.dumps(meta,indent=2))
    print(json.dumps(meta["commands"][-1]),flush=True)
    if result.returncode: sys.exit(result.returncode)
