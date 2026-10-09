#!/usr/bin/env python3
"""Frozen-clock synthetic check-in proof; no installed binaries or live state."""
import argparse
import json
from pathlib import Path
import sqlite3
import subprocess
from datetime import datetime, timedelta, timezone
import tempfile
import time
from daemon_cell import Herdr, env, record


class Cell:
    def __init__(self, binary, home):
        self.binary, self.home = binary, home
        self.fake = Herdr(home)
        self.env = env(home, self.fake)
        self.env['TASKR_CHECKIN'] = '1'
        record(binary, self.env, 'status')
        self.agents = []

    def sql(self, sql, args=()):
        with sqlite3.connect(self.env['TASKR_DB']) as db:
            return db.execute(sql, args).fetchall()

    def root(self, rid=1, status='idle', remote=False, parked=False, waiting=False):
        pane = f'w{rid}:p0'
        self.sql("insert into tasks(id,name,role,status,pane_id,machine,lead_status,lead_present,lead_observed_at,waiting_until,created_at,updated_at) values(?,?,'orchestrator','open',?,?,?,1,'2026-10-08T00:00:00.000Z',?,'2026-10-08T00:00:00.000Z','2026-10-08T00:00:00.000Z')", (rid, f'campaign-{rid}', pane, 'client' if remote else None, status, '2026-10-09T00:00:00.000Z' if waiting else None))
        if not remote:
            self.agents.append({'name': f'campaign-{rid}', 'pane_id': pane, 'agent_status': status, 'state_change_seq': 1})
        if parked:
            self.event(rid, 'ref', data={'key': 'glance.state', 'value': 'parked'})

    def event(self, tid, kind, recipient=None, data=None, related=None, at='2026-10-08T00:00:00.000Z'):
        self.sql('insert into events(task_id,recipient_task_id,kind,summary,data,related_event_id,created_at) values(?,?,?,?,?,?,?)', (tid, recipient, kind, 'synthetic', json.dumps(data or {}), related, at))
        return self.sql('select max(id) from events')[0][0]

    def signal(self, rid=1):
        lane = rid + 100
        self.sql("insert into tasks(id,parent_id,name,role,status,created_at,updated_at) values(?,?,'lane','implementer','done','2026-10-08T00:00:00.000Z','2026-10-08T00:00:00.000Z')", (lane, rid))
        return self.event(lane, 'done', rid)

    def clock(self, at):
        self.env['TASKR_FROZEN_NOW'] = f'2026-10-08T{at}Z'

    def run(self, refresh_remote=True):
        if refresh_remote:
            stamp = self.env['TASKR_FROZEN_NOW'].replace('Z', '.000Z')
            for (host,) in self.sql('select distinct machine from tasks where machine is not null'):
                self.sql('insert into meta(key,value) values(?,?) on conflict(key) do update set value=excluded.value', (f'daemon_heartbeat:{host}', stamp))
        (self.home / 'agents.json').write_text(json.dumps(self.agents))
        started = time.monotonic()
        record(self.binary, self.env, 'daemon', '--once')
        return time.monotonic() - started

    def glance(self):
        stamp = self.env['TASKR_FROZEN_NOW'].replace('Z', '.000Z')
        self.sql("insert into meta(key,value) values('daemon_heartbeat',?) on conflict(key) do update set value=excluded.value", (stamp,))
        return record(self.binary, self.env, 'glance')

    def calls(self, kind):
        return [a for a in map(json.loads, (self.home / 'calls').read_text().splitlines()) if a[:2] == kind]

    def nudges(self):
        return self.sql("select id,task_id,json_extract(data,'$.nudge'),event_key from events where kind='prompt' and json_extract(data,'$.nudge') is not null")


def run(binary):
    results = []
    def case(name, check):
        with tempfile.TemporaryDirectory(prefix='taskr-checkin-') as temp:
            c = Cell(binary, Path(temp))
            try:
                check(c)
                results.append(name)
            finally:
                c.fake.close()

    def threshold(c, rule):
        c.root()
        anchor = c.signal() if rule == 'R1' else 0
        c.clock('00:29:59' if rule == 'R1' else '01:29:59')
        c.run(); assert not c.nudges()
        c.clock('00:30:00' if rule == 'R1' else '01:30:00')
        assert c.run() < 5
        n = c.nudges(); assert len(n) == 1 and n[0][2:] == (rule, f'nudge:{rule}:1:{anchor}'), n
        assert not c.sql("select key from meta where key like 'receipt_due:%'")
        c.run(); assert c.nudges() == n
        prompts = c.calls(['agent', 'prompt'])
        assert len(prompts) == 1 and '--wait' not in prompts[0], prompts
        assert prompts[0][3].startswith('taskr check-in (automatic, not the owner): '), prompts
        assert c.sql("select recipient_task_id from events where kind='prompt_outcome' and related_event_id=?", (n[0][0],)) == [(None,)]
        c.clock('01:29:59' if rule == 'R1' else '02:29:59')
        c.run(); assert not c.calls(['notification', 'show'])
        c.clock('01:30:00' if rule == 'R1' else '02:30:00')
        c.run(); c.run()
        assert c.nudges() == n
        assert len(c.calls(['notification', 'show'])) == 1
        if rule == 'R1':
            glance = c.glance()
            backlog = [a for a in glance['attention'] if a['kind'] == 'lead_idle_results']
            assert len(backlog) == 1 and backlog[0]['count'] == 1, backlog
            c.sql('update tasks set acked_event_id=? where id=1', (anchor,))
            c.run(); assert len([row for row in c.nudges() if row[2] == 'R1']) == 1
            return
        # Lead replies do not rearm R2.
        c.event(1, 'note'); c.event(1, 'next')
        c.clock('05:00:00'); c.run(); assert c.nudges() == n
        assert len(c.calls(['notification', 'show'])) == 1
        glance = c.glance()
        assert glance['campaigns'][0]['last']['kind'] == 'next'
    for rule in ['R1', 'R2']:
        case(rule + ' thresholds, dedupe, no receipt, escalation, root replies', lambda c, rule=rule: threshold(c, rule))

    def unsafe(c, status, parked, waiting, remote, rule):
        c.root(status=status, parked=parked, waiting=waiting, remote=remote)
        if rule == 'R1': c.signal()
        if remote:
            c.clock('00:29:59' if rule == 'R1' else '01:29:59')
            c.run(); assert not c.calls(['notification', 'show'])
        c.clock('00:30:00' if rule == 'R1' else '01:30:00'); c.run(); c.run()
        assert not c.nudges() and not c.calls(['agent', 'prompt'])
        assert len(c.calls(['notification', 'show'])) == int(remote)
    for rule in ['R1', 'R2']:
        for state in ['working', 'blocked', 'waiting', 'parked', 'remote']:
            case(rule + ' safe lead: ' + state, lambda c, state=state, rule=rule: unsafe(c, state if state in ['working', 'blocked'] else 'done', state == 'parked', state == 'waiting', state == 'remote', rule))

    def cap(c):
        for rid in [1, 2, 3]: c.root(rid); c.signal(rid)
        c.clock('03:00:00'); assert c.run() < 5
        assert len(c.nudges()) == len(c.calls(['agent', 'prompt'])) == 1
        c.run(); assert len(c.nudges()) == 2
    case('one nudge per pass with three due, under 5 seconds', cap)

    def slow_prompt(c):
        for rid in [1, 2, 3]: c.root(rid); c.signal(rid)
        script = c.home / 'bin/herdr'
        script.write_text(script.read_text().replace('import json, os, sys', 'import json, os, sys, time').replace("if a[:2]==['agent','list']:", "if a[:2]==['agent','prompt']: time.sleep(3)\nif a[:2]==['agent','list']:"))
        c.clock('03:00:00')
        assert c.run() < 5
        assert len(c.nudges()) == len(c.calls(['agent', 'prompt'])) == 1
        assert c.sql("select json_extract(data,'$.outcome') from events where kind='prompt_outcome'") == [('delivery_unknown',)]
    case('slow prompt times out: one of three due roots, pass under five seconds', slow_prompt)

    def slow_notification(c):
        for rid in [1, 2, 3]: c.root(rid, remote=True); c.signal(rid)
        script = c.home / 'bin/herdr'
        script.write_text(script.read_text().replace('import json, os, sys', 'import json, os, sys, time').replace("if a[:2]==['agent','list']:", "if a[:2]==['notification','show']: time.sleep(3)\nif a[:2]==['agent','list']:"))
        c.clock('03:00:00')
        assert c.run() < 5
        assert len(c.calls(['notification', 'show'])) == 1
        assert len(c.sql("select key from meta where key glob 'escalated:R[12]:*'")) == 1
        assert not c.nudges() and not c.calls(['agent', 'prompt'])
    case('slow notification: loop budget stops after one of three fresh remote roots, under five seconds', slow_notification)

    def r2_reply(c, kind):
        c.root(); c.clock('01:30:00'); c.run()
        assert len(c.nudges()) == 1
        c.event(1, kind, at='2026-10-08T01:40:00.000Z')
        c.clock('02:30:00'); c.run(); c.run()
        c.clock('05:00:00'); c.run()
        assert len(c.nudges()) == 1 and not c.calls(['notification', 'show'])
    for kind in ['note', 'next']:
        case('R2 root ' + kind + ' reply stops escalation without rearming', lambda c, kind=kind: r2_reply(c, kind))

    def steady_results(c):
        c.root(); first = c.signal()
        for h in range(4):
            c.clock(f'0{h}:30:00'); c.run()
            c.event(101, 'done', 1, at=f'2026-10-08T0{h+1}:00:00.000Z')
        c.clock('04:30:00'); c.run(); c.run()
        assert len(c.calls(['notification', 'show'])) == 1
        assert c.calls(['notification', 'show'])[0][4] == 'campaign-1: R1 90m'
        assert c.sql("select key from meta where key like 'escalated:R1:%'") == [(f'escalated:R1:1:{first}',)]
        assert len(c.nudges()) == 1
        c.sql('update tasks set acked_event_id=? where id=1', (first,))
        c.run(); assert len(c.nudges()) == 2
    case('steady hourly results escalate original R1 anchor once; ack allows new nudge', steady_results)

    def remote_stream(c):
        c.root(remote=True); c.signal(); c.clock('00:30:00'); c.run()
        for minute in range(35, 90, 5):
            at = f'{minute // 60:02}:{minute % 60:02}:00'
            c.event(101, 'done', 1, at=f'2026-10-08T{at}.000Z')
            c.clock(at); c.run()
        c.clock('01:29:59'); c.run()
        assert len(c.calls(['notification', 'show'])) == 1
        assert len(c.sql("select key from meta where key glob 'escalated:R[12]:1:*'")) == 1
        c.clock('01:30:00'); c.run(); c.run()
        assert len(c.calls(['notification', 'show'])) == 2
        assert not c.nudges() and not c.calls(['agent', 'prompt'])
    case('remote results every five minutes notify once in first hour; cooldown opens at exactly sixty minutes', remote_stream)

    def remote_rules(c):
        c.root(remote=True); c.clock('01:30:00'); c.run()
        c.signal(); c.clock('01:35:00'); c.run()
        assert len(c.calls(['notification', 'show'])) == 1
        c.clock('02:30:00'); c.run()
        assert [a[4] for a in c.calls(['notification', 'show'])] == ['campaign-1: R2 90m', 'campaign-1: R1 150m']
    case('remote escalation cooldown spans R2 and R1 on the same root', remote_rules)

    def remote_stale(c, rule):
        c.root(remote=True)
        if rule == 'R1': c.signal()
        c.clock('03:00:00'); c.run(refresh_remote=False)
        assert not c.calls(['notification', 'show'])
        c.sql("insert into meta values('daemon_heartbeat:client','2026-10-08T02:59:30.000Z')")
        c.run(refresh_remote=False)
        assert not c.calls(['notification', 'show'])
        c.sql("update meta set value='2026-10-08T02:59:31.000Z' where key='daemon_heartbeat:client'")
        c.run(refresh_remote=False); c.run(refresh_remote=False)
        assert not c.nudges() and len(c.calls(['notification', 'show'])) == 1
    for rule in ['R1', 'R2']:
        case(rule + ' remote notification requires heartbeat younger than 30 seconds', lambda c, rule=rule: remote_stale(c, rule))

    def next_milestone(c):
        c.env.pop('TASKR_CHECKIN'); c.root(); c.signal()
        c.event(1, 'note', at='2026-10-08T00:10:00.000Z')
        c.event(101, 'next', at='2026-10-08T00:20:00.000Z')
        c.clock('00:30:00')
        assert c.glance()['campaigns'][0]['last']['kind'] == 'note'
        c.event(1, 'next', at='2026-10-08T00:30:00.000Z')
        assert c.glance()['campaigns'][0]['last']['kind'] == 'next'
    case('switch-off next milestone includes root only; lane next cannot replace root note', next_milestone)

    def orphan_ask(c):
        c.root(parked=True); c.signal()
        c.sql("update tasks set status='closed' where id=101")
        c.event(101, 'ask', data={'owner': True, 'blocking': False})
        c.clock('05:00:00'); c.run(); c.run()
        assert not c.calls(['notification', 'show']) and not c.glance()['needs_you']
        # Preserve the existing unconditional blocking branch, even for closed askers.
        c.event(101, 'ask', data={'owner': True, 'blocking': True})
        c.run(); assert len(c.calls(['notification', 'show'])) == 1
    case('aged non-blocking ask on closed asker is excluded; blocking branch unchanged', orphan_ask)

    def cooldown(c):
        c.root(); first = c.signal()
        c.clock('00:30:00'); c.run()
        c.sql('update tasks set acked_event_id=? where id=1', (first,))
        c.event(101, 'done', 1, at='2026-10-08T00:31:00.000Z')
        c.clock('01:01:00'); c.run(); assert len(c.nudges()) == 1
        c.clock('01:30:00'); c.run(); assert len(c.nudges()) == 2
    case('new anchor respects 60 minute cooldown', cooldown)

    def silent_busy(c, state):
        c.root(); c.signal()
        c.sql('delete from events')
        if state == 'owner': c.event(101, 'ask', 1, {'owner': True})
        else:
            c.sql('update tasks set status=? where id=101', (state,))
        c.clock('03:00:00'); c.run(); assert not c.nudges()
    for state in ['open', 'ready', 'owner']:
        case('R2 excludes ' + state, lambda c, state=state: silent_busy(c, state))

    def recent(c):
        c.root()
        c.event(1, 'prompt', at='2026-10-08T01:00:00.000Z')
        c.clock('02:29:59'); c.run(); assert not c.nudges()
        c.clock('02:30:00'); c.run(); assert c.nudges()[0][2] == 'R2'
        c.event(1, 'note', at='2026-10-08T02:30:00.000Z')
        c.clock('04:30:00'); c.run(); assert len(c.nudges()) == 1
        c.event(1, 'prompt', at='2026-10-08T04:30:00.000Z')
        c.clock('06:00:00'); c.run(); assert len(c.nudges()) == 2
    case('ordinary prompts reset R2; root replies do not', recent)

    def observation_failure(c):
        c.root(); c.signal(); c.clock('03:00:00')
        script = c.home / 'bin/herdr'
        script.write_text(script.read_text().replace("if a[:2]==['agent','list']:", "if a[:2]==['agent','list']: sys.exit(1)\nif a[:2]==['agent','list']:"))
        import subprocess
        p = subprocess.run([str(binary), 'daemon', '--once'], env=c.env, capture_output=True, timeout=10)
        assert p.returncode == 5 and not c.nudges()
    case('failed observation cannot prompt using stale cached lead state', observation_failure)

    def parked_activity(c):
        c.root(parked=True)
        before = c.glance()
        attempt = c.event(1, 'prompt', data={'nudge': 'R2'})
        c.event(1, 'prompt_outcome', related=attempt, data={'outcome': 'delivered'})
        after = c.glance()
        assert not after['attention']
        assert not after['campaigns'][0].get('parked_active')
        assert after['campaigns'][0]['activity_age_ms'] == before['campaigns'][0]['activity_age_ms']
    case('nudge and outcome do not activate parked glance', parked_activity)

    def off(c):
        c.root(); c.signal(); c.clock('05:00:00')
        c.env.pop('TASKR_CHECKIN'); c.run()
        assert not c.nudges() and not c.calls(['notification', 'show'])
    case('switch off by default', off)

    def owner_asks(c):
        c.root()
        local = c.event(1, 'ask', data={'owner': True, 'blocking': False})
        blocking = c.event(1, 'ask', data={'owner': True, 'blocking': True})
        c.clock('03:59:59'); c.run()
        assert len(c.calls(['notification', 'show'])) == 1
        assert c.sql("select key from meta where key like 'notified:%'") == [(f'notified:{blocking}',)]
        c.env.pop('TASKR_CHECKIN'); c.clock('04:00:00'); c.run()
        assert len(c.calls(['notification', 'show'])) == 1
        c.env['TASKR_CHECKIN'] = '1'; c.run(); c.run()
        assert len(c.calls(['notification', 'show'])) == 2
        assert sorted(c.sql("select key from meta where key like 'notified:%'")) == sorted([(f'notified:{blocking}',), (f'notified:{local}',)])
    case('owner asks: immediate blocking, opt-in four hour non-blocking, dedupe', owner_asks)

    from daemon_cell import eventually
    from net_cell import Cell as NetworkCell
    with tempfile.TemporaryDirectory(prefix='taskr-checkin-client-') as tmp:
        network = NetworkCell(binary, binary, tmp)
        fake = Herdr(network.client_home)
        network.client_env.update(HERDR_SOCKET_PATH=str(fake.path), PATH=str(network.client_home / 'bin') + ':' + network.env['PATH'], TASKR_CHECKIN='1')
        try:
            top = network.record(['new', 'client-campaign', '--role', 'orchestrator', '--pane', 'wClient:p0'])
            rid = json.loads(top.stdout)['task_id']
            (network.client_home / 'agents.json').write_text(json.dumps([{'name': 'client-campaign', 'pane_id': 'wClient:p0', 'agent_status': 'done'}]))
            at = (datetime.now(timezone.utc) - timedelta(hours=4, seconds=1)).isoformat(timespec='milliseconds').replace('+00:00', 'Z')
            with sqlite3.connect(network.db) as db:
                db.execute("insert into events(task_id,kind,summary,data,created_at) values(?,'ask','synthetic remote owner ask',?,?)", (rid, json.dumps({'owner': True, 'blocking': False}), at))
                ask = db.execute('select max(id) from events').fetchone()[0]
            network.obj(binary, ['daemon', '--once'])
            assert not any(json.loads(line)[:2] == ['notification', 'show'] for line in (network.client_home / 'calls').read_text().splitlines())
            assert network.count("select count(*) from meta where key=?", (f'notified:{ask}',)) == 0
            # Restart only this synthetic hub to turn its daemon-start switch on.
            network.close()
            network.hub_env['TASKR_CHECKIN'] = '1'
            network.log = open(Path(tmp) / 'hub.log', 'ab')
            network.hub = subprocess.Popen([str(binary), 'daemon', '--stay'], env=network.hub_env, stdout=network.log, stderr=network.log)
            def bound():
                with sqlite3.connect(network.db) as db:
                    row = db.execute("select value from meta where key='dashboard_url'").fetchone()
                if not row: return False
                assert network.hub.poll() is None, (Path(tmp) / 'hub.log').read_text()
                port = row[0].split(':')[-1].strip('/')
                network.url = f'http://[::1]:{port}'
                network.server_file.write_text(network.url + '\n')
                return True
            eventually(bound)
            network.obj(binary, ['daemon', '--once'])
            network.obj(binary, ['daemon', '--once'])
            notifications = [json.loads(line) for line in (network.client_home / 'calls').read_text().splitlines() if json.loads(line)[:2] == ['notification', 'show']]
            assert len(notifications) == 1 and 'synthetic remote owner ask' in notifications[0], notifications
            assert network.count("select count(*) from meta where key=?", (f'notified:{ask}',)) == 1
            results.append('actual client host_reply: hub-owned opt-in gate and notification once')
        finally:
            fake.close()
            network.close()

    print(json.dumps({'passed': len(results), 'results': results}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--rust', required=True, type=Path)
    run(parser.parse_args().rust.resolve())
