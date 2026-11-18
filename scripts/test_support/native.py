"""Native compiler fixtures and public MCP query assertions."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import queue
import subprocess
import threading
import time
import unittest
from .process_metrics import measured_run


class QueryError(AssertionError):
    pass


class NativeProject:
    def __init__(self, cq: Path, root: Path, output: Path, name: str, *, environment=None, repetitions=1, diagnostic_export=True, **options):
        self.cq, self.root, self.output, self.name = cq.resolve(), root.resolve(), output, name
        output.mkdir(parents=True, exist_ok=True)
        self.store = output / "store"
        (output / "codequest-binary.json").write_text(json.dumps({"path":str(self.cq),
            "sha256":hashlib.sha256(self.cq.read_bytes()).hexdigest()},indent=2))
        command = [str(self.cq), "index", "--store", str(self.store), "--root", str(self.root), "--name", name]
        for key, value in options.items():
            if key != "build_args":
                command += ["--" + key.replace("_", "-"), str(value)]
        if options.get("build_args"):
            command += ["--", *options["build_args"]]
        measurements=[]
        for repeat in range(repetitions):
            started=time.monotonic()
            result,resources=measured_run(command,store=self.store,timeout=2400,env=environment)
            measurement={"command":command,"exit":result.returncode,"seconds":time.monotonic()-started,"stdout":result.stdout,"stderr":result.stderr,"repeat":repeat+1,"resources":resources}
            measurements.append(measurement)
            (output/"index-runs.json").write_text(json.dumps(measurements,indent=2))
            (output/"index.json").write_text(json.dumps(measurement,indent=2))
            if result.returncode:raise AssertionError(f"native index failed: {result.stderr}")
        self.summary = json.loads(result.stdout)
        self.revision = self.summary["revision"]
        # The snapshot is evidence for diagnostics, not a source of expected answers.
        self.snapshot_path = self.store / "revisions" / self.revision / "snapshot.json"
        if diagnostic_export and not self.snapshot_path.exists():
            self.snapshot_path = output / "explicit-diagnostic-snapshot.json"
            export = subprocess.run([str(self.cq), "export-snapshot", "--store", str(self.store),
                "--project", name, "--revision", self.revision, "--output", str(self.snapshot_path)],
                capture_output=True, text=True, timeout=300, env=environment)
            if export.returncode:
                raise AssertionError(f"diagnostic export failed: {export.stderr}")
        self.stderr_file = (output / "mcp.stderr").open("w")
        self.peer = subprocess.Popen([str(self.cq), "mcp", "--store", str(self.store)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr_file, text=True, bufsize=1)
        self.responses = queue.Queue()
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()
        self.sequence = 0
        try:
            self.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                "clientInfo": {"name": "native-structure-tests", "version": "1"}})
        except BaseException:
            self.close()
            raise

    def _read(self):
        for line in self.peer.stdout:
            try:
                self.responses.put(json.loads(line))
            except ValueError as error:
                self.responses.put({"protocol_error": str(error), "line": line})
        self.responses.put({"protocol_error": "MCP EOF"})

    def rpc(self, method, params):
        self.sequence += 1
        request = {"jsonrpc": "2.0", "id": self.sequence, "method": method, "params": params}
        started = time.monotonic()
        self.peer.stdin.write(json.dumps(request) + "\n")
        self.peer.stdin.flush()
        response = self.responses.get(timeout=180)
        with (self.output / "queries.jsonl").open("a") as log:
            log.write(json.dumps({"request": request, "response": response,
                "seconds": time.monotonic() - started}) + "\n")
        if response.get("id") != self.sequence or "result" not in response:
            raise QueryError(str(response))
        result = response["result"]
        if result.get("isError"):
            raise QueryError(result["content"][0]["text"])
        return json.loads(result["content"][0]["text"]) if "content" in result else result

    def page(self, operation, **kwargs):
        return self.rpc("tools/call", {"name": "query_codequest", "arguments": {
            "project": self.name, "operation": operation, "limit": 100,
            "max_bytes": 512 * 1024, "max_work": 2_000_000, "timeout_ms": 30000, **kwargs}})

    def query(self, operation, **kwargs):
        page = self.page(operation, **kwargs)
        rows = list(page["items"])
        cursors = set()
        while page.get("next_cursor"):
            cursor = page["next_cursor"]
            if cursor in cursors:
                raise AssertionError("continuation cycle")
            cursors.add(cursor)
            page = self.page("next", cursor=cursor, limit=kwargs.get("limit", 100))
            rows.extend(page["items"])
        return rows

    def find(self, name, *, file=None, kind=None, qualified=None, unit=None):
        rows = self.query("search", text="^" + __import__("re").escape(name) + "$", regex=True,
            **({"file": file} if file else {}), **({"kind": kind} if kind else {}))
        return [r for r in rows if r["name"] == name
            and (qualified is None or qualified in r["qualified_name"])
            and (unit is None or unit == r["unit"])]

    def one(self, name, **kwargs):
        rows = self.find(name, **kwargs)
        if len(rows) != 1:
            raise AssertionError(f"{self.name} {name} {kwargs}: expected one identity, got "
                + str([(r["kind"], r["qualified_name"], r.get("signature"), r["unit"]) for r in rows]))
        return rows[0]

    def calls(self, entity, **kwargs):
        return self.query("callees", subject=entity["id"], **kwargs)

    def native_exports(self):
        """Inspect retained raw facts independently of normalization/querying."""
        snapshot=json.loads(self.snapshot_path.read_text())
        result=[]
        for provider in snapshot["providers"]:
            digest=provider["protocol"].split("raw=",1)[1].split(";",1)[0]
            data=(self.store/"blobs"/digest).read_bytes()
            assert hashlib.sha256(data).hexdigest()==digest
            result.append(json.loads(data))
        return result

    def close(self):
        if self.peer.poll() is None:
            self.peer.stdin.close()
            try:
                self.peer.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.peer.kill()
                self.peer.wait()
        self.reader.join(timeout=2)
        self.peer.stdout.close()
        self.stderr_file.close()
        if self.peer.returncode:
            raise AssertionError(f"MCP exited {self.peer.returncode}")


def compdb(root, output, files):
    entries = []
    for file in files:
        compiler = "clang" if file.endswith(".c") else "clang++"
        args = [compiler, "-c", str(root / file), "-I", str(root), "-o", str(output / (file + ".o"))]
        if compiler == "clang++":
            args += ["-std=c++17"]
        entries.append({"file": str(root / file), "directory": str(root), "arguments": args})
    path = output / "compile_commands.json"
    path.write_text(json.dumps(entries, indent=2))
    return path


class EvidenceResult(unittest.TextTestResult):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.cases = []

    def addSuccess(self, test):
        super().addSuccess(test)
        self.cases.append({"test": test.id(), "state": "passed"})

    def addFailure(self, test, err):
        super().addFailure(test, err)
        self.cases.append({"test": test.id(), "state": "failed", "error": self._exc_info_to_string(err, test)})

    def addError(self, test, err):
        super().addError(test, err)
        self.cases.append({"test": test.id(), "state": "error", "error": self._exc_info_to_string(err, test)})
