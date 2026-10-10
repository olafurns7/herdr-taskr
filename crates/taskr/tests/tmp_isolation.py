#!/usr/bin/env python3
"""Check actual fixture env builders without starting a daemon or deleting lane data."""
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
from unittest.mock import patch
sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import daemon_cell
import daemon_limits
import hub_cell
import net_cell
import run as contract


def check(environment):
    base = Path(environment['TASKR_TMP_BASE'])
    home = Path(environment['HOME'])
    assert base.is_absolute() and base.is_relative_to(home), environment
    assert base != home


class Stopped(Exception):
    pass


def no_process(*args, **kwargs):
    check(kwargs['env'])
    raise Stopped()


with tempfile.TemporaryDirectory(prefix='tmp-isolation-', dir=sys.argv[1]) as tmp:
    root = Path(tmp).resolve()
    check(daemon_cell.env(root, SimpleNamespace(path=root/'fake.sock')))
    for cls, name in [(net_cell.Cell, 'network'), (hub_cell.RustHub, 'hub')]:
        place = root/name
        place.mkdir()
        fixture = cls.__new__(cls)
        with patch.object(subprocess, 'Popen', side_effect=no_process):
            try:
                fixture.__init__(Path('/unused/oracle'), Path('/unused/rust'), place)
            except Stopped:
                pass
            else:
                raise AssertionError('fixture did not reach the process boundary')
        check(fixture.hub_env)
        check(fixture.client_env)
        assert fixture.hub_env['TASKR_TMP_BASE'] != fixture.client_env['TASKR_TMP_BASE']
        fixture.log.close()
    with patch.object(subprocess, 'Popen', side_effect=no_process):
        try:
            daemon_limits.check(Path('/unused/rust'), root/'limits', 256)
        except Stopped:
            pass
        else:
            raise AssertionError('limits fixture did not reach the process boundary')
    def no_run(*args, **kwargs):
        check(kwargs['env'])
        return SimpleNamespace(returncode=0, stdout=b'', stderr=b'')
    with patch.object(subprocess, 'run', side_effect=no_run):
        assert contract.execute(Path('/unused/rust'), ['daemon','--once'], root/'contract',root/'contract.db',{},1)==(0,b'',b'')
print('tmp isolation: daemon/network/limits/contract child envs use distinct scratch bases; no processes started')
