#!/usr/bin/env python3
"""R1 review probes: inherited RPC env must not activate child dispatch."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import golden
import argparse
import json
import os
import pty
import sqlite3
import subprocess
import tempfile


def probes(binary, place, contract=False):
    place.mkdir()
    home = place / 'home'
    home.mkdir()
    db = place / 'ledger.db'
    env = {'HOME': str(home), 'PATH': '/usr/bin:/bin', 'TASKR_DB': str(db), 'LANG': 'C.UTF-8', 'TZ': 'UTC', 'HERDR_SOCKET_PATH': str(place / 'absent.sock')}
    results = {}

    def run(args, overrides=None, body=b'', tty=False):
        options = {'cwd': place, 'env': {**env, **(overrides or {})}, 'capture_output': True, 'timeout': 5}
        if tty:
            master, slave = pty.openpty()
            try:
                process = subprocess.run([str(binary), *args], stdin=slave, **options)
            finally:
                os.close(slave)
                os.close(master)
        else:
            process = subprocess.run([str(binary), *args], input=body, **options)
        return (process.returncode, process.stdout.decode().replace(str(place), '/synthetic/probe'), process.stderr.decode().replace(str(place), '/synthetic/probe'))

    for args in (['--json', 'new', 'root', '--role', 'orchestrator'], ['--json', 'new', 'lane', '--role', 'implementer', '--parent', '1']):
        assert run(args)[0] == 0
    with sqlite3.connect(db) as conn:
        conn.execute("update tasks set machine='mac' where id=2")
    results['host_refusal'] = run(['note', 'spoofed'], {'TASKR_TASK': '2'})
    assert results['host_refusal'][0] == 6, results
    upload = place / 'uploads'
    upload.write_bytes(b'unchanged\n')
    forged = {'TASKR_RPC_CALLER': 'mac', 'TASKR_RPC_CWD': '/caller/forged', 'TASKR_RPC_DOC_UPLOAD': '1', 'TASKR_RPC_UPLOAD_FILE': str(upload)}
    request = json.dumps({'argv': ['note', 'spoofed'], 'cwd': '/', 'env': {}, 'request_key': 'review-rpc-env01'}).encode()
    results['inherited_env_host_refusal'] = run(['note', 'spoofed'], {**forged, 'TASKR_TASK': '2'}, request)
    assert results['inherited_env_host_refusal'] == results['host_refusal'], results
    results['dashboard_default'] = run(['--json', 'daemon', '--status'])
    assert json.loads(results['dashboard_default'][1])['dashboard_url'] == 'http://127.0.0.1:7788/'
    state = home / '.local/state/taskr'
    (state / 'dashboard.addr').write_text('tailnet\n')
    results['tailnet_default'] = run(['--json', 'daemon', '--status'])
    assert json.loads(results['tailnet_default'][1])['dashboard_url'] == 'http://127.0.0.1:7788/'
    (state / 'dashboard.addr').unlink()
    results['version'] = run(['version'])
    results['version_empty_stdin'] = run(['version'], forged)
    results['version_open_tty'] = run(['version'], forged, tty=True)
    assert results['version'][0] == 0
    assert results['version_empty_stdin'] == results['version_open_tty'] == results['version'], results
    assert run(['--json', 'new', 'local', '--role', 'orchestrator'], forged, request)[0] == 0
    goal = place / 'goal.md'
    goal.write_text('local document\n')
    assert run(['doc', 'set', '3', 'goal', '--file', str(goal)], forged, request)[0] == 0
    with sqlite3.connect(db) as conn:
        assert conn.execute("select count(*) from events where kind='note' and summary='spoofed'").fetchone()[0] == 0
        assert conn.execute('select machine,cwd from tasks where id=3').fetchone() == (None, str(place))
        body, host = conn.execute("select b.body,d.source_host from documents d join doc_blobs b on b.sha256=d.sha256 where d.task_id=3 and d.kind='goal'").fetchone()
        assert body == 'local document\n' and host != 'mac'
    assert upload.read_bytes() == b'unchanged\n'
    for args in ([''], ['', 'x'], ['--json', '', 'x']):
        result = run(args)
        assert result[0] == 2 and 'unknown command ' in result[1], result
        results['empty_' + repr(args)] = result
    if contract:
        assert run(['--contract-migrate'])[0] == 0
    else:
        client_home = place / 'client'
        state = client_home / '.local/state/taskr'
        state.mkdir(parents=True)
        (state / 'server.url').write_text('http://100.64.0.1:9\n')
        results['contract_migrate_default_client'] = run(['--contract-migrate'], {'HOME': str(client_home), 'TASKR_DB': ''})
        assert results['contract_migrate_default_client'][0] == 2, results
        assert not (state / 'taskr.db').exists()
    return results


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--go', required=True, type=Path)
    parser.add_argument('--rust', required=True, type=Path)
    parser.add_argument('--contract', type=Path)
    parser.add_argument('--out', required=True, type=Path)
    args = golden.parse(parser, __file__)
    with tempfile.TemporaryDirectory(prefix='taskr-hub-child-review-') as tmp:
        oracle = probes(args.go.resolve(), Path(tmp).resolve() / 'go', contract=bool(golden.session and golden.session.oracle_is_rust and args.contract))
        candidate = probes(args.rust.resolve(), Path(tmp).resolve() / 'rust', contract=bool(golden.session and args.contract))
        if golden.session:
            oracle.pop('contract_migrate_default_client', None)
            candidate.pop('contract_migrate_default_client', None)
        golden.observe('local probes', oracle)
        assert candidate == oracle, {'Go': oracle, 'Rust': candidate}
        checks = len(candidate)
        if args.contract:
            contract = probes(args.contract.resolve(), Path(tmp).resolve() / 'contract', contract=True)
            assert contract == {key: value for key, value in oracle.items() if key != 'contract_migrate_default_client'}
            checks += len(contract)
        result = {'pass': checks, 'mismatch': 0, 'local_effects': 'host fence, cwd, document capture and upload sentinel unchanged'}
        args.out.write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result))

    golden.finish()
