#!/usr/bin/env python3
"""Fetch exact corpus revisions; existing checkouts must already match the lock."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def prepare(destination, *, fetch=False, sources=None):
    lock = json.loads((ROOT / "fixtures/real-corpus.lock.json").read_text())
    sources = sources or {}
    destination.mkdir(parents=True, exist_ok=True)
    for repo in lock["repositories"]:
        target = destination / repo["name"]
        if not target.exists():
            if not fetch:
                raise RuntimeError(f"missing {target}; run prepare_test_corpus.py --fetch")
            with tempfile.TemporaryDirectory(prefix="corpus-fetch-", dir=destination) as tmp:
                temporary = Path(tmp) / "repo"
                subprocess.run(["git", "init", "--quiet", str(temporary)], check=True)
                subprocess.run(["git", "-C", str(temporary), "fetch", "--quiet", "--depth=1",
                    str(sources.get(repo["name"], repo["url"])), repo["commit"]], check=True)
                subprocess.run(["git", "-C", str(temporary), "checkout", "--quiet", "--detach", repo["commit"]], check=True)
                temporary.rename(target)
        actual = subprocess.check_output(["git", "-C", str(target), "rev-parse", "HEAD"], text=True).strip()
        if actual != repo["commit"]:
            raise RuntimeError(f"{target}: locked commit differs; existing checkout left untouched")
        if subprocess.check_output(["git", "-C", str(target), "status", "--porcelain"], text=True):
            raise RuntimeError(f"{target}: corpus working tree is dirty")
        print(repo["name"], actual, flush=True)
    return lock


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, default=ROOT / ".codequest-tools/test-corpus")
    parser.add_argument("--fetch", action="store_true")
    parser.add_argument("--source", action="append", default=[], metavar="NAME=LOCAL_GIT")
    args = parser.parse_args()
    prepare(args.corpus, fetch=args.fetch, sources=dict(s.split("=", 1) for s in args.source))
