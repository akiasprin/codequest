#!/usr/bin/env python3
"""Fetch pinned Apache-2.0 token vectors, verifying every asset digest."""
from pathlib import Path
import hashlib,json,urllib.request,os
root=Path(__file__).resolve().parent.parent
lock=json.loads((root/'providers/embeddings/model.lock.json').read_text())
dest=Path(os.environ.get('CQ_TOOLS_DIR',root/'.codequest-tools'))/'embeddings'
dest.mkdir(parents=True,exist_ok=True)
for name,digest in lock['files'].items():
    path=dest/name
    if path.exists() and hashlib.sha256(path.read_bytes()).hexdigest()==digest: continue
    url=f"https://raw.githubusercontent.com/DeusData/codebase-memory-mcp/{lock['commit']}/{lock['directory']}/{name}"
    data=urllib.request.urlopen(url,timeout=60).read()
    assert hashlib.sha256(data).hexdigest()==digest, f'digest mismatch: {name}'
    path.write_bytes(data)
print(dest)
