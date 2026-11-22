#!/usr/bin/env python3
"""Exercise each replay entry point of an existing, completed small fixture.

This intentionally mutates the specified execution. Use a disposable fixture,
never a production store or a repository-wide performance run.
"""

import argparse
import json
import subprocess
import time
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cq", type=Path, required=True)
    parser.add_argument("--store", type=Path, required=True)
    parser.add_argument("--run", required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    events = []

    def call(*command):
        started = time.monotonic()
        result = subprocess.run(
            [str(args.cq.resolve()), *command, "--store", str(args.store.resolve())],
            capture_output=True,
            text=True,
            timeout=300,
        )
        event = {
            "command": list(command),
            "seconds": round(time.monotonic() - started, 3),
            "exit": result.returncode,
            "stdout": result.stdout,
            "stderr": result.stderr,
        }
        events.append(event)
        (args.output / "events.json").write_text(
            json.dumps(events, ensure_ascii=False, indent=2) + "\n"
        )
        assert result.returncode == 0, event
        value = json.loads(result.stdout)
        assert "error" not in value, event
        return value

    status = call("run-status", "--run", args.run)
    stages = [entry["stage"] for entry in status["stages"]]
    assert len(stages) == 20, stages
    assert all(entry["attempt"]["state"] == "completed" for entry in status["stages"])
    cases = []
    for index, stage in enumerate(stages):
        before = call("run-status", "--run", args.run)
        old_head = call("status", "--project", args.project)["revision"]
        paused = call(
            "rerun-stage", "--run", args.run, "--stage", stage, "--until-stage", stage
        )
        assert paused["state"] == "paused", paused
        after = call("run-status", "--run", args.run)
        for position, (old, new) in enumerate(zip(before["stages"], after["stages"])):
            if position < index:
                assert old == new, (stage, old, new)
            elif position == index:
                assert new["attempt"]["number"] == old["attempt"]["number"] + 1
                assert new["attempt"]["state"] == "completed"
            else:
                assert new["attempt"]["state"] == "stale", (stage, new)
        # Only publish/validate are allowed to have changed HEAD at this point.
        if index < stages.index("publish"):
            assert call("status", "--project", args.project)["revision"] == old_head
        completed = call("resume", "--run", args.run)
        assert completed["state"] in ("complete", "complete_with_warnings"), completed
        final = call("run-status", "--run", args.run)
        assert all(entry["attempt"]["state"] == "completed" for entry in final["stages"])
        assert call("status", "--project", args.project)["revision"] == completed["revision"]
        cases.append({"stage": stage, "passed": True, "revision": completed["revision"]})
        print(f"{len(cases)}/{len(stages)} {stage}: passed", flush=True)
    summary = {
        "passed": True,
        "execution": args.run,
        "cases": cases,
        "commands": len(events),
        "scope": "existing disposable compiler fixture; no full repository benchmark",
    }
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")


if __name__ == "__main__":
    main()
