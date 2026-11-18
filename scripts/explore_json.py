#!/usr/bin/env python3
"""Reproducible, graph-led ripgrep JSON exploration through the public MCP API."""
import argparse
import json
import pathlib
import queue
import resource
import subprocess
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument('--cq', type=pathlib.Path, required=True)
parser.add_argument('--store', type=pathlib.Path, required=True)
parser.add_argument('--output', type=pathlib.Path, required=True)
parser.add_argument('--project', default='ripgrep')
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=True)
peer = subprocess.Popen([str(args.cq), 'mcp', '--store', str(args.store)], stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
responses = queue.Queue()
threading.Thread(target=lambda: [responses.put(json.loads(line)) for line in peer.stdout], daemon=True).start()
records = []


def rpc(method, params, label):
    number = len(records) + 1
    request = {'jsonrpc': '2.0', 'id': number, 'method': method, 'params': params}
    started = time.monotonic()
    peer.stdin.write(json.dumps(request) + '\n')
    peer.stdin.flush()
    response = responses.get(timeout=180)
    assert response.get('id') == number and 'result' in response, response
    result = response['result']
    assert not result.get('isError'), result
    result = json.loads(result['content'][0]['text']) if 'content' in result else result
    record = {'label': label, 'request': request, 'seconds': time.monotonic()-started,
              'response_bytes': len(json.dumps(response).encode()), 'response': result}
    records.append(record)
    (args.output / (label+'.json')).write_text(json.dumps(record, ensure_ascii=False, indent=2))
    print(label, round(record['seconds'], 3), 's', len(result.get('items', [])), 'items', flush=True)
    return result


def query(label, operation, **kw):
    return rpc('tools/call', {'name': 'query_codequest', 'arguments': {
        'project': args.project, 'operation': operation, 'limit': 30, 'max_work': 2000000,
        'max_bytes': 262144, 'timeout_ms': 30000, **kw}}, label)


try:
    rpc('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                      'clientInfo': {'name': 'ripgrep-exploration', 'version': '1'}}, 'initialize')
    query('overview-cold', 'overview', limit=7)
    query('overview-warm', 'overview', limit=7)
    query('schema', 'schema')
    found = {}
    for name, family in [('matched', 'JSONSink'), ('begin', 'JSONSink'), ('finish', 'JSONSink'),
                         ('write_begin_message', 'JSONSink'), ('write_message', 'JSON'), ('serialize', 'Data')]:
        file = 'crates/printer/src/jsont.rs' if family == 'Data' else 'crates/printer/src/json.rs'
        result = query(name+'-lookup', 'search', text=name, file=file, kind='Method', limit=100)
        candidates = [e for e in result['items'] if e['name'] == name and family in e['qualified_name']]
        assert candidates, (name, result)
        # Identity is not collapsed across compilation units. Use a declared unit consistently.
        chosen = next((e for e in candidates if e['unit'] == found.get('matched', {}).get('unit')), candidates[0])
        found[name] = chosen
        query(name+'-details', 'details', subject=chosen['id'], limit=7)
        query(name+'-callees', 'callees', subject=chosen['id'], limit=100, include_candidates=True)
        query(name+'-source', 'source', subject=chosen['id'], max_lines=100, limit=100)
        if name in ['matched', 'serialize']:
            query(name+'-docs', 'docs', subject=chosen['id'])
            query(name+'-trace', 'trace', subject=chosen['id'], depth=3, limit=30, include_candidates=True)
    # Follow an actual native callee to its author rationale, then inspect the constructor.
    serializer=query('serializer-with-external', 'callees', subject=found['serialize']['id'], include_candidates=True, include_external=True)
    base64=next(row['entity'] for row in serializer['items'] if row.get('entity',{}).get('name')=='base64_standard')
    base_docs=query('base64-docs','docs',subject=base64['id'])
    for index, doc in enumerate(base_docs['items']):query('base64-document-'+str(index),'document',subject=doc['id'])
    constructors=query('from-bytes-lookup','search',text='from_bytes',file='crates/printer/src/jsont.rs',kind='Method')
    constructor=next(e for e in constructors['items'] if e['name']=='from_bytes' and 'Data' in e['qualified_name'])
    query('from-bytes-callees','callees',subject=constructor['id'],include_external=True,include_candidates=True)
    query('from-bytes-source','source',subject=constructor['id'],max_lines=28)
    query('matched-contract','relations',subject=found['matched']['id'],relation='Overrides',include_external=True)
    query('json-sink-callers','callers',subject=found['matched']['id'],include_candidates=True)
    query('compiler-context', 'build_configuration', subject=found['matched']['unit'])
    query('matched-to-writer', 'path', subject=found['matched']['id'], target=found['write_message']['id'], depth=8)
    query('json-doc-search', 'doc_search', text='JSON', limit=7)
    query('semantic-global', 'semantic', text='serialize non UTF8 bytes JSON text base64', limit=7)
    query('semantic-printer', 'semantic', text='serialize non UTF8 bytes JSON text base64',
          file='crates/printer/src/jsont.rs', limit=7)
    for n in range(5):
        query('matched-callees-warm-'+str(n), 'callees', subject=found['matched']['id'], limit=7)
    (args.output/'selected-symbols.json').write_text(json.dumps(found, ensure_ascii=False, indent=2))
finally:
    peer.stdin.close()
    peer.wait(timeout=30)
    (args.output/'session.json').write_text(json.dumps({
        'records': records, 'exit': peer.returncode,
        'max_rss_kib': resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss,
        'stderr': peer.stderr.read()}, ensure_ascii=False, indent=2))
