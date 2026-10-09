#!/usr/bin/env python3
"""Synthetic Go/Go, Rust/Go, Go/Rust, Rust/Rust hub matrix; no live services."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import golden
import argparse
import base64
import hashlib
import http.client
import json
import os
import select
import socket
import sqlite3
import subprocess
import tempfile
import time
from net_cell import Cell, DropProxy, FAKE_TS, run as net_checks

ROOT=Path(__file__).resolve().parents[3]

class RustHub(Cell):
    def __init__(self,go,rust,tmp):
        self.provenance="Rust hub / Go client"
        self.go,self.rust,self.tmp=go,rust,Path(tmp).resolve()
        self.hub_home=self.tmp/'hub';self.client_home=self.tmp/'client';self.db=self.tmp/'hub.db'
        fake=self.tmp/'bin';fake.mkdir();(fake/'tailscale').write_text(FAKE_TS);(fake/'tailscale').chmod(0o755)
        self.env={'PATH':f'{fake}:/usr/bin:/bin','LANG':'C.UTF-8','TZ':'UTC','TASKR_CONTRACT_TAILNET':'1','TASKR_CONTRACT_ORACLE':'1','HERDR_SOCKET_PATH':str(self.tmp/'absent.sock')}
        for home in (self.hub_home,self.client_home):(home/'.local/state/taskr').mkdir(parents=True)
        self.client_env={**self.env,'NET_ID':'host-a','HOME':str(self.client_home)}
        self.hub_env={**self.env,'NET_ID':'hub','HOME':str(self.hub_home),'TASKR_DB':str(self.db)}
        self.log=open(self.tmp/'hub.log','wb')
        self.hub=subprocess.Popen([str(rust),'--contract-hub'],env=self.hub_env,stdout=subprocess.PIPE,stderr=self.log)
        readable,_,_=select.select([self.hub.stdout],[],[],15)
        assert readable,'Rust hub fixture did not announce its listener'
        self.url=self.hub.stdout.readline().decode().strip()
        assert self.url.startswith('http://[::1]:'),(self.url,(self.tmp/'hub.log').read_text())
        if golden.session: golden.session.ports.add(int(self.url.rsplit(':',1)[1]))
        self.server_file=self.client_home/'.local/state/taskr/server.url';self.server_file.write_text(self.url+'\n')
        self.seq=0;self.results=[]
    def close(self):
        super().close()
        self.hub.stdout.close()


def checks(cell):
    root=json.loads(cell.record(['new','hub-root','--role','orchestrator']).stdout)['task_id']
    assert cell.count('select machine from tasks where id=?',(root,))=='host-a'
    # Both executable clients read caller/server identity and fresh campaign data.
    for binary in (cell.go,cell.rust):
        glance=cell.obj(binary,['glance'])
        assert glance['caller_host']=='host-a' and glance['server_host'],glance
        key=cell.key()
        args=['--json','--request-key',key,'campaign',str(root)]
        before=json.loads(cell.want(binary,args).stdout)
        assert before['root']['host']=='host-a' and len(before['spark'])==24,before
        next_text='campaign fresh '+key
        cell.record(['next',str(root),next_text])
        after=json.loads(cell.want(binary,args).stdout)
        assert after['root']['next']==next_text,after
        assert cell.count('select count(*) from requests where key=?',(key,))==0
    log_file=cell.hub_home/'.local/state/taskr/daemon.log'
    logged=log_file.read_text() if log_file.exists() else ''
    assert not any('cmd='+name+' ' in line and 'exit=0' in line for line in logged.splitlines() for name in ('glance','campaign')),logged
    cell.results.append({'glance_hosts_campaign_fresh_and_quiet_success_both_clients':True,'pass':True})

    for args in (['status'],['log',str(root)],['notes','--root',str(root)],['version']):cell.compare(['--json',*args])
    key=cell.key();args=['--json','--request-key',key,'note','once <&> Þ😀','--as',str(root)]
    cell.compare(args);cell.compare(args)
    cell.compare(['--json','--request-key',key,'note','other hash','--as',str(root)],6)
    assert cell.count("select count(*) from events where kind='note' and summary='once <&> Þ😀'")==1
    lane=json.loads(cell.record(['new','hub-worker','--role','implementer','--parent',str(root),'--pane','wTEST:p1']).stdout)['task_id']
    launch=json.loads(cell.record(['launch',str(lane),'--provider','codex','--model','fixture','--effort','high']).stdout)['launch_id']
    worker={'TASKR_TASK':str(lane),'TASKR_LAUNCH':str(launch),'HERDR_PANE_ID':'wTEST:p1'}
    cell.record(['note','worker identity'],env=worker)
    old=worker.copy()
    launch=json.loads(cell.record(['launch',str(lane),'--provider','codex','--model','fixture','--effort','high']).stdout)['launch_id']
    worker['TASKR_LAUNCH']=str(launch)
    cell.record(['note','stale launch'],6,old)
    report=cell.tmp/'report.md';report.write_text('report <&> Þ😀\n')
    event=json.loads(cell.record(['ready','uploaded report','--report',str(report)],env=worker).stdout)['event_id']
    assert cell.count("select count(*) from documents where task_id=? and kind='report' and captured=1",(lane,))==1
    for binary in (cell.go,cell.rust):
        reply=cell.obj(binary,['wait','--as',str(root),'--for','ready','--timeout','0'])
        assert reply['event']['id']==event,reply
    replay=cell.obj(cell.rust,['wait','--as',str(root),'--for','ready','--timeout','0'])
    assert replay['replay'] is True
    cell.record(['ack',str(event),'--as',str(root)])
    cell.compare(['--json','wait','--as',str(root),'--timeout','0'],3)
    goal=cell.tmp/'empty.md';goal.write_bytes(b'')
    cell.record(['doc','set',str(root),'goal','--file',str(goal)])
    assert cell.count("select bytes from documents where task_id=? and kind='goal' order by version desc limit 1",(root,))==0
    cell.compare(['doc','get',str(cell.count("select id from documents where task_id=? and kind='goal' order by version desc limit 1",(root,)))])
    for binary in (cell.go,cell.rust):
        for before in (True,False):
            proxy=DropProxy(cell.url,before)
            try:
                cell.server_file.write_text(proxy.url+'\n');text=f'lost-reply-{before}-{cell.key()}'
                cell.want(binary,['--json','--request-key',cell.key(),'note',text,'--as',str(root)])
                assert len(proxy.keys)==2 and proxy.keys[0]==proxy.keys[1],proxy.keys
                assert cell.count("select count(*) from events where kind='note' and summary=?",(text,))==1
            finally:cell.server_file.write_text(cell.url+'\n');proxy.close()
    cell.results.append({'lost_reply_before_and_after_commit_both_clients':True,'pass':True})
    # Admission is exercised through the actual listener, never a forwarded peer header.
    port=int(cell.url.rsplit(':',1)[1])
    def post(body,headers=None):
        c=http.client.HTTPConnection('::1',port,timeout=10)
        c.request('POST','/api/rpc',body=body,headers={'Content-Type':'application/json','X-Taskr-RPC':'1',**(headers or {})})
        r=c.getresponse();out=(r.status,r.read());c.close();return out
    body=json.dumps({'argv':['status'],'cwd':str(cell.tmp),'env':None,'request_key':'hub-admit-01'})
    for headers,code in (({},200),({'Origin':'http://evil.example'},403),({'Sec-Fetch-Site':'same-origin'},403),({'Host':'evil.example'},421),({'Content-Type':'text/plain'},415),({'X-Taskr-RPC':''},400)):
        result=post(body,headers);assert result[0]==code,(headers,result)
    for invalid in ('{}',body+' {}',body[:-1]+',"machine":"host-b"}'):
        assert post(invalid)[0]==400
    assert post(json.dumps({'argv':['note','x'*(2<<20)],'cwd':'/','request_key':'hub-too-big'}))[0]==413
    cell.results.append({'admission_host_origin_body_cap':True,'pass':True})
    # A tagged peer must fail before executing or opening a local ledger.
    for binary in (cell.go,cell.rust):
        cell.want(binary,['--json','status'],5,{'NET_DENY_HUB':'tag','TASKR_CONTRACT_RETRY_MS':'0'})
    cell.results.append({'bad_peer_client_verification':True,'pass':True})
    if isinstance(cell,RustHub):
        forged={'argv':['--contract-env'],'cwd':str(cell.tmp),'request_key':'hub-env-override','env':{'PATH':'/caller/evil','HERDR_SOCKET_PATH':'/caller/evil.sock','HOME':'/caller/home','TASKR_DB':'/caller/evil.db','TASKR_RPC_CALLER':'forged'}}
        code,raw=post(json.dumps(forged));assert code==200,(code,raw)
        observed=json.loads(json.loads(raw)['stdout'])
        assert observed=={'PATH':cell.hub_env['PATH'],'HERDR_SOCKET_PATH':cell.hub_env['HERDR_SOCKET_PATH'],'HOME':str(cell.hub_home),'TASKR_DB':str(cell.db),'TASKR_RPC_CALLER':'host-a'},observed
        cell.results.append({'caller_server_env_override_ignored':True,'pass':True})
        # Disconnect a fresh wait only after its marker has reached the ledger.
        payload=json.dumps({'argv':['--json','wait','--as',str(root),'--for','done','--timeout','20000'],'cwd':str(cell.tmp),'request_key':'hub-cancel-wait','env':{}}).encode()
        connection=socket.create_connection(('::1',port),timeout=5)
        connection.sendall(b'POST /api/rpc HTTP/1.1\r\nHost: [::1]:'+str(port).encode()+b'\r\nContent-Type: application/json\r\nX-Taskr-RPC: 1\r\nContent-Length: '+str(len(payload)).encode()+b'\r\n\r\n'+payload)
        deadline=time.monotonic()+5
        while cell.count('select waiting_until is not null from tasks where id=?',(root,))!=1:
            assert time.monotonic()<deadline,'wait marker never appeared';select.select([],[],[],.02)
        connection.close();deadline=time.monotonic()+3
        while cell.count('select waiting_until is not null from tasks where id=?',(root,))!=0:
            assert time.monotonic()<deadline,'disconnected wait did not clear marker';select.select([],[],[],.02)
        cell.results.append({'fresh_wait_disconnect_clears_marker':True,'pass':True})
    assert not (cell.client_home/'.local/state/taskr/taskr.db').exists()
    return cell.results


def cross_hub(go,rust,tmp):
    cells=[]
    for name,cls in [('oracle',Cell),('candidate',RustHub)]:
        place=Path(tmp).resolve()/name;place.mkdir();cells.append(cls(go,rust,place))
    sequence=[]
    def pair(key,argv,env=None,document=None,caps=None):
        body=json.dumps({'argv':argv,'cwd':'/synthetic/caller','env':env or {},'request_key':key,'capabilities':caps or [],'document':document})
        outputs=[]
        for cell in cells:
            port=int(cell.url.rsplit(':',1)[1]);c=http.client.HTTPConnection('::1',port,timeout=10)
            c.request('POST','/api/rpc',body,{'Content-Type':'application/json','X-Taskr-RPC':'1'})
            response=c.getresponse();outputs.append((response.status,response.read()));c.close()
        if '_host' in argv and 'observe' in argv:
            for index, (code, raw) in enumerate(outputs):
                envelope = json.loads(raw)
                value = json.loads(envelope['stdout'])
                extension = value.pop('hostd', None)
                if extension is not None:
                    assert extension['version'] == 1 and extension['epoch'] and extension['stale_ms'] == 30000, extension
                    envelope['stdout'] = json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(',', ':')) + '\n'
                    outputs[index] = (code, (json.dumps(envelope, ensure_ascii=False, separators=(',', ':'))+'\n').encode())
        assert outputs[0]==outputs[1],{'argv':argv,'Go':repr(outputs[0]),'Rust':repr(outputs[1])}
        golden.observe(argv, outputs[0])
        sequence.append({'argv':argv,'pass':True});return json.loads(outputs[0][1])
    try:
        pair('cross-new-root',['--json','new','parity-root','--role','orchestrator'])
        pair('cross-new-lane',['--json','new','parity-lane','--role','implementer','--parent','1','--pane','wTEST:p1'])
        pair('cross-launch-01',['--json','launch','2','--provider','codex','--model','fixture','--effort','high'])
        prompt=pair('cross-prompt-begin',['--json','_prompt','begin','2','--text','synthetic prompt <&> Þ😀','--local-herdr','--receipt-timeout','0'])
        attempt=json.loads(prompt['stdout'])['attempt_id']
        pair('cross-prompt-outcome',['--json','_prompt','outcome',str(attempt),'--outcome','activity_observed','--detail','{"agent_status":"working"}'])
        pair('cross-prompt-repeat',['--json','_prompt','outcome',str(attempt),'--outcome','activity_observed'])
        pair('cross-host-observe',['--json','_host','observe','--agents','[{"pane_id":"wTEST:p1","agent_status":"working"}]'])
        argv=['--json','note','same <&> Þ😀','--as','1'];pair('cross-note-once',argv);pair('cross-note-once',argv)
        pair('cross-note-once',['--json','note','different','--as','1'])
        reply=pair('cross-ready-01',['--json','ready','uploaded','--report','/synthetic/report.md'],{'TASKR_TASK':'2','TASKR_LAUNCH':'1'},caps=['doc-upload'])
        event=json.loads(reply['stdout'])['event_id'];body='report <&> Þ😀\n'.encode()
        payload={'task':2,'kind':'report','name':'','path':'/synthetic/report.md','event_id':event,'body':base64.b64encode(body).decode(),'sha256':hashlib.sha256(body).hexdigest(),'bytes':len(body)}
        pair('cross-upload-01',['--json','_doc','put'],document=payload,caps=['doc-upload'])
        pair('cross-doc-wanted',['--json','_doc','wanted','--tree','1'],caps=['doc-upload'])
        # Freeze only synthetic stored rows for event-read bytes; network timers keep real time.
        for cell in cells:
            with sqlite3.connect(cell.db) as db:
                db.execute("update events set created_at='2026-10-08T00:00:00.000Z'")
        pair('cross-wait-01',['--json','wait','--as','1','--for','ready','--timeout','0'])
        pair('cross-wait-01',['--json','wait','--as','1','--for','ready','--timeout','0'])
        pair('cross-wait-ack',['--json','wait','--as','1','--ack',str(event),'--timeout','0'])
        pair('cross-wait-ack',['--json','wait','--as','1','--ack',str(event),'--timeout','0'])
        pair('cross-doc-get',['doc','get','2'])
        return {'cross_hub_byte_pairs':len(sequence),'mismatch':0,'checks':sequence}
    finally:
        for cell in cells:cell.close()

def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--go',type=Path);ap.add_argument('--rust',type=Path,default=ROOT/'target/debug/taskr')
    ap.add_argument('--net-fixture',action='store_true');ap.add_argument('--out',type=Path);ap.add_argument('--rust-hub-only',action='store_true');args=golden.parse(ap, __file__)
    with tempfile.TemporaryDirectory(prefix='taskr-hub-matrix-') as tmp:
        go=args.go
        if go is None:
            go=Path(tmp).resolve()/'taskr-go';subprocess.run(['go','build','-tags','taskr_contract','-o',str(go),'.'],cwd=ROOT,check=True)
        results=[]
        for kind,cls in ([('Rust',RustHub)] if args.rust_hub_only else [('Go',Cell),('Rust',RustHub)]):
            place=Path(tmp).resolve()/kind;place.mkdir();cell=cls(go.resolve(),args.rust.resolve(),place)
            try:
                checks(cell);results.append({'hub':kind,'clients':['Go','Rust'],'pass':len(cell.results),'mismatch':0,'checks':cell.results})
            finally:cell.close()
            if args.net_fixture:
                place=Path(tmp).resolve()/(kind+'-net');place.mkdir();cell=cls(go.resolve(),args.rust.resolve(),place)
                try:
                    net_checks(cell);results.append({'hub':kind,'fixture':'impl-rust-net','clients':['Go','Rust'],'pass':len(cell.results),'mismatch':0,'checks':cell.results})
                finally:cell.close()
        cross=cross_hub(go.resolve(),args.rust.resolve(),tmp) if not args.rust_hub_only else None
        result={'cells':results,'cross_hub':cross,'mismatch':0}
        if args.out:args.out.write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
        print(json.dumps(result,ensure_ascii=False))
if __name__=='__main__':
    main()
    golden.finish()
