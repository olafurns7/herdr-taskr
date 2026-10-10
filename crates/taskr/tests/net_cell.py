#!/usr/bin/env python3
"""Real Go hub / Rust client contract cell; synthetic HOME, DB and tailnet only."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import golden
import argparse
from datetime import datetime, timezone
import fcntl
import hashlib
import http.client
import json
import os
import select
import shutil
import signal
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[3]
FAKE_TS = '''#!/bin/sh
case "$1" in
ip) printf '::1\n' ;;
status)
 name=host-a
 [ "$NET_ID" = hub ] && name=hub
 printf '{"MagicDNSSuffix":"example.ts.net","Self":{"ID":"%s-node","DNSName":"%s.example.ts.net.","UserID":1,"TailscaleIPs":[]},"User":{"1":{"LoginName":"owner@example.com"}}}\n' "$name" "$name" ;;
whois)
 name=host-a
 case "$3" in hub-*) name=hub ;; esac
 login=owner@example.com; tags='[]'
 case "$3:$NET_DENY_HUB" in hub-*:user) login=other@example.com ;; hub-*:tag) tags='["tag:ci"]' ;; hub-*:bad) tags='{}' ;; esac
 printf '{"Node":{"StableID":"%s-node","Name":"%s.example.ts.net.","Tags":%s},"UserProfile":{"LoginName":"%s"}}\n' "$name" "$name" "$tags" "$login" ;;
*) exit 2 ;;
esac
'''

class Cell:
    def __init__(self, go, rust, tmp, legacy=False, provenance="Go hub / Go client"):
        self.provenance = provenance
        self.go, self.rust, self.tmp = go, rust, Path(tmp).resolve()
        self.hub_home = self.tmp/'hub'
        self.client_home = self.tmp/'client'
        self.db = self.tmp/'hub.db'
        fake = self.tmp/'bin'
        fake.mkdir()
        (fake/'tailscale').write_text(FAKE_TS)
        (fake/'tailscale').chmod(0o755)
        self.env = {'PATH':f'{fake}:/usr/bin:/bin','LANG':'C.UTF-8','TZ':'UTC',
                    'TASKR_CONTRACT_TAILNET':'1','TASKR_CONTRACT_ORACLE':'1',
                    'HERDR_SOCKET_PATH':str(self.tmp/'absent.sock')}
        for home in (self.hub_home,self.client_home):
            (home/'.local/state/taskr').mkdir(parents=True)
        (self.hub_home/'.local/state/taskr/dashboard.addr').write_text('tailnet:0\n')
        self.hub_env = {**self.env,'NET_ID':'hub','HOME':str(self.hub_home),'TASKR_TMP_BASE':str(self.hub_home/'taskr-tmp'),'TASKR_DB':str(self.db)}
        self.client_env = {**self.env,'NET_ID':'host-a','HOME':str(self.client_home),'TASKR_TMP_BASE':str(self.client_home/'taskr-tmp')}
        self.log = open(self.tmp/'hub.log','wb')
        self.hub = subprocess.Popen([str(go),'daemon','--stay'],env=self.hub_env,stdout=self.log,stderr=self.log)
        try:
            deadline = time.monotonic()+15
            while time.monotonic()<deadline:
                if self.hub.poll() is not None:
                    raise AssertionError((self.tmp/'hub.log').read_text())
                try:
                    with sqlite3.connect(self.db) as db:
                        row=db.execute("select value from meta where key='dashboard_url'").fetchone()
                    if row:
                        port=row[0].split(':')[-1].strip('/')
                        url=f'http://[::1]:{port}'
                        with socket.create_connection(('::1',int(port)), timeout=.2):
                            self.url=url
                            if golden.session: golden.session.ports.add(int(port))
                            break
                except (sqlite3.Error,OSError):
                    pass
                select.select([],[],[],.02)
            else:
                raise AssertionError('Go hub did not bind synthetic tailnet')
            self.server_file=self.client_home/'.local/state/taskr/server.url'
            self.server_file.write_text(self.url+'\n')
            self.seq=0
            self.results=[]
            self.legacy_proxy = None
            if legacy and golden.session:
                argv = ['--json', '_host', 'observe', '--agents', '[]']
                def legacy_response():
                    client = http.client.HTTPConnection('::1', int(self.url.rsplit(':', 1)[1]), timeout=15)
                    try:
                        client.request('POST', '/api/rpc', json.dumps({'argv': argv, 'cwd': str(self.tmp), 'request_key': 'golden-legacy-probe'}), {'Content-Type': 'application/json', 'X-Taskr-RPC': '1'})
                        response = client.getresponse()
                        assert response.status == 200
                        envelope = json.loads(response.read())
                        assert envelope['exit'] == 0, envelope
                        return json.loads(envelope['stdout'])
                    finally:
                        client.close()
                response = legacy_response()
                if not golden.session.recording or golden.session.oracle_is_rust:
                    recorded = golden.session.expected[len(golden.session.observations)]['value']
                    self.legacy_proxy = golden.LegacyProxy(self.url, recorded.keys())
                    self.url = self.legacy_proxy.url
                    self.server_file.write_text(self.url+'\n')
                    response = legacy_response()
                golden.observe('legacy empty observe response', response)
        except BaseException:
            self.close()
            raise
    def close(self):
        if getattr(self, 'legacy_proxy', None):
            self.legacy_proxy.close()
        if self.hub.poll() is None:
            self.hub.terminate()
            try: self.hub.wait(timeout=10)
            except subprocess.TimeoutExpired: self.hub.kill(); self.hub.wait()
        self.log.close()
    def cli(self,binary,args,env=None):
        return subprocess.run([str(binary),*args],cwd=self.tmp,env={**self.client_env,**(env or {})},capture_output=True,timeout=75)
    def want(self,binary,args,code=0,env=None):
        p=self.cli(binary,args,env)
        assert p.returncode==code,(args,p.returncode,p.stdout,p.stderr)
        return p
    def obj(self,binary,args,code=0,env=None):
        p=self.want(binary,['--json',*args],code,env)
        return json.loads(p.stdout.splitlines()[-1])
    def compare(self,args,code=0,env=None):
        # Writes use explicit request keys so Go and Rust observe the same stored result.
        a=self.want(self.go,args,code,env)
        b=self.want(self.rust,args,code,env)
        assert (a.stdout,a.stderr)==(b.stdout,b.stderr),{'args':args,'go':[a.stdout.decode(),a.stderr.decode()],'rust':[b.stdout.decode(),b.stderr.decode()]}
        reference = b if golden.session and self.rust == golden.session.oracle else a
        observation = (args, (reference.returncode, reference.stdout, reference.stderr), self.provenance)
        if getattr(self, "defer_observations", False): self.pending_observations.append(observation)
        else: golden.observe(*observation)
        self.results.append({'args':args,'pass':True})
        return a
    def count(self,sql,params=()):
        with sqlite3.connect(self.db) as db:
            return db.execute(sql,params).fetchone()[0]
    def key(self):
        self.seq+=1
        return f'net-cell-{self.seq:04}'
    def record(self,args,code=0,env=None):
        return self.compare(['--json','--request-key',self.key(),*args],code,env)

class InterruptProxy:
    """Hold delivery at an observed boundary so the actual sender can be killed."""
    def __init__(self, upstream, before):
        self.port=int(upstream.rsplit(':',1)[1]);self.before=before
        self.hit=threading.Event();self.release=threading.Event();self.error=None
        self.listener=socket.socket(socket.AF_INET6)
        self.listener.bind(('::1',0));self.listener.listen();self.listener.settimeout(15)
        self.url='http://[::1]:'+str(self.listener.getsockname()[1])
        if golden.session: golden.session.ports.add(self.listener.getsockname()[1])
        self.thread=threading.Thread(target=self.run);self.thread.start()
    def run(self):
        try:
            client,_=self.listener.accept()
            with client:
                client.settimeout(15);data=b''
                while b'\r\n\r\n' not in data: data+=client.recv(65536)
                header,body=data.split(b'\r\n\r\n',1)
                length=int(next(x.split(b':',1)[1] for x in header.split(b'\r\n') if x.lower().startswith(b'content-length:')))
                while len(body)<length: body+=client.recv(length-len(body))
                if self.before:
                    self.hit.set();assert self.release.wait(15);return
                with socket.create_connection(('::1',self.port),timeout=10) as remote:
                    header=b'\r\n'.join(b'Host: [::1]:'+str(self.port).encode() if x.lower().startswith(b'host:') else x for x in header.split(b'\r\n'))
                    remote.sendall(header+b'\r\n\r\n'+body)
                    while remote.recv(65536): pass
                # A complete hub reply proves the DB commit preceded the kill.
                self.hit.set();assert self.release.wait(15)
        except Exception as e: self.error=repr(e);self.hit.set()
    def close(self):
        self.release.set();self.thread.join(timeout=16);self.listener.close()
        assert not self.thread.is_alive()
        assert self.error is None,self.error

class DropProxy:
    """Forward to the real hub; cut first reply after the command committed."""
    def __init__(self,upstream,before=False):
        self.port=int(upstream.rsplit(':',1)[1])
        self.before=before
        self.listener=socket.socket(socket.AF_INET6)
        self.listener.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
        self.listener.bind(('::1',0));self.listener.listen();self.listener.settimeout(.2)
        self.url='http://[::1]:'+str(self.listener.getsockname()[1])
        if golden.session: golden.session.ports.add(self.listener.getsockname()[1])
        self.keys=[];self.stop=False;self.error=None;self.first_done=threading.Event()
        self.thread=threading.Thread(target=self.run)
        self.thread.start()
    def run(self):
        try:
            while not self.stop:
                try: client,_=self.listener.accept()
                except socket.timeout: continue
                with client:
                    client.settimeout(10)
                    data=b''
                    while b'\r\n\r\n' not in data:
                        data+=client.recv(65536)
                    header,body=data.split(b'\r\n\r\n',1)
                    length=int(next(x.split(b':',1)[1] for x in header.split(b'\r\n') if x.lower().startswith(b'content-length:')))
                    while len(body)<length: body+=client.recv(length-len(body))
                    req=json.loads(body)
                    self.keys.append(req['request_key'])
                    if len(self.keys)==1 and self.before: continue
                    with socket.create_connection(('::1',self.port),timeout=10) as remote:
                        # Host header must name the actual hub listener.
                        header=b'\r\n'.join(b'Host: [::1]:'+str(self.port).encode() if x.lower().startswith(b'host:') else x for x in header.split(b'\r\n'))
                        remote.sendall(header+b'\r\n\r\n'+body)
                        reply=b''
                        while True:
                            chunk=remote.recv(65536)
                            if not chunk: break
                            reply+=chunk
                    if len(self.keys)==1: self.first_done.set();continue
                    client.sendall(reply)
        except Exception as e:
            self.error=repr(e)
    def close(self):
        self.stop=True;self.thread.join(timeout=12);self.listener.close()
        assert not self.thread.is_alive()
        assert self.error is None,self.error

def run(cell):
    root=json.loads(cell.record(['new','net-root','--role','orchestrator']).stdout)['task_id']
    cell.compare(['--json','status'])
    # Admission must fail before HTTP bytes for another user's or tagged hub.
    for refusal in ('user','tag','bad'):
        listener=socket.socket(socket.AF_INET6,socket.SOCK_STREAM)
        listener.bind(('::1',0));listener.listen();listener.settimeout(5)
        received=[]
        def probe():
            for _ in range(2):
                conn,_=listener.accept()
                with conn:
                    conn.settimeout(5);received.append(conn.recv(1))
        thread=threading.Thread(target=probe);thread.start()
        if golden.session: golden.session.ports.add(listener.getsockname()[1])
        cell.server_file.write_text(f'http://[::1]:{listener.getsockname()[1]}\n')
        try:
            cell.compare(['--json','--request-key',cell.key(),'status'],5,{'NET_DENY_HUB':refusal,'TASKR_CONTRACT_RETRY_MS':'0'})
            thread.join(timeout=10)
            assert not thread.is_alive() and received==[b'',b''],received
        finally:
            listener.close();cell.server_file.write_text(cell.url+'\n')
    cell.results.append({'hub_user_tag_and_malformed_refusal_before_http_bytes':True,'pass':True})
    # Same key + same argv = same bytes and only one note; other hash = exit 6.
    key=cell.key();argv=['--json','--request-key',key,'note','once <&> Þ😀','--as',str(root)]
    cell.compare(argv);cell.compare(argv)
    cell.compare(['--json','--request-key',key,'note','different','--as',str(root)],6)
    assert cell.count("select count(*) from events where kind='note' and summary='once <&> Þ😀'")==1
    lane=json.loads(cell.record(['new','net-worker','--role','implementer','--parent',str(root)]).stdout)['task_id']
    launch=json.loads(cell.record(['launch',str(lane),'--provider','codex','--model','fixture','--effort','high']).stdout)['launch_id']
    old={'TASKR_TASK':str(lane),'TASKR_LAUNCH':str(launch)}
    cell.record(['note','fresh'],env=old)
    cell.record(['launch',str(lane),'--provider','codex','--model','fixture','--effort','high'])
    cell.record(['note','stale'],6,old)
    cell.record(['new', 'empty-cwd', '--role', 'orchestrator', '--cwd', ''])
    # Document upload originates on the client, including an empty text body.
    report=cell.tmp/'client.md';report.write_text('report <&> Þ😀\n')
    current=cell.count('select current_launch_id from tasks where id=?',(lane,))
    worker={'TASKR_TASK':str(lane),'TASKR_LAUNCH':str(current)}
    cell.record(['ready','report uploaded','--report',str(report)],env=worker)
    assert cell.count("select count(*) from documents where task_id=? and kind='report' and captured=1",(lane,))==1
    goal=cell.tmp/'goal.md';goal.write_bytes(b'')
    cell.record(['doc','set',str(root),'goal','--file',str(goal)])
    assert cell.count("select bytes from documents where task_id=? and kind='goal' order by version desc limit 1",(root,))==0
    # Both clients recover a reply lost before or after the hub commit, with one key.
    # Raw stdin hooks run on the client; both clients queue silently and can be
    # resumed by the other language without opening a local ledger.
    payload=json.dumps({'session_id':'net-hook','transcript_path':str(cell.tmp/'synthetic.jsonl')}).encode()
    with sqlite3.connect(cell.db) as db:
        db.execute("update launches set pane_id='net:p1' where id=?",(current,))
    hook_env={**cell.client_env,**worker,'HERDR_ENV':'1','HERDR_PANE_ID':'net:p1'}
    for queue_bin,send_bin in ((cell.go,cell.rust),(cell.rust,cell.go)):
        cell.server_file.write_text('http://[::1]:1\n')
        p=subprocess.run([str(queue_bin),'hook','codex','SessionStart'],cwd=cell.tmp,env=hook_env,input=payload,capture_output=True,timeout=2)
        assert (p.returncode,p.stdout,p.stderr)==(0,b'',b''),(p.returncode,p.stdout,p.stderr)
        cell.server_file.write_text(cell.url+'\n')
        assert cell.obj(send_bin,['spool','send'])['sent']==1
        assert cell.count('select session_ref from launches where id=?',(current,))=='net-hook'
    # A hook waits briefly for a held queue lock, then queues in order when it
    # releases. With an empty queue it sends directly after the busy timeout.
    queue_dir=cell.client_home/'.local/state/taskr/spool/queue'
    lock_file=open(cell.client_home/'.local/state/taskr/spool/lock','r+b')
    try:
        cell.server_file.write_text('http://[::1]:1\n')
        cell.want(cell.rust,['--request-key',cell.key(),'note','before hook','--as',str(root)])
        cell.server_file.write_text(cell.url+'\n')
        for queued in (True,False):
            if sys.platform == 'linux': fcntl.flock(lock_file,fcntl.LOCK_EX)
            elif queued: cell.server_file.write_text('http://[::1]:1\n')
            p=subprocess.Popen([str(cell.rust),'hook','codex','SessionStart'],cwd=cell.tmp,env=hook_env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            p.stdin.write(payload);p.stdin.close();p.stdin=None
            if sys.platform == 'linux' and queued:
                # The hook worker sleeps only after observing the held queue lock.
                from daemon_cell import eventually
                eventually(lambda: any(path.read_text().strip() == 'hrtimer_nanosleep'
                                      for path in Path(f'/proc/{p.pid}/task').glob('*/wchan')), timeout=2)
            elif sys.platform == 'linux':
                p.wait(timeout=2)
            fcntl.flock(lock_file,fcntl.LOCK_UN)
            out,err=p.communicate(timeout=2)
            assert (p.returncode,out,err)==(0,b'',b''),(p.returncode,out,err)
            cell.server_file.write_text(cell.url+'\n')
            assert len(list(queue_dir.glob('*.json')))==(2 if queued else 0),(queued,list(queue_dir.glob('*.json')))
            if queued: assert cell.obj(cell.rust,['spool','send'])['sent']==2
    finally:
        fcntl.flock(lock_file,fcntl.LOCK_UN);lock_file.close()
    if sys.platform != 'linux': print('SKIP hook queue-lock readiness: Linux /proc proof; queued/direct delivery asserted')
    cell.results.append({'raw_hook_cross_spool':True,'queue_lock_and_direct_fallback':sys.platform == 'linux','queued_and_direct_delivery':True,'pass':True})
    # Fresh reads under a reused key must see new ledger state, not a cached reply.
    fresh=['--json','--request-key','net-fresh-read-01','log',str(root)]
    first=cell.want(cell.rust,fresh).stdout
    cell.record(['note','fresh read changed','--as',str(root)])
    second=cell.want(cell.rust,fresh).stdout
    assert first!=second and b'fresh read changed' in second
    assert cell.count("select count(*) from requests where key='net-fresh-read-01'")==0
    # A stored running key retries until the hub returns its completed response.
    argv=['--json','new','running-root','--role','orchestrator']
    key=cell.key();digest=hashlib.sha256(json.dumps(argv,separators=(',',':')).encode()).hexdigest()
    with sqlite3.connect(cell.db) as db:
        db.execute("insert into requests(key,machine,argv_sha,state,created_at) values(?,'host-a',?,'running',strftime('%Y-%m-%dT%H:%M:%fZ','now'))",(key,digest))
    caller=subprocess.Popen([str(cell.rust),'--request-key',key,*argv],cwd=cell.tmp,env=cell.client_env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    try:
        ready,_,_=select.select([caller.stderr],[],[],10)
        assert ready
        announce=caller.stderr.readline()
        assert b'request still running; retrying until' in announce,announce
        with sqlite3.connect(cell.db) as db:
            db.execute("update requests set state='done',exit=0,stdout='completed once\n',stderr='',upload='null' where key=?",(key,))
        out,err=caller.communicate(timeout=15)
        assert caller.returncode==0 and out==b'completed once\n' and err==b'',(caller.returncode,out,err)
    finally:
        if caller.poll() is None: caller.kill();caller.communicate(timeout=5)
    cell.results.append({'running_key_retry':True,'fresh_read_key':True,'pass':True})
    # SIGINT/SIGTERM cancel a caller in flight without queuing a second write.
    for sig in (signal.SIGINT,signal.SIGTERM):
        proxy=InterruptProxy(cell.url,True)
        cell.server_file.write_text(proxy.url+'\n')
        caller=subprocess.Popen([str(cell.rust),'--request-key',cell.key(),'note','signal abort','--as',str(root)],cwd=cell.tmp,env=cell.client_env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        try:
            assert proxy.hit.wait(10) and proxy.error is None
            began=time.monotonic();caller.send_signal(sig);out,err=caller.communicate(timeout=2)
            assert caller.returncode==5 and time.monotonic()-began<1,(caller.returncode,out,err)
            assert b'retry with:' in err and b'queued' not in out,(out,err)
            assert cell.count("select count(*) from events where kind='note' and summary='signal abort'")==0
        finally:
            if caller.poll() is None: caller.kill();caller.communicate(timeout=5)
            proxy.close();cell.server_file.write_text(cell.url+'\n')
        cell.results.append({'signal':sig.name,'phase':'in flight','pass':True})
    proxy=DropProxy(cell.url,False)
    cell.server_file.write_text(proxy.url+'\n')
    text='signal in backoff';key=cell.key()
    caller=subprocess.Popen([str(cell.rust),'--request-key',key,'note',text,'--as',str(root)],cwd=cell.tmp,env=cell.client_env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    try:
        assert proxy.first_done.wait(10) and proxy.error is None
        began=time.monotonic();caller.send_signal(signal.SIGTERM);out,err=caller.communicate(timeout=2)
        assert caller.returncode==5 and time.monotonic()-began<1 and b'queued' not in out,(caller.returncode,out,err)
        assert cell.count("select count(*) from events where kind='note' and summary=?",(text,))==1
    finally:
        if caller.poll() is None: caller.kill();caller.communicate(timeout=5)
        proxy.close();cell.server_file.write_text(cell.url+'\n')
    cell.results.append({'signal':'SIGTERM','phase':'backoff after commit','pass':True})
    for binary in (cell.go,cell.rust):
        for before in (True,False):
            proxy=DropProxy(cell.url,before)
            try:
                cell.server_file.write_text(proxy.url+'\n')
                text=f'lost-{binary.name}-{before}-{cell.key()}'
                p=cell.want(binary,['--json','--request-key',cell.key(),'note',text,'--as',str(root)])
                assert json.loads(p.stdout)['event_id']>0
                assert len(proxy.keys)==2 and proxy.keys[0]==proxy.keys[1],proxy.keys
                assert cell.count("select count(*) from events where kind='note' and summary=?",(text,))==1
            finally:
                cell.server_file.write_text(cell.url+'\n');proxy.close()
    # Go queues -> Rust sends; Rust queues -> Go sends. A replay of a committed
    # queue head models interruption after commit and before local deletion.
    for index,(queue_bin,send_bin) in enumerate(((cell.go,cell.rust),(cell.rust,cell.go))):
        for committed in (False,True):
            key=cell.key();text=f'spool-{index}-{committed}-{key}'
            argv=['--json','--request-key',key,'note',text,'--as',str(root)]
            if committed: cell.want(queue_bin,argv)
            cell.server_file.write_text('http://[::1]:1\n')
            try:
                p=cell.want(queue_bin,argv)
                assert json.loads(p.stdout)=={'queued':True,'request_key':key}
            finally: cell.server_file.write_text(cell.url+'\n')
            if index == 0 and not committed:
                queued_path, = queue_dir.glob('*.json')
                golden.observe('Go queued spool record', json.loads(queued_path.read_text()))
                if golden.session and not golden.session.recording:
                    frozen = golden.replay('Go queued spool record', {'<tmp>': golden.TEMP_PATH.match(str(cell.tmp))[0], '<time>': datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')})
                    queued_path.write_text(json.dumps(frozen)+'\n')
            p=cell.want(send_bin,['--json','spool','send'])
            assert json.loads(p.stdout)=={'ok':True,'queued':0,'refused':0,'sent':1},p.stdout
            assert cell.count("select count(*) from events where kind='note' and summary=?",(text,))==1
    for queue_bin,send_bin in ((cell.go,cell.rust),(cell.rust,cell.go)):
        for before in (True,False):
            key=cell.key();text=f'kill-spool-{queue_bin.name}-{before}-{key}'
            cell.server_file.write_text('http://[::1]:1\n')
            p=cell.want(queue_bin,['--json','--request-key',key,'note',text,'--as',str(root)])
            assert json.loads(p.stdout)['queued'] is True
            proxy=InterruptProxy(cell.url,before)
            cell.server_file.write_text(proxy.url+'\n')
            sender=subprocess.Popen([str(send_bin),'--json','spool','send'],cwd=cell.tmp,env=cell.client_env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            try:
                assert proxy.hit.wait(15), 'sender did not reach interruption boundary'
                assert proxy.error is None,proxy.error
                assert sender.poll() is None
                sender.kill();sender.communicate(timeout=5)
                assert sender.returncode==-signal.SIGKILL
                assert len(list((cell.client_home/'.local/state/taskr/spool/queue').glob('*.json')))==1
                assert cell.count("select count(*) from events where kind='note' and summary=?",(text,))==(0 if before else 1)
            finally:
                if sender.poll() is None: sender.kill();sender.communicate(timeout=5)
                proxy.close();cell.server_file.write_text(cell.url+'\n')
            rep=cell.obj(send_bin,['spool','send'])
            assert rep=={'ok':True,'queued':0,'refused':0,'sent':1},rep
            assert cell.count("select count(*) from events where kind='note' and summary=?",(text,))==1
            cell.results.append({'interrupted_sender':send_bin.name,'phase':'before commit' if before else 'after commit','pass':True})
    cell.compare(['--json','spool','ls'])
    assert not (cell.client_home/'.local/state/taskr/taskr.db').exists()
    return cell.results

def isolated_uploads(go, rust):
    """Each uploader starts with a fresh Go hub, without a prior same-key write."""
    outcomes = []
    for client in (go, rust):
        with tempfile.TemporaryDirectory(prefix='taskr-doc-cell-') as tmp:
            cell = Cell(go, rust, tmp)
            result = {}
            try:
                def obj(args, env=None):
                    return cell.obj(client, args, env=env)
                root = obj(['new', 'root', '--role', 'orchestrator', '--cwd', str(cell.tmp)])['task_id']
                files = {'text': b'report <&> \xc3\x9e\xf0\x9f\x98\x80\n', 'empty': b'',
                         'binary': b'a\x00b', 'large': b'x' * ((1 << 20) + 5), 'badutf8': b'\xff\xfe ok',
                         'missing': None, 'dir': None}
                for name, body in files.items():
                    lane = obj(['new', f'lane-{name}', '--role', 'implementer', '--parent', str(root), '--cwd', str(cell.tmp)])['task_id']
                    launch = obj(['launch', str(lane), '--provider', 'codex', '--model', 'fixture', '--effort', 'high'])['launch_id']
                    path = cell.tmp / f'{name}.md'
                    if name == 'dir': path.mkdir()
                    elif body is not None: path.write_bytes(body)
                    worker = {'TASKR_TASK': str(lane), 'TASKR_LAUNCH': str(launch)}
                    for cmd in ('ready', 'done'):
                        args = [cmd, f'{cmd} {name}']
                        if cmd == 'ready': args += ['--report', str(path)]
                        reply = cell.cli(client, ['--json', *args], worker)
                        result[f'{cmd}-{name}'] = (reply.returncode, reply.stderr.decode().replace(str(cell.tmp), '<tmp>'))
                plan = cell.tmp / 'plan.md'; plan.write_text('plan body\n')
                result['plan'] = cell.want(client, ['doc', 'set', str(root), 'plan', '--name', 'p1', '--file', str(plan)]).returncode
                result['backfill'] = cell.want(client, ['doc', 'backfill', '--dry-run']).stdout.decode().replace(str(cell.tmp), '<tmp>')
                with sqlite3.connect(cell.db) as db:
                    result['documents'] = db.execute(
                        "select t.name,d.kind,coalesce(d.name,''),d.version,d.captured,d.bytes,d.sha256,coalesce(d.reason,''),coalesce(d.source_host,'') "
                        "from documents d join tasks t on t.id=d.task_id order by t.name,d.kind,d.version").fetchall()
                    for sha, size, body in db.execute('select sha256,bytes,body from doc_blobs'):
                        assert hashlib.sha256(body.encode()).hexdigest() == sha
                        assert len(body.encode()) == size
                assert any(row[4] and row[5] == 0 for row in result['documents']), result
                outcomes.append(result)
            finally:
                cell.close()
    golden.observe('isolated document uploads', outcomes[0])
    assert outcomes[0] == outcomes[1], {'Go uploader': outcomes[0], 'Rust uploader': outcomes[1]}
    return {'args': ['isolated document upload: ready/done, text/empty/binary/UTF8/size/missing/directory, doc set/backfill'], 'pass': True}


def malformed_spool_heads(cell):
    for index, binary in enumerate((cell.go, cell.rust)):
        home = cell.tmp / f'badargv-{index}'
        state = home / '.local/state/taskr'
        queue = state / 'spool/queue'; queue.mkdir(parents=True)
        (state / 'server.url').write_text('http://[::1]:1\n')
        for seq, argv in ((1, ['note', 7]), (2, ['note', 'valid'])):
            key = f'badargv-probe-{seq}'
            record = {'v': 1, 'seq': seq, 'request_key': key, 'queued_at': '2026-10-08T00:00:00Z',
                      'request': {'argv': argv, 'cwd': str(cell.tmp), 'env': {}, 'request_key': key}}
            (queue / f'{seq:06}-{key}.json').write_text(json.dumps(record))
        p = cell.want(binary, ['--json', 'spool', 'send'], env={'HOME': str(home)})
        assert json.loads(p.stdout) == {'ok': True, 'sent': 0, 'queued': 1, 'refused': 0}, p.stdout
        assert len(list(queue.glob('*.json'))) == 1
        assert len(list((state / 'spool/bad').glob('*.json'))) == 1
        (state / 'server.url').write_text('https://100.64.0.1:9\n')
        failure = cell.want(binary, ['--json', 'spool', 'send'], code=2, env={'HOME': str(home)})
        body = json.loads(failure.stdout)
        assert {k: body[k] for k in ('ok', 'sent', 'queued', 'refused')} == {'ok': False, 'sent': 0, 'queued': 1, 'refused': 0}
        assert body['kind'] == 'usage' and body['error'], body
        (state / 'server.url').write_text('http://[::1]:1\n')
        started = time.time()
        caller = subprocess.Popen([str(binary), 'wait', '--timeout', '300000000000000', '--as', '1'],
                                  env={**cell.client_env, 'HOME': str(home)}, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            ready, _, _ = select.select([caller.stderr], [], [], 10)
            assert ready, 'huge wait did not announce retry'
            line = caller.stderr.readline().decode()
            assert 'retrying until ' in line, line
            end = datetime.fromisoformat(line.split('until ', 1)[1].strip().replace('Z', '+00:00')).timestamp()
            wrapped_seconds = (300000000000000 * 1000000 % (1 << 64)) / 1000000000
            assert abs(end - started - wrapped_seconds) < 10, line
            assert caller.poll() is None, 'huge wait exited instead of retrying'
        finally:
            caller.terminate()
            caller.communicate(timeout=15)
    cell.results.append({'args': ['malformed spool head quarantined; next valid record survives'], 'pass': True})


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--go',type=Path)
    ap.add_argument('--rust',type=Path,default=ROOT/'target/release/taskr')
    ap.add_argument('--out',type=Path)
    args=golden.parse(ap, __file__)
    with tempfile.TemporaryDirectory(prefix='taskr-net-cell-') as tmp:
        go=args.go
        if go is None:
            go=Path(tmp).resolve()/'taskr-go'
            subprocess.run(['go','build','-tags','taskr_contract','-o',str(go),'.'],cwd=ROOT,check=True)
        cell=Cell(go.resolve(),args.rust.resolve(),tmp)
        try:
            results=run(cell)
            malformed_spool_heads(cell)
            results.append(isolated_uploads(go.resolve(), args.rust.resolve()))
            result={'cell':'Rust-client/Go-hub','pass':len(results),'mismatch':0,'checks':results}
            if args.out: args.out.write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
            print(json.dumps(result,ensure_ascii=False))
        finally: cell.close()

if __name__=='__main__':
    main()
    golden.finish()
