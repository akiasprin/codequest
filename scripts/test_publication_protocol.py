#!/usr/bin/env python3
"""Real CLI pending replies, detached commits and crash recovery on Ladybug."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import time

ROOT = Path(__file__).resolve().parent.parent
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--cq', type=Path, default=ROOT/'target/debug/cq')
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=True)
a.cq = a.cq.resolve()
env = os.environ | {'CQ_STORE_HOST': str(a.cq.with_name('cq-store-host')), 'CQ_PUBLICATION_WAIT_MS': '0'}
results = []
stores = []

def check(ok, message):
    if not ok:
        raise AssertionError(message)

def run(store, *args):
    started = time.monotonic()
    r = subprocess.run([str(a.cq), *map(str, args), '--store', str(store)], env=env,
                       capture_output=True, text=True, timeout=60)
    elapsed = time.monotonic() - started
    with (a.output/'events.jsonl').open('a') as f:
        f.write(json.dumps({'args': list(map(str, args)), 'store': str(store), 'exit': r.returncode,
                            'stdout': r.stdout, 'stderr': r.stderr, 'seconds': elapsed})+'\n')
    check(r.returncode == 0, r.stderr)
    return json.loads(r.stdout), elapsed

def rpc(store, value):
    path = '/tmp/codequest-%s-%s.sock' % (os.geteuid(), hashlib.sha256(os.fsencode(store.resolve())).hexdigest()[:24])
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(5)
        s.connect(path)
        b = json.dumps(value).encode()
        s.sendall(struct.pack('>I', len(b))+b)
        def read(n):
            out = b''
            while len(out) < n:
                part = s.recv(n-len(out))
                check(part, 'host disconnected')
                out += part
            return out
        r = json.loads(read(struct.unpack('>I', read(4))[0]))
        check('error' not in r, r)
        return r['result']

fixture = a.output/'fixture'
r = subprocess.run(['cargo', 'test', '-p', 'cq-store', '--test', 'paged_store', '--offline',
                    'export_cli_acceptance_fixture', '--', '--ignored', '--exact'], cwd=ROOT,
                   env=env | {'CQ_PAGED_FIXTURE_OUTPUT': str(fixture.resolve())}, capture_output=True, text=True, timeout=120)
(a.output/'fixture.log').write_text(r.stdout+r.stderr)
check(r.returncode == 0, r.stderr)
seed = json.loads((fixture/'R1.json').read_text())
seed['entities'] = [e for e in seed['entities'] if e['id'] in ['f0', 'f1']]
seed['relations'] = [r for r in seed['relations'] if r['from'] == 'f0' and r['to'] == 'f1']
seed['contexts'] = []
seed['revision'] = ''
seed['revision'] = hashlib.sha256(json.dumps(seed, ensure_ascii=False, separators=(',', ':')).encode()).hexdigest()
candidate = a.output/'candidate.json'
candidate.write_text(json.dumps(seed, ensure_ascii=False, separators=(',', ':')))

try:
    for stage, crash in [('after-index', False), ('after-index', True), ('after-head', True)]:
        name = stage+('-crash' if crash else '-complete')
        store = a.output/name/'store'
        stores.append(store)
        import shutil
        shutil.copytree(fixture/'store/blobs', store/'blobs')
        project = store/'format-2/projects'/hashlib.sha256(b'paged-test').hexdigest()
        hooks = project/'test-hooks'
        hooks.mkdir(parents=True)
        (hooks/stage).touch()
        pending, reply_seconds = run(store, 'publish-snapshot', '--snapshot', candidate)
        check(pending.get('state') == 'publication_in_progress', pending)
        check(pending['revision'] == seed['revision'], pending)
        end = time.monotonic()+30
        while not (hooks/('ready-'+stage)).exists():
            check(time.monotonic() < end, 'publication did not reach barrier')
            time.sleep(.01)
        query = json.dumps({'operation': 'publication_status', 'revision': pending['revision']})
        status, status_seconds = run(store, 'query', '--project', 'paged-test', '--json', query)
        check(status['state'] == 'in_progress', status)
        # The frontend has already exited. Only the detached host owns the job.
        pid = rpc(store, {'op': 'ping'})['pid']
        if crash:
            os.kill(pid, signal.SIGKILL)
            (hooks/stage).unlink()
        else:
            (hooks/('release-'+stage)).touch()
        while True:
            status, _ = run(store, 'query', '--project', 'paged-test', '--json', query)
            if status.get('result'):
                break
            check(time.monotonic() < end, 'publication did not settle')
            time.sleep(.01)
        outcome = 'NotCommitted' if crash and stage == 'after-index' else 'Committed'
        check(status['result']['outcome'] == outcome, status)
        if outcome == 'NotCommitted':
            run(store, 'publish-snapshot', '--snapshot', candidate)
            while True:
                status, _ = run(store, 'query', '--project', 'paged-test', '--json', query)
                if status.get('result'):
                    break
                check(time.monotonic() < end, 'retry did not settle')
                time.sleep(.01)
            check(status['result']['outcome'] == 'Committed', status)
        view, _ = run(store, 'query', '--project', 'paged-test', '--json', '{"operation":"search","text":"^f0$","regex":true}')
        check(view['revision'] == seed['revision'] and len(view['items']) == 1, view)
        results.append({'case': name, 'outcome_after_barrier': outcome, 'final_outcome': 'Committed',
                        'pending_reply_seconds': reply_seconds, 'status_reply_seconds': status_seconds})
        os.kill(rpc(store, {'op': 'ping'})['pid'], signal.SIGTERM)
finally:
    for store in stores:
        try:
            os.kill(rpc(store, {'op': 'ping'})['pid'], signal.SIGTERM)
        except (OSError, AssertionError):
            pass
    (a.output/'results.json').write_text(json.dumps({'passed': len(results) == 3, 'cases': results}, indent=2))
