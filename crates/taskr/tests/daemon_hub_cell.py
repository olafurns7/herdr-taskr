#!/usr/bin/env python3
"""Rust hub daemon + Go/Rust verified clients, all synthetic identities/sockets."""
import argparse
import json
import tempfile
from pathlib import Path
from daemon_cell import Herdr
from net_cell import Cell


def run(go,rust,out):
    results=[]
    with tempfile.TemporaryDirectory(prefix='taskr-rhub-') as tmp:
        # Cell's first binary runs the hub; its second is used only by compare().
        cell=Cell(rust,go,tmp)
        fake=Herdr(cell.client_home)
        cell.client_env.update(HERDR_SOCKET_PATH=str(fake.path),PATH=str(cell.client_home/'bin')+':'+cell.env['PATH'])
        try:
            response=cell.record(['new','campaign','--role','orchestrator','--pane','wRoot:p0','--workspace','wRoot'])
            top=json.loads(response.stdout)['task_id']
            response=cell.record(['new','lane','--role','implementer','--parent',str(top),'--pane','wLane:p1','--workspace','wLane'])
            worker=json.loads(response.stdout)['task_id']
            response=cell.record(['launch',str(worker),'--provider','fixture','--model','fixture','--effort','medium'])
            launch=json.loads(response.stdout)['launch_id']
            assert cell.count('select count(*) from tasks where id=? and machine=?',(worker,'host-a'))==1
            results.append('stored RPC writes and exact duplicate replies, caller host')
            (cell.client_home/'agents.json').write_text(json.dumps([{'name':'lane','pane_id':'wLane:p1','agent_status':'blocked','state_change_seq':1},{'name':'campaign','pane_id':'wRoot:p0','agent_status':'working','state_change_seq':1}]))
            for binary in [go,rust]:
                value=cell.obj(binary,['daemon','--once'])
                assert value=={'mode':'client','ok':True,'once':True,'watch':['wLane:p1']},value
            assert cell.count("select count(*) from events where kind='herdr' and task_id=?",(worker,))==1
            assert cell.count("select count(*) from tasks where id=? and lead_status='working'",(top,))==1
            results.append('both client daemon uploads through Rust hub, scoped CAS and leads')
            # Status is served against the same persisted identity record; timestamps are stable.
            import subprocess
            a=subprocess.run([str(go),'--json','daemon','--status'],env=cell.hub_env,capture_output=True,timeout=15)
            b=subprocess.run([str(rust),'--json','daemon','--status'],env=cell.hub_env,capture_output=True,timeout=15)
            assert (a.returncode,a.stdout,a.stderr)==(b.returncode,b.stdout,b.stderr),(a.stdout,b.stdout,a.stderr,b.stderr)
            results.append('hub-mode status bytes against Go')
        finally:
            fake.close();cell.close()
    out.write_text(json.dumps({'passed':len(results),'results':results},indent=2)+'\n')
    print(json.dumps({'passed':len(results),'mismatch':0}))


if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('--go',type=Path,required=True);p.add_argument('--rust',type=Path,required=True);p.add_argument('--out',type=Path,required=True)
    a=p.parse_args();run(a.go.resolve(),a.rust.resolve(),a.out)
