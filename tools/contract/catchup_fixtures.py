"""Synthetic P1a/P1b corpus for the existing byte/logical-DB runner."""
import hashlib
import json
import sqlite3
from datetime import datetime, timedelta, timezone

NOW = datetime(2026, 10, 8, tzinfo=timezone.utc)

def stamp(age=0):
    return (NOW - timedelta(minutes=age)).isoformat(timespec='milliseconds').replace('+00:00', 'Z')

def fixtures(directory, schema):
    corpus = {}
    variants = [f'{status}-{lease}' for status in ('working', 'idle', 'done', 'unknown', 'blocked', 'gone') for lease in (0, 1)]
    variants += ['asks-blocking', 'asks-nonblocking', 'asks-answered', 'asks-closed', 'root-with-launch', 'notes', 'parked', 'parked-active', 'reparked', 'quiet', 'unregistered-119', 'unregistered-120', 'unregistered-quiet', 'stale', 'planned', 'missing-heartbeat', 'missing-listing', 'rich', 'archived', 'paging', 'sparks']
    variants += ['owner-' + str(i) for i in range(9)]
    for variant in variants:
        path = directory / (variant + '.db')
        with sqlite3.connect(path) as db:
            db.executescript(schema)
            def task(name, parent=None, status='open', pane='wDemo:p1', age=0):
                cur = db.execute("insert into tasks(name,parent_id,role,status,pane_id,lead_status,lead_present,lead_observed_at,created_at,updated_at) values(?,?,'orchestrator',?,?,'working',1,?,?,?)", (name,parent,status,pane,stamp(1),stamp(age),stamp(age)))
                return cur.lastrowid
            def event(task, kind, text='', data=None, age=0, recipient=None):
                return db.execute('insert into events(task_id,recipient_task_id,kind,summary,data,created_at) values(?,?,?,?,?,?)',(task,recipient,kind,text,json.dumps(data or {}),stamp(age))).lastrowid
            def document(task, kind, name='', captured=True, version=1, eid=None):
                body = '# Synthetic <&> Þ😀\nSample goal.\n'
                sha = hashlib.sha256(body.encode()).hexdigest()
                db.execute('insert or ignore into doc_blobs values(?,?,?)',(sha,len(body.encode()),body))
                db.execute('insert into documents(root_id,task_id,kind,name,version,sha256,bytes,format,captured,reason,event_id,source_path,source_host,backfill,created_at) values(1,?,?,?,?,?,?,?, ?,?,?,?,?,?,?)',(task,kind,name,version,sha if captured else None,len(body.encode()),'md' if captured else None,captured,None if captured else 'missing',eid,'/synthetic/sample.md',None,0,stamp(20)))
            db.executemany('insert into meta values(?,?)', [('daemon_heartbeat',stamp()),('lead_listed_at',stamp()),('daemon_heartbeat:host-a',stamp())])
            r = task('campaign <&> Þ😀',age=1000)
            w = task('worker',r,'done',pane='wDemo:p2')
            event(w,'ready','synthetic ready',age=31,recipient=r)
            if variant.rsplit('-',1)[0] in ('working','idle','done','unknown','blocked','gone'):
                status,lease = variant.rsplit('-',1)
                db.execute('update tasks set lead_status=?,lead_present=?,waiting_until=? where id=?',(status,status!='gone',stamp(-1) if lease=='1' else None,r))
                sub=task('sub',r)
                event(w,'fail','sub backlog',age=180,recipient=sub)
            elif variant.startswith('asks-'):
                ask=event(w,'ask','owner action',{'owner':True,'blocking':variant=='asks-blocking'},age=2)
                if variant=='asks-answered':
                    answer=event(r,'owner_answer','approved')
                    db.execute('update events set answered_by=? where id=?',(answer,ask))
                if variant=='asks-closed': db.execute("update tasks set status='closed' where id=?",(w,))
            elif variant=='root-with-launch':
                launch=db.execute("insert into launches(task_id,recorded_at) values(?,?)",(r,stamp())).lastrowid
                db.execute('update tasks set current_launch_id=? where id=?',(launch,r))
            elif variant.startswith('owner-'):
                texts=['OWNER: ship DONE: built','DONE: built OWNER: ship','OWNER: nothing.','OWNER: nothing yet (waiting)','OWNER: nothing until approval','x\u0085OWNER: approve','(OWNER: glued','OWNER:','bookkeeping']
                event(r,'note','OWNER: old action',{'owner':True},age=6000)
                event(r,'note',texts[int(variant[6:])],{'owner':True},age=5000)
                event(r,'note','ordinary bookkeeping')
            elif variant.startswith('parked') or variant=='reparked':
                db.execute("update tasks set lead_present=0 where id=?",(r,))
                event(w,'ask','owner approval',{'owner':True,'blocking':True},age=180)
                event(r,'ref',data={'key':'glance.state','value':'parked'},age=120)
                if variant!='parked': event(w,'note','activity with older clock',age=300)
                if variant=='reparked': event(r,'ref',data={'key':'glance.state','value':'parked'},age=90)
            elif variant in ('quiet','unregistered-quiet'):
                db.execute('update events set created_at=?',(stamp(1000),))
                if variant=='unregistered-quiet': db.execute('update tasks set pane_id=null where id=?',(r,))
            elif variant.startswith('unregistered-'):
                db.execute('update tasks set pane_id=null where id=?',(r,))
                event(r,'note','silent',age=int(variant.split('-')[1]))
            elif variant=='stale':
                db.execute("update tasks set machine='host-a' where id=?",(r,))
                db.execute("update meta set value=? where key='daemon_heartbeat:host-a'",(stamp(60),))
            elif variant=='planned': db.execute("update tasks set status='planned' where id=?",(r,))
            elif variant=='missing-heartbeat': db.execute("delete from meta where key='daemon_heartbeat'")
            elif variant=='missing-listing': db.execute("delete from meta where key='lead_listed_at'")
            elif variant in ('rich','archived','paging','sparks','notes'):
                closed=task('closed-parent',r,'closed')
                nested=task('nested',closed,pane='wDemo:p3')
                launch=db.execute("insert into launches(task_id,provider,model,effort,machine,pane_id,observed_status,present,recorded_at) values(?,'fixture','fixture','high','host-a','wDemo:p4','idle',1,?)",(nested,stamp())).lastrowid
                db.execute('update tasks set current_launch_id=? where id=?',(launch,nested))
                event(nested,'ask','blocking <&> approval',{'owner':True,'blocking':True},age=1)
                event(closed,'ask','closed asker',{'owner':True})
                for i in range(21):
                    ask=event(r,'ask',str(i),age=i)
                    ans=event(r,'owner_answer','answered',age=i)
                    db.execute('update events set answered_by=? where id=?',(ans,ask))
                decision=event(r,'decision','retired')
                db.execute('insert into events(task_id,kind,related_event_id,created_at) values(?,?,?,?)',(r,'revoke',decision,stamp()))
                event(r,'decision','kept')
                event(r,'next','review sample')
                document(r,'goal');document(r,'goal',captured=False,version=2)
                document(r,'plan',eid=decision);document(r,'plan','named');document(nested,'brief');document(nested,'report',captured=False)
                for key,value in [('pr','123'),('pr.backend','https://example.test/pull/123'),('pr.signed','+124'),('pr.zero','0'),('pr.large','4294967296'),('pr.title','Demo PR'),('pr.review','approved'),('pr.state','open'),('pr.ci','pass')]: event(nested,'ref',data={'key':key,'value':value})
                event(r,'ref',data={'key':'pr','value':'2'});event(r,'ref',data={'key':'pr','value':''})
                event(r,'note','OWNER: old action',{'owner':True},age=6000);event(r,'note','bookkeeping',{'owner':True})
                empty=task('empty',age=1000)
                task('quiet',age=1000)
                if variant=='archived': db.execute("update tasks set status='closed' where id=?",(r,))
                if variant=='paging':
                    for i in range(101): task('lane-'+str(i),r);event(r,'note',str(i))
                if variant=='sparks':
                    for age in [230.001,230,220.001,220,0,-0.001]: event(nested,'note','bucket edge',age=age)
                    event(empty,'note','other campaign',age=230)
        corpus[variant]=path
    return corpus
