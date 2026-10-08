#!/usr/bin/env python3
"""Go hub + fake Herdr, Go/Rust client daemon observation parity."""
import argparse
import json
import sqlite3
import tempfile
from pathlib import Path
from daemon_cell import Herdr, eventually, start, stop
from net_cell import Cell


def run(go,rust,out):
    results=[]
    with tempfile.TemporaryDirectory(prefix='taskr-relay-') as tmp:
        cell=Cell(go,rust,tmp)
        fake=Herdr(cell.client_home)
        cell.client_env.update(HERDR_SOCKET_PATH=str(fake.path),PATH=str(cell.client_home/'bin')+':'+cell.env['PATH'])
        try:
            top=cell.obj(rust,['new','campaign','--role','orchestrator','--pane','wRoot:p0','--workspace','wRoot'])['task_id']
            worker=cell.obj(rust,['new','lane','--role','implementer','--parent',str(top),'--pane','wLane:p1','--workspace','wLane'])['task_id']
            launch=cell.obj(rust,['launch',str(worker),'--provider','fixture','--model','fixture','--effort','medium'])['launch_id']
            (cell.client_home/'agents.json').write_text(json.dumps([{'name':'lane','pane_id':'wLane:p1','agent_status':'blocked','state_change_seq':1},{'name':'campaign','pane_id':'wRoot:p0','agent_status':'working','state_change_seq':1}]))
            first=cell.want(go,['--json','daemon','--once'])
            second=cell.want(rust,['--json','daemon','--once'])
            assert (first.stdout,first.stderr)==(second.stdout,second.stderr),(first.stdout,second.stdout,first.stderr,second.stderr)
            assert json.loads(first.stdout)['watch']==['wLane:p1']
            assert cell.count("select count(*) from events where kind='herdr' and task_id=?",(worker,))==1
            assert cell.count("select count(*) from meta where key='daemon_heartbeat:host-a'")==1
            assert cell.count("select count(*) from tasks where id=? and lead_status='working' and lead_present=1",(top,))==1
            assert not (cell.client_home/'.local/state/taskr/taskr.db').exists()
            results.append('once bytes, host heartbeat, CAS and root lead, no local ledger')
            a=cell.want(go,['--json','daemon','--status']);b=cell.want(rust,['--json','daemon','--status'])
            assert (a.stdout,a.stderr)==(b.stdout,b.stderr),(a.stdout,b.stdout,a.stderr,b.stderr)
            results.append('client status bytes')
            p=start(rust,cell.client_env)
            try:
                eventually(lambda:bool(fake.requests))
                (cell.client_home/'agents.json').write_text(json.dumps([{'name':'lane','pane_id':'wLane:p1','agent_status':'idle','state_change_seq':2}]))
                fake.wake()
                eventually(lambda:cell.count('select count(*) from launches where id=? and observed_seq=2',(launch,))==1)
                assert not fake.extra_bytes
                fake.close()
                p.wait(timeout=15)
                assert p.returncode==0
                results.append('resident upload on event and socket-removal exit')
            finally:
                if p.poll() is None:stop(p)
        finally:
            if not fake.stop.is_set():fake.close()
            cell.close()
    out.write_text(json.dumps({'passed':len(results),'results':results},indent=2)+'\n')
    print(json.dumps({'passed':len(results),'mismatch':0}))


if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('--go',type=Path,required=True);p.add_argument('--rust',type=Path,required=True);p.add_argument('--out',type=Path,required=True)
    a=p.parse_args();run(a.go.resolve(),a.rust.resolve(),a.out)
