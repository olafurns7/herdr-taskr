#!/usr/bin/env python3
"""Synthetic daemon parity/lifecycle cell. Never uses installed taskr or Herdr."""
import argparse
import json
import os
from pathlib import Path
import selectors
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[3]


class Herdr:
    def __init__(self, home):
        self.home = home
        self.path = home / 'h.sock'
        self.streams = []
        self.requests = []
        self.subscriptions = {}
        self.extra_bytes = []
        self.stop = threading.Event()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.path))
        self.listener.listen()
        self.listener.settimeout(.1)
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()
        (home / 'agents.json').write_text('[]')
        (home / 'calls').write_text('')
        tools = home / 'bin'
        tools.mkdir()
        script = tools / 'herdr'
        script.write_text('''#!/usr/bin/python3
import json, os, sys
from pathlib import Path
h=Path(os.environ['HOME']); a=sys.argv[1:]
with (h/'calls').open('a') as f: f.write(json.dumps(a)+'\\n')
if a[:2]==['agent','list']: result={'agents':json.loads((h/'agents.json').read_text())}
elif a[:2]==['workspace','list']: result={'workspaces':[]}
elif a[:2]==['pane','list']: result={'panes':[]}
else: result={}
print(json.dumps({'result':result}))
''')
        script.chmod(0o755)

    def accept(self):
        while not self.stop.is_set():
            try:
                conn, _ = self.listener.accept()
            except (socket.timeout, OSError):
                continue
            threading.Thread(target=self.handle, args=(conn,), daemon=True).start()

    def handle(self, conn):
        conn.settimeout(.1)
        data = b''
        try:
            while b'\n' not in data and not self.stop.is_set():
                try:
                    chunk = conn.recv(4096)
                except socket.timeout:
                    continue
                if not chunk:
                    return
                data += chunk
            if not data:
                return
            req = json.loads(data.split(b'\n', 1)[0])
            assert req['method'] == 'events.subscribe', req
            assert all(s.get('pane_id') for s in req['params']['subscriptions'] if s['type'] == 'pane.agent_status_changed'), req
            self.requests.append(req)
            self.streams.append(conn)
            self.subscriptions[conn] = req['params']['subscriptions']
            conn.sendall(b'{"id":"taskr-daemon","result":{"type":"subscription_started"}}\n')
            while not self.stop.is_set():
                try:
                    chunk = conn.recv(4096)
                except socket.timeout:
                    continue
                if not chunk:
                    return
                self.extra_bytes.append(chunk)
        except (OSError, ValueError):
            pass
        finally:
            self.subscriptions.pop(conn, None)
            conn.close()

    def wake(self, pane_id='wLane:p1'):
        sent = 0
        for conn in list(self.streams):
            if not any(s['type'] == 'pane.agent_status_changed' and s.get('pane_id') == pane_id
                       for s in self.subscriptions.get(conn, [])):
                continue
            try:
                conn.sendall((json.dumps({'event': 'pane.agent_status_changed', 'pane_id': pane_id}) + '\n').encode())
                sent += 1
            except OSError:
                pass
        return sent

    def close(self):
        self.stop.set()
        self.listener.close()
        self.path.unlink(missing_ok=True)
        for conn in self.streams:
            conn.close()
        self.thread.join(timeout=1)


def eventually(check, timeout=12):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if check():
            return
        threading.Event().wait(.02)
    raise AssertionError('condition did not become true')


def env(home, fake):
    return {'HOME':str(home), 'TASKR_DB':str(home/'ledger.db'), 'PATH':str(home/'bin')+':/usr/bin:/bin',
            'HERDR_SOCKET_PATH':str(fake.path), 'TASKR_FROZEN_NOW':'2026-10-08T00:00:00Z', 'TASKR_CONTRACT_ORACLE':'1', 'LANG':'C.UTF-8', 'TZ':'UTC'}


def invoke(binary, environment, *args):
    p = subprocess.run([str(binary), *args], env=environment, capture_output=True, timeout=20)
    return p.returncode, p.stdout, p.stderr


def record(binary, environment, *args):
    code, out, err = invoke(binary, environment, '--json', *args)
    assert code == 0, (args, code, out, err)
    return json.loads(out.splitlines()[-1])


def start(binary, environment, *args):
    p = subprocess.Popen([str(binary), 'daemon', *args], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    sel = selectors.DefaultSelector()
    sel.register(p.stdout, selectors.EVENT_READ)
    assert sel.select(15), 'daemon startup timed out'
    line = p.stdout.readline()
    sel.close()
    assert line, p.stderr.read()
    first = json.loads(line.removeprefix(b'j1 '))
    assert first.get('pid') == p.pid and first['ok'], first
    return p


def stop(p):
    if p.poll() is None:
        p.terminate()
    out, err = p.communicate(timeout=15)
    assert p.returncode == 0, (p.returncode, out, err)


def run(go, rust, output):
    results = []
    with tempfile.TemporaryDirectory(prefix='taskr-d-') as temp:
        home = Path(temp)
        state = home/'.local/state/taskr'
        state.mkdir(parents=True)
        (state/'dashboard.addr').write_text('off')
        fake = Herdr(home)
        e = env(home, fake)
        procs = []
        try:
            record(rust,e,'status')
            for args in [('daemon','--status'),('daemon','--help'),('daemon','--once','--stay'),('daemon','--once','--status')]:
                g=invoke(go,e,*args);r=invoke(rust,e,*args)
                assert g==r,(args,g,r)
                results.append({'case':list(args),'pass':True})
            for heartbeat in [None,'2026-10-07T23:59:50.000Z','2026-10-07T23:58:00.000Z','bad']:
                with sqlite3.connect(e['TASKR_DB']) as db:
                    db.execute("delete from meta where key='daemon_heartbeat'")
                    if heartbeat is not None: db.execute("insert into meta values('daemon_heartbeat',?)",(heartbeat,))
                for json_mode in [False,True]:
                    args=(['--json'] if json_mode else [])+['daemon','--status']
                    g=invoke(go,e,*args);r=invoke(rust,e,*args)
                    assert g==r,(heartbeat,args,g,r)
                    results.append({'case':['status',heartbeat,json_mode],'pass':True})
            # Both target pairs contend on the same unchanged Go lock file.
            for holder,challenger,label in [(rust,rust,'rust-rust'),(rust,go,'rust-go'),(go,rust,'go-rust')]:
                p=start(holder,e);procs.append(p)
                out=record(challenger,e,'daemon')
                assert out=={'already_running':True,'ok':True,'pid':p.pid},out
                assert p.poll() is None
                def fresh_holder():
                    with sqlite3.connect(e['TASKR_DB']) as db:
                        return db.execute("select value from meta where key='daemon_heartbeat'").fetchone()==('2026-10-08T00:00:00.000Z',)
                eventually(fresh_holder)
                g=invoke(go,e,'--json','daemon','--status');r=invoke(rust,e,'--json','daemon','--status')
                assert g==r,(label,g,r)
                stop(p);procs.remove(p)
                assert (state/'daemon.lock').read_bytes()==b''
                assert not (state/'daemon.json').exists()
                results.append({'case':['lock',label],'pass':True})
            # Restart refuses an unrelated lock holder and leaves it alive.
            helper=subprocess.Popen([sys.executable,'-c',
                "import fcntl,os,sys,time; f=open(sys.argv[1],'w'); fcntl.flock(f,fcntl.LOCK_EX); f.write(str(os.getpid())+'\\n'); f.flush(); print('ready',flush=True); time.sleep(30)",str(state/'daemon.lock')],stdout=subprocess.PIPE)
            assert helper.stdout.readline()==b'ready\n'
            try:
                for forged in [False,True]:
                    with sqlite3.connect(e['TASKR_DB']) as db:
                        db.execute("delete from meta where key in('daemon_pid','daemon_exe','daemon_proc_start')")
                        if forged:
                            proc=Path('/proc')/str(helper.pid)
                            exe=str((proc/'exe').resolve())
                            stamp=(proc/'stat').read_text().rsplit(')',1)[1].split()[19]
                            db.executemany('insert into meta values(?,?)',[('daemon_pid',str(helper.pid)),('daemon_exe',exe),('daemon_proc_start',stamp)])
                    g=invoke(go,e,'daemon','--restart');r=invoke(rust,e,'daemon','--restart')
                    assert g==r and r[0]==6,(forged,g,r)
                    assert helper.poll() is None
                    results.append({'case':['restart refusal preserves unrelated PID',forged],'pass':True})
            finally:
                helper.terminate();helper.wait(timeout=5)
                (state/'daemon.lock').write_text('')
            # Rust owner sound is P1a: claim only blocking owner asks, once.
            top=record(rust,e,'new','top','--role','orchestrator')['task_id']
            worker=record(rust,e,'new','worker','--role','implementer','--parent',str(top),'--pane','w1:p1')['task_id']
            launch=record(rust,e,'launch',str(worker),'--provider','fixture','--model','fixture','--effort','medium')['launch_id']
            we={**e,'TASKR_TASK':str(worker),'TASKR_LAUNCH':str(launch)}
            silent=record(rust,we,'ask','silent owner','--owner')['ask_id']
            audible=record(rust,we,'ask','blocking owner','--owner','--blocking')['ask_id']
            for _ in range(2): record(rust,e,'daemon','--once')
            calls=[json.loads(line) for line in (home/'calls').read_text().splitlines()]
            notify=[a for a in calls if a[:2]==['notification','show']]
            assert len(notify)==1 and 'blocking owner' in notify[0],notify
            with sqlite3.connect(e['TASKR_DB']) as db:
                assert db.execute('select key from meta where key like ?',('notified:%',)).fetchall()==[(f'notified:{audible}',)]
                assert silent!=audible
            results.append({'case':['P1a blocking-only once'],'pass':True})
            # Subscription changes with fresh snapshot, and emits no extra client bytes.
            (home/'agents.json').write_text(json.dumps([{'name':'worker','pane_id':'w1:p1','agent_status':'working','state_change_seq':1}]))
            p=start(rust,e);procs.append(p)
            def heartbeat():
                with sqlite3.connect(e['TASKR_DB']) as db: return db.execute("select value from meta where key='daemon_heartbeat'").fetchone() is not None
            eventually(heartbeat)
            record(rust,e,'answer',str(silent),'done')
            record(rust,e,'answer',str(audible),'done')
            record(rust,we,'ready','slice')
            fake.wake('w1:p1')
            def ready_token():
                return any('taskr_state=ready' in json.loads(line) for line in (home/'calls').read_text().splitlines())
            eventually(ready_token)
            assert fake.requests and not fake.extra_bytes,(fake.requests,fake.extra_bytes)
            fake.close()
            out,err=p.communicate(timeout=15)
            assert p.returncode==0,(out,err)
            procs.remove(p)
            results.append({'case':['subscription, fresh pass, token, removed socket'],'pass':True})
        finally:
            for p in procs: stop(p)
            if not fake.stop.is_set():fake.close()
    output.write_text(json.dumps({'passed':len(results),'results':results},indent=2)+'\n')
    print(json.dumps({'passed':len(results),'mismatch':0}))


if __name__=='__main__':
    p=argparse.ArgumentParser()
    p.add_argument('--go',type=Path,required=True)
    p.add_argument('--rust',type=Path,required=True)
    p.add_argument('--out',type=Path,required=True)
    a=p.parse_args()
    run(a.go.resolve(),a.rust.resolve(),a.out)
