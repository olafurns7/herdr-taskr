"""A real full refresh with every panel toggled on must respect all three scopes.

Health reads only with both health_repo and health_root; a configured repos list with no
saved panels starts with My PRs, Shipped and Build shown."""
import fcntl, json, os, pathlib, pty, select, struct, subprocess, sys, tempfile, termios, time

for configured, health_root in [(False, None), (True, 4242), ('repo_only', None)]:
    with tempfile.TemporaryDirectory(prefix='taskr-repo-scopes-') as temp:
        root = pathlib.Path(temp)
        tools = root / 'bin'
        tools.mkdir()
        log = root / 'gh.log'
        log.write_text('')
        taskr_log = root / 'taskr.log'
        taskr_log.write_text('')
        gh = tools / 'gh'
        gh.write_text('''#!/bin/sh
printf '%s\\n' "$*" >> "$HOME/gh.log"
case "$*" in
  'api user --jq .login') printf 'example-user\\n' ;;
  'api graphql'*) printf '{"data":{"search":{"issueCount":0,"nodes":[]}}}\\n' ;;
  *) printf '[]\\n' ;;
esac
''')
        gh.chmod(0o700)
        taskr = tools / 'taskr'
        taskr.write_text('''#!/bin/sh
printf '%s\\n' "$*" >> "$HOME/taskr.log"
case "$*" in
  '--json log '*) exit 0 ;;
  *) printf '{}\\n' ;;
esac
''')
        taskr.chmod(0o700)
        if configured:
            config = root / '.config/taskr/tui.json'
            config.parent.mkdir(parents=True)
            settings = {'repos': ['example-org/app', 'example-org/tools'],
                'issues_repo': 'separate/issues', 'health_repo': 'separate/health'}
            if health_root:
                settings['health_root'] = health_root
            config.write_text(json.dumps(settings))
        env = os.environ.copy()
        for key in ['TASKR_TASK', 'TASKR_LAUNCH', 'TASKR_DB', 'NO_COLOR']:
            env.pop(key, None)
        env.update(HOME=temp, XDG_CONFIG_HOME=str(root / '.config'), PATH=str(tools),
                   TASKR_BIN=str(taskr), TERM='xterm-256color')
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 80, 160, 0, 0))
        child = subprocess.Popen([sys.argv[1], '--theme', 'dark'], stdin=slave,
            stdout=slave, stderr=slave, env=env, start_new_session=True)
        os.close(slave)
        output = bytearray()
        replies = 0
        def pump(seconds, predicate=None):
            global replies
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if predicate is not None and predicate():
                    return
                if not select.select([master], [], [], 0.05)[0]:
                    continue
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    child.wait(timeout=5)
                    break
                if not chunk:
                    break
                output.extend(chunk)
                while replies < output.count(b'\x1b[6n'):
                    os.write(master, b'\x1b[1;1R')
                    replies += 1
            if predicate is not None:
                assert predicate(), (configured, child.poll(), repr(bytes(output[-4000:])))
        try:
            # Configured repos with no saved panels show 4-6 at start.
            repo_panels = b'4 PRs+' if configured else b'4 PRs-'
            pump(5, lambda: repo_panels in output and b'H Health-' in output)
            if not configured:
                assert log.read_text() == '', 'fresh hidden panels called gh'
            # Toggle every optional panel on, then refresh twice through the real event loop.
            os.write(master, (b'' if configured else b'456') + b'7890Hr\x0c')
            pump(5, lambda: b'4 PRs+' in output and b'H Health+' in output)
            os.write(master, b'r\x0c')
            if configured:
                pump(5, lambda: len(log.read_text().splitlines()) >= 8)
                calls = log.read_text().splitlines()
                assert calls.count('api user --jq .login') == 1, calls
                searches = [line for line in calls if line.startswith('api graphql')]
                assert len(searches) == 1 and 'repo:example-org/app' in searches[0] and 'repo:example-org/tools' in searches[0], calls
                for repo in ['example-org/app', 'example-org/tools']:
                    assert any(line.startswith('pr list ') and '--repo ' + repo + ' ' in line for line in calls), calls
                    assert any(line.startswith('run list ') and '--repo ' + repo + ' ' in line for line in calls), calls
                issues = [line for line in calls if line.startswith('issue list ')]
                assert len(issues) == 2 and all('--repo separate/issues ' in line for line in issues), calls
                assert any('--state open' in line for line in issues) and any('--state closed' in line for line in issues), calls
                if health_root:
                    pump(5, lambda: '--json log 4242 --limit 100' in taskr_log.read_text())
                    # Toggle saves keep the root.
                    saved = json.loads(config.read_text())
                    assert saved['health_root'] == 4242 and saved['panels'][10], saved
                else:
                    pump(0.6)
                    assert '--json log ' not in taskr_log.read_text(), taskr_log.read_text()
                    assert b'set "health_repo" and "health_root" in ~/.config/taskr/tui.json' in output
            else:
                # Drain multiple polling ticks after both manual refreshes.
                pump(0.6)
                assert log.read_text() == '', log.read_text()
                assert '--json log ' not in taskr_log.read_text(), taskr_log.read_text()
                for hint in [b'"repos"', b'"issues_repo"', b'"health_repo" and "health_root"']:
                    assert b'set ' + hint + b' in ~/.config/taskr/tui.json' in output, (hint, repr(bytes(output[-6000:])))
            os.write(master, b'q')
            pump(5, lambda: child.poll() is not None)
            assert child.wait(timeout=5) == 0
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            os.close(master)
print('PASS: fresh defaults and all-panel manual refreshes make zero gh calls; configured PR, Shipped, Build, Issues and Health scopes are used; Health needs its root')
