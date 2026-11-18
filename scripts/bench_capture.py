#!/usr/bin/env python3
"""Paired cold/warm Cargo baseline and full CodeQuest observation, no network."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import time


def measure(command, directory, cwd, env):
    directory.mkdir(parents=True)
    record = directory / "resources.json"
    wrapped = ["/usr/bin/time", "-f", '{"wall_s":%e,"user_s":%U,"system_s":%S,"peak_kib":%M}', "-o", str(record), *map(str, command)]
    started = time.perf_counter()
    with (directory / "stdout").open("wb") as stdout, (directory / "stderr").open("wb") as stderr:
        result = subprocess.run(wrapped, cwd=cwd, env=env, stdout=stdout, stderr=stderr)
    elapsed = time.perf_counter() - started
    resources = json.loads(record.read_text())
    assert result.returncode == 0, (command, (directory / "stderr").read_text(), (directory / "stdout").read_text())
    return {"command": list(map(str, command)), "wall_ms": elapsed * 1000, "resources": resources}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cq", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--pairs", type=int, default=5)
    args = parser.parse_args()
    assert args.pairs >= 5
    root = Path(__file__).resolve().parent.parent
    output = args.output.resolve()
    assert not output.exists(), "Use a new output directory to preserve prior samples"
    output.mkdir(parents=True)
    project = output / "fixture"
    shutil.copytree(root / "fixtures/compile-workspace", project)
    cq = args.cq.resolve()
    source_files = sorted([root / "Cargo.toml", root / "Cargo.lock", *root.glob("crates/**/*.rs"), *root.glob("crates/**/Cargo.toml")])
    source_digest = hashlib.sha256(b"".join(str(p.relative_to(root)).encode() + b"\0" + p.read_bytes() for p in source_files)).hexdigest()
    env = os.environ.copy()
    env.update(LC_ALL="C")
    for key in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"):
        env.pop(key, None)
    samples = []
    for pair in range(args.pairs):
        baseline_target = output / "targets" / f"baseline-{pair}"
        capture_output = output / "captures" / str(pair)
        target = None
        for cache in ("cold", "warm"):
            baseline_env = {**env, "CARGO_TARGET_DIR": str(baseline_target), "CARGO_BUILD_BUILD_DIR": str(baseline_target)}
            baseline = ["cargo", "check", "--workspace", "--offline", "--message-format=json"]
            capture = [cq, "capture", "run", "--project", project, "--output", capture_output, "--mode", "exclusive"]
            if target:
                capture += ["--reuse-target", target]
            capture += ["--", "cargo", "check", "--workspace", "--offline"]
            # Alternate order within each matched pair; cold always precedes warm.
            commands = [("baseline", baseline, baseline_env), ("capture", capture, env)]
            if pair % 2:
                commands.reverse()
            results = {}
            for kind, command, command_env in commands:
                directory = output / "samples" / f"{pair}-{cache}-{kind}"
                results[kind] = measure(command, directory, project, command_env)
            summary = json.loads((output / "samples" / f"{pair}-{cache}-capture" / "stdout").read_text())
            report = json.loads(Path(summary["report_path"]).read_text())
            target = Path(report["plan"]["target"]["display"])
            results["capture"].update(run_id=summary["run_id"], report=summary["report_path"], cargo_ms=report["run"]["build_duration_ms"], seal_ms=report["run"]["seal_duration_ms"], observation_ms=report["duration_ms"], invocations=summary["invocations"], compile_invocations=sum(i["classification"] == "Compile" for i in report["invocations"]), journal_bytes=sum(f["blob"]["len"] for f in report["seal"]["value"]["files"]), content_bytes=sum(f["blob"]["len"] for f in report["snapshot"]["files"]), artifacts=summary["artifacts"], fresh=sum(a["fresh"] for a in report["artifacts"]))
            baseline_messages = []
            for line in (output / "samples" / f"{pair}-{cache}-baseline" / "stdout").read_text().splitlines():
                try:
                    message = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if message.get("reason") == "compiler-artifact":
                    baseline_messages.append(message)
            def identity(artifact):
                target = artifact.get("target", {})
                return (artifact.get("target_name", target.get("name")), artifact.get("target_kinds", target.get("kind")), artifact["profile"]["test"], artifact["features"])
            assert sorted(map(identity, baseline_messages)) == sorted(map(identity, report["artifacts"]))
            sample = {"pair": pair, "cache": cache, "order": [c[0] for c in commands], **results}
            samples.append(sample)
            with (output / "samples.jsonl").open("a") as f:
                f.write(json.dumps(sample, ensure_ascii=False) + "\n")
    medians = {}
    for cache in ("cold", "warm"):
        rows = [s for s in samples if s["cache"] == cache]
        medians[cache] = {"baseline_ms": statistics.median(s["baseline"]["wall_ms"] for s in rows), "capture_total_ms": statistics.median(s["capture"]["wall_ms"] for s in rows), "capture_cargo_ms": statistics.median(s["capture"]["cargo_ms"] for s in rows), "seal_ms": statistics.median(s["capture"]["seal_ms"] for s in rows), "paired_total_extra_ms": statistics.median(s["capture"]["wall_ms"] - s["baseline"]["wall_ms"] for s in rows), "paired_total_extra_percent": statistics.median((s["capture"]["wall_ms"] / s["baseline"]["wall_ms"] - 1) * 100 for s in rows), "baseline_peak_kib": statistics.median(s["baseline"]["resources"]["peak_kib"] for s in rows), "capture_peak_kib": statistics.median(s["capture"]["resources"]["peak_kib"] for s in rows)}
    result = {"date": datetime.datetime.now(datetime.timezone.utc).isoformat(), "source_digest": source_digest, "binary_digest": hashlib.sha256(cq.read_bytes()).hexdigest(), "platform": platform.platform(), "toolchain": subprocess.check_output(["rustc", "-vV"], text=True), "cargo": subprocess.check_output(["cargo", "-V"], text=True), "pairs": args.pairs, "scope": "Small dependency-free workspace; full metadata/source snapshot/toolchain/cfg probes plus capture. Cold means fresh Cargo target, not empty OS page cache. GNU time peak RSS is the maximum reported for a process, not a simultaneous process-tree RSS sum. Dedicated target paths and recorder environment differ, and all are recorded.", "medians": medians}
    (output / "summary.json").write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n")
    print(json.dumps(result, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
