#!/usr/bin/env python3
"""Build/run the isolated Ladybug R0 probe and retain process-tree measurements."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import threading
import time


def process_status(pid):
    try:
        values = {}
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            key, _, value = line.partition(":")
            if key in ("VmRSS", "VmHWM"):
                values[key] = int(value.split()[0]) * 1024
        return values
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return {}


def process_tree(pid):
    found = {pid}
    pending = [pid]
    while pending:
        current = pending.pop()
        try:
            for task in Path(f"/proc/{current}/task").iterdir():
                for value in (task / "children").read_text().split():
                    child = int(value)
                    if child not in found:
                        found.add(child)
                        pending.append(child)
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            pass
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="fresh, nonexisting probe directory")
    parser.add_argument("--case", help="run one named experiment")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    output = args.output.resolve()
    logs = output.with_name(output.name + "-logs")
    if output.exists() or logs.exists():
        parser.error("output and its -logs directory must not exist")
    logs.mkdir(parents=True)
    build = ["cargo", "build", "-p", "cq-store", "--example", "store_r0", "--locked", "--offline"]
    with (logs / "build.stdout").open("w") as stdout, (logs / "build.stderr").open("w") as stderr:
        result = subprocess.run(build, cwd=repo, stdout=stdout, stderr=stderr)
    if result.returncode:
        sys.stderr.write((logs / "build.stderr").read_text())
        return result.returncode
    def file_hash(path):
        h=hashlib.sha256()
        with path.open('rb') as source:
            for block in iter(lambda:source.read(1024*1024),b''):h.update(block)
        return h.hexdigest()
    source_paths=[*sorted((repo/'crates/cq-store/examples/store_r0').glob('*.rs')),*sorted((repo/'crates/cq-store/examples/store_r0').glob('*.c')),repo/'scripts/run_store_r0.py',repo/'Cargo.lock',repo/'Cargo.toml']
    sources={str(p.relative_to(repo)):file_hash(p) for p in source_paths}
    executable_hash=file_hash(repo/'target/debug/examples/store_r0')
    command = [str(repo / "target/debug/examples/store_r0"), "--output", str(output)]
    if args.case:
        command += ["--case", args.case]
    started = time.monotonic()
    samples = []
    peaks = {}
    active_case = [None]
    done = threading.Event()
    with (logs / "stderr.log").open("w") as stderr, (logs / "stdout.log").open("w") as stdout:
        peer = subprocess.Popen(command, cwd=repo, stdout=subprocess.PIPE, stderr=stderr, text=True, bufsize=1)

        def sample():
            while not done.is_set():
                statuses = {pid: process_status(pid) for pid in process_tree(peer.pid)}
                for pid, status in statuses.items():
                    peaks[pid] = max(peaks.get(pid, 0), status.get("VmHWM", 0))
                samples.append({"seconds": time.monotonic() - started, "case": active_case[0],
                                "parent_rss": statuses.get(peer.pid, {}).get("VmRSS", 0),
                                "tree_rss": sum(status.get("VmRSS", 0) for status in statuses.values())})
                done.wait(0.1)

        observer = threading.Thread(target=sample, daemon=True)
        observer.start()
        for line in peer.stdout:
            if line.startswith("START "):
                active_case[0] = line.strip().split(" ", 1)[1]
            stdout.write(line)
            stdout.flush()
            print(line, end="", flush=True)
        code = peer.wait()
        done.set()
        observer.join()
    elapsed = time.monotonic() - started
    resources = {"sampling_interval_ms": 100, "includes_build": False,
                 "peak_parent_rss": max((s["parent_rss"] for s in samples), default=0),
                 "peak_tree_rss": max((s["tree_rss"] for s in samples), default=0),
                 "per_pid_peak_hwm": peaks, "samples": samples}
    (logs / "resources.json").write_text(json.dumps(resources, indent=2))
    cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                if line.startswith("model name")), "unknown")
    memory = next(line for line in Path("/proc/meminfo").read_text().splitlines() if line.startswith("MemTotal"))
    metadata = {"command": command, "build_command": build, "profile": "Rust dev; native library from locked lbug build",
                "git_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
                "rustc": subprocess.check_output(["rustc", "--version", "--verbose"], text=True),
                "kernel": platform.release(), "cpu": cpu, "logical_cpus": os.cpu_count(),
                "memory": memory, "seconds": elapsed, "exit_code": code,
                "source_sha256": sources,"executable_sha256":executable_hash,
                "measurement_limits": "100 ms sampling can miss short peaks; no power-loss fault injection or compiler capture"}
    (logs / "metadata.json").write_text(json.dumps(metadata, indent=2))
    print(f"R0 exit={code}; {elapsed:.2f}s; peak process tree RSS={resources['peak_tree_rss'] / 1024 ** 2:.1f} MiB")
    return code


if __name__ == "__main__":
    sys.exit(main())
