#!/usr/bin/env python3
"""Replay retained fixtures against release store tests, one block at a time."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

from test_support.process_metrics import measured_run


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--binary", type=Path, help="already-built cq-store lib-test executable")
    parser.add_argument("--native-binary", type=Path, help="already-built native_import_performance integration-test executable")
    parser.add_argument("--raw-sample", type=Path)
    parser.add_argument("--raw-count", type=int, default=50000)
    parser.add_argument("--run-list", type=Path)
    parser.add_argument("--relations", type=Path)
    parser.add_argument("--scoped-fixture", type=Path, help="real scope ownership and retained canonical-spool offsets")
    parser.add_argument("--vectors", type=Path, help="same token-vector model as the reference index")
    parser.add_argument("--workers", type=int)
    parser.add_argument("--seconds", type=int, default=600, help="deadline per component, not an index target")
    args = parser.parse_args()
    if not any([args.raw_sample, args.run_list, args.relations, args.scoped_fixture]):
        parser.error("provide at least one retained fixture")
    if args.raw_count < 1 or args.seconds < 1 or (args.workers is not None and not 1 <= args.workers <= 128):
        parser.error("positive count/deadline and 1..128 workers required")
    root = Path(__file__).resolve().parents[1]
    output = args.output.resolve()
    if output.exists() and (not output.is_dir() or any(output.iterdir())):
        parser.error("--output must be a new or empty directory; existing reports are preserved")
    output.mkdir(parents=True, exist_ok=True)
    scratch = output / "scratch"
    scratch.mkdir(exist_ok=True)
    env = os.environ | {"TMPDIR": str(scratch)}
    if args.vectors:
        if not (args.vectors / "code_vectors.bin").is_file():
            parser.error("missing code_vectors.bin in --vectors")
        env["CQ_TOKEN_VECTORS"] = str(args.vectors.resolve())
    if args.workers is not None:
        env["CQ_WORKERS"] = str(args.workers)
    binary = args.binary.resolve() if args.binary else None
    native_binary = args.native_binary.resolve() if args.native_binary else None
    if binary is not None and not (binary.is_file() and os.access(binary, os.X_OK)):
        parser.error("--binary must be an executable cq-store lib-test binary")
    if native_binary is not None and not (native_binary.is_file() and os.access(native_binary, os.X_OK)):
        parser.error("--native-binary must be executable")
    if binary is None or (args.relations and native_binary is None):
        targets = []
        if binary is None:
            targets.append("--lib")
        if args.relations and native_binary is None:
            targets.extend(["--test", "native_import_performance"])
        command = ["cargo", "test", "--release", "-p", "cq-store", *targets, "--no-run", "--message-format=json"]
        print("编译性能用例 | 已完成 0/1", file=sys.stderr, flush=True)
        build = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True)
        (output / "build.stderr").write_text(build.stderr)
        if build.returncode:
            raise SystemExit(f"build failed; see {output / 'build.stderr'}")
        for line in build.stdout.splitlines():
            item = json.loads(line)
            if item.get("reason") == "compiler-artifact" and item.get("executable") and item["target"]["name"] == "cq_store":
                binary = Path(item["executable"])
            if item.get("reason") == "compiler-artifact" and item.get("executable") and item["target"]["name"] == "native_import_performance":
                native_binary = Path(item["executable"])
        if binary is None:
            raise SystemExit("missing cq-store lib-test executable")
        if args.relations and native_binary is None:
            raise SystemExit("missing native_import_performance test executable")
        print("编译性能用例 | 已完成 1/1", file=sys.stderr, flush=True)
    cases = []
    if args.raw_sample:
        cases.append(("writer", "真实记录写入", "retained_payloads_profile_the_complete_initial_writer", {
            "CQ_RAW_SAMPLE": str(args.raw_sample.resolve()), "CQ_RAW_COUNT": str(args.raw_count),
            "CQ_WRITER_REPORT": str(output / "writer.json"),
        }))
    if args.run_list:
        cases.append(("directories", "真实目录合并", "retained_runs_validate_bounded_writer_pipeline_against_sync_oracle", {
            "CQ_RUN_SAMPLE_LIST": str(args.run_list.resolve()), "CQ_RUN_REPORT": str(output / "directories.json"),
        }))
    if args.relations:
        cases.append(("native", "真实关系图导入", "retained_real_relations_profile_native_import_and_exact_replay", {
            "CQ_RELATION_SAMPLE": str(args.relations.resolve()), "CQ_NATIVE_IMPORT_REPORT": str(output / "native.json"),
        }))
    if args.scoped_fixture:
        cases.append(("scoped_encoding", "真实范围交付与编码", "retained_scopes_reproduce_production_encoding", {
            "CQ_ENCODING_FIXTURE": str(args.scoped_fixture.resolve()),
            "CQ_ENCODING_REPORT": str(output / "scoped_encoding.json"),
            "CQ_ENCODING_PROGRESS": str(output / "progress.json"),
        }))
    results = []
    manifest = {
        "binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "native_binary": str(native_binary) if native_binary else None,
        "native_binary_sha256": hashlib.sha256(native_binary.read_bytes()).hexdigest() if native_binary else None,
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
        "working_diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "HEAD"], cwd=root)).hexdigest(),
        "scope": "serial component replays; retained compiler inputs; no whole-repository index or end-to-end performance qualification",
        "deadline_seconds_per_component": args.seconds,
        "workers": env.get("CQ_WORKERS", "default: double available CPU, capped at 128"),
        "temporary_directory": str(scratch),
        "token_vectors": env.get("CQ_TOKEN_VECTORS"),
        "encoding_pending_batches": env.get("CQ_ENCODING_PENDING_BATCHES", "4"),
        "index_run_flush": env.get("CQ_INDEX_RUN_FLUSH", "async"),
        "scoped_fixture": str(args.scoped_fixture.resolve()) if args.scoped_fixture else None,
    }
    (output / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    for number, (name, label, test, overrides) in enumerate(cases):
        command = [str(native_binary if name == "native" else binary), test, "--ignored", "--nocapture", "--test-threads=1"]
        report_path = output / f"{name}.json"
        if report_path.exists():
            parser.error(f"refusing to reuse an existing report: {report_path}")
        stop = threading.Event()
        started = time.monotonic()

        def show():
            while not stop.is_set():
                elapsed = time.monotonic() - started
                detail = f"已完成 {number}/{len(cases)} 分块，剩余 {len(cases)-number}"
                if name == "scoped_encoding":
                    try:
                        v = json.loads((output / "progress.json").read_text())
                        detail = f"{v['phase']} | 已完成 {v['completed']:,}/{v['total']:,}"
                    except (OSError, ValueError, KeyError):
                        pass
                print(f"\r{label} | {detail} | {elapsed:.0f}s", end="", file=sys.stderr, flush=True)
                stop.wait(.5)

        ticker = threading.Thread(target=show, daemon=True)
        ticker.start()
        try:
            run, resources = measured_run(command, timeout=args.seconds, env=env | overrides, metrics_output=output / f"{name}.metrics.json")
            (output / f"{name}.stdout").write_text(run.stdout)
            (output / f"{name}.stderr").write_text(run.stderr)
            report = json.loads(report_path.read_text()) if report_path.exists() else {}
            row = {"block": name, "exit": run.returncode, "seconds": time.monotonic()-started, "passed": run.returncode == 0 and report.get("passed") is True, "resources": resources}
        except subprocess.TimeoutExpired as error:
            (output / f"{name}.stdout").write_text(error.output or "")
            (output / f"{name}.stderr").write_text(error.stderr or "")
            row = {"block": name, "exit": "timeout", "seconds": time.monotonic()-started, "passed": False}
        finally:
            stop.set()
            ticker.join()
            print(file=sys.stderr)
        results.append(row)
        (output / "summary.json").write_text(json.dumps({"passed": all(r["passed"] for r in results), "scope": manifest["scope"], "results": results}, ensure_ascii=False, indent=2) + "\n")
        print(f"{label} | 已完成 {number+1}/{len(cases)} 分块 | {row['seconds']:.3f}s | {'通过' if row['passed'] else '失败'}", file=sys.stderr, flush=True)
    print(json.dumps({"output": str(output), "passed": all(r["passed"] for r in results)}, ensure_ascii=False))
    return 0 if all(r["passed"] for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
