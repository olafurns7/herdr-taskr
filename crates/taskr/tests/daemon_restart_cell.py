#!/usr/bin/env python3
"""Synthetic Linux restart, setsid, minimal environment and stay-lock lifecycle."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import golden
import argparse
import ctypes
import select
import json
import os
import selectors
import signal
import sqlite3
import subprocess
import tempfile
import threading
from daemon_cell import Herdr,env,eventually,record,start,stop


def alive(pid):
    try: os.kill(pid,0);return True
    except ProcessLookupError:return False


def run(go,rust,out):
    results=[]
    for binary,label in [(go,'go'),(rust,'rust')]:
        with tempfile.TemporaryDirectory(prefix='taskr-restart-') as tmp:
            home=Path(tmp).resolve();state=home/'.local/state/taskr';state.mkdir(parents=True)
            (state/'dashboard.addr').write_text('off')
            fake=Herdr(home);e=env(home,fake)
            e['TASKR_DB']=str(state/'taskr.db');e.pop('TASKR_FROZEN_NOW')
            e.update(TASKR_TASK='untrusted-fixture',TASKR_LAUNCH='untrusted-fixture',INVOCATION_ID='inherited-fixture',SYSTEMD_EXEC_PID='not-the-child')
            owned=set();holder=None;waiting=None
            try:
                # A stopped daemon restart starts exactly one detached child with only these variables.
                first=record(binary,e,'daemon','--restart');pid=first['new_pid'];owned.add(pid)
                if label == 'go': golden.observe('first', first)
                assert first['restarted'] is False and first['version']=='dev',first
                assert os.getsid(pid)==pid
                environ=(Path('/proc')/str(pid)/'environ').read_bytes().split(b'\0')
                keys={p.split(b'=',1)[0].decode() for p in environ if p}
                assert keys==({'HOME','PATH','HERDR_SOCKET_PATH','TASKR_TMP_BASE'} if label=='rust' else {'HOME','PATH','HERDR_SOCKET_PATH'}),keys
                if label=='rust': assert b'TASKR_TMP_BASE='+e['TASKR_TMP_BASE'].encode() in environ
                results.append(label+': initial detached restart, setsid, minimal env')
                second=record(binary,e,'daemon','--restart');new=second['new_pid'];owned.add(new)
                if label == 'go': golden.observe('second', second)
                assert second['old_pid']==pid and second['restarted'] is True and second['old_version']=='dev' and new!=pid,second
                assert os.getsid(new)==new
                assert int((state/'daemon.lock').read_text())==new
                results.append(label+': verified running restart and lock handoff')
                os.kill(new,signal.SIGTERM)
                eventually(lambda:(state/'daemon.lock').read_bytes()==b'')
                # A stay contender waits without a startup record, and signals cancel the wait.
                holder=start(binary,e)
                # Opening the contended lock happens after daemon cancellation is installed.
                libc = ctypes.CDLL(None, use_errno=True)
                opened = libc.inotify_init1(os.O_CLOEXEC)
                assert opened >= 0, ctypes.get_errno()
                try:
                    assert libc.inotify_add_watch(opened, os.fsencode(state/'daemon.lock'), 0x20) >= 0, ctypes.get_errno()
                    waiting=subprocess.Popen([str(binary),'daemon','--stay'],env=e,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                    assert select.select([opened],[],[],10)[0], 'stay did not reach the held lock'
                    assert os.read(opened,4096)
                finally:
                    os.close(opened)
                sel=selectors.DefaultSelector();sel.register(waiting.stdout,selectors.EVENT_READ)
                assert not sel.select(0),'stay printed startup before acquiring lock'
                waiting.terminate();stdout,stderr=waiting.communicate(timeout=5)
                assert waiting.returncode==0 and stdout==b'j1 null\n',(stdout,stderr)
                waiting=None;sel.close()
                results.append(label+': stay lock wait cancellation')
                waiting=subprocess.Popen([str(binary),'daemon','--stay'],env=e,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                stop(holder);holder=None
                sel=selectors.DefaultSelector();sel.register(waiting.stdout,selectors.EVENT_READ)
                assert sel.select(10),'stay did not take over free lock'
                line=waiting.stdout.readline();sel.close()
                first=json.loads(line.removeprefix(b'j1 '));assert first['pid']==waiting.pid,first
                status=record(binary,e,'daemon','--status');assert status['stay'] is True and status['supervised'] is False,status
                replacement=record(binary,e,'daemon','--restart');owned.add(replacement['new_pid'])
                if label == 'go': golden.observe('replacement', replacement)
                waiting.communicate(timeout=10);waiting=None
                status=record(binary,e,'daemon','--status');assert status['stay'] is True,status
                results.append(label+': stay takes over and restart preserves --stay')
                current=replacement['new_pid'];os.kill(current,signal.SIGTERM)
                eventually(lambda:(state/'daemon.lock').read_bytes()==b'')
                def supervised():
                    p=subprocess.Popen(['/bin/sh','-c','export SYSTEMD_EXEC_PID=$$; exec "$@"','supervisor',str(binary),'daemon','--stay'],env=e,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                    sel=selectors.DefaultSelector();sel.register(p.stdout,selectors.EVENT_READ);assert sel.select(10)
                    json.loads(p.stdout.readline().removeprefix(b'j1 '));sel.close();return p
                holder=supervised();previous=holder
                next_process=[]
                def relaunch():
                    previous.wait(timeout=15);next_process.append(supervised())
                monitor=threading.Thread(target=relaunch);monitor.start()
                swapped=record(binary,e,'daemon','--restart');monitor.join(timeout=15)
                if label == 'go': golden.observe('swapped', swapped)
                assert next_process and not monitor.is_alive()
                holder=next_process[0]
                assert swapped['supervised'] is True and swapped['started_detached'] is False and swapped['new_pid']==holder.pid,swapped
                results.append(label+': supervised replacement is verified and awaited')
                # With no supervisor replacement, wait then fall back to one detached stay daemon.
                fallback=record(binary,e,'daemon','--restart');owned.add(fallback['new_pid'])
                if label == 'go': golden.observe('fallback', fallback)
                holder.communicate(timeout=5);holder=None
                assert fallback['started_detached'] is True and fallback['restarted'] is True,fallback
                assert record(binary,e,'daemon','--status')['stay'] is True
                results.append(label+': supervised restart falls back after deadline')

            finally:
                if waiting is not None and waiting.poll() is None:stop(waiting)
                if holder is not None and holder.poll() is None:stop(holder)
                for pid in owned:
                    if alive(pid):
                        try:os.kill(pid,signal.SIGTERM)
                        except ProcessLookupError:pass
                eventually(lambda:not (state/'daemon.lock').exists() or (state/'daemon.lock').read_bytes()==b'')
                fake.close()
    out.write_text(json.dumps({'passed':len(results),'results':results},indent=2)+'\n');print(json.dumps({'passed':len(results),'mismatch':0}))


if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('--go',type=Path,required=True);p.add_argument('--rust',type=Path,required=True);p.add_argument('--out',type=Path,required=True)
    a=golden.parse(p, __file__);run(a.go.resolve(),a.rust.resolve(),a.out)

    golden.finish()
