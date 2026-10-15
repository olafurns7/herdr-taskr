"""Real keyboard toggles must never replace a config that failed to load."""
import fcntl, json, os, pathlib, pty, select, struct, subprocess, sys, tempfile, termios, time

cases = [b'{bad json', json.dumps({'repos': [f'owner/repo{i}' for i in range(9)]}).encode(), b' ' * 16385]
if os.geteuid() != 0:
    cases.append(b'{"repos":["private/kept-unreadable"]}')
cases.extend(json.dumps({'panels': values, 'repos': ['kept/private']}).encode() for values in [
    [], [True] * 5, [True] * 7, [True] * 8, [True] * 12, [True, True, True, 1], [True, True, True, True, True, 'bad'],
    [True] * 8 + ['bad'], [True] * 9 + ['bad'], [True] * 10 + ['bad']])
for i, original in enumerate(cases):
    with tempfile.TemporaryDirectory(prefix='taskr-config-preservation-') as temp:
        root = pathlib.Path(temp)
        config = root / 'config/taskr/tui.json'
        config.parent.mkdir(parents=True)
        config.write_bytes(original)
        before = config.stat()
        if i == 3:
            config.chmod(0)
        tools = root / 'bin'
        tools.mkdir()
        for name, body in [('taskr', "printf '{}\\n'"), ('gh', "printf '[]\\n'")]:
            tool = tools / name
            tool.write_text('#!/bin/sh\n' + body + '\n')
            tool.chmod(0o700)
        env = os.environ.copy()
        env.update(HOME=temp, XDG_CONFIG_HOME=str(root / 'config'), PATH=str(tools), TASKR_BIN=str(tools / 'taskr'), TERM='xterm-256color')
        env.pop('NO_COLOR', None)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
        child = subprocess.Popen([sys.argv[1], '--theme', 'dark'], stdin=slave, stdout=slave, stderr=slave, env=env, start_new_session=True)
        os.close(slave)
        output = bytearray()
        replies = [0]
        def until(predicate):
            deadline = time.monotonic() + 5
            while not predicate() and time.monotonic() < deadline:
                if not select.select([master], [], [], 0.1)[0]:
                    continue
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    child.wait(timeout=1)
                    break
                if not chunk:
                    child.wait(timeout=1)
                    break
                output.extend(chunk)
                while replies[0] < output.count(b'\x1b[6n'):
                    os.write(master, b'\x1b[1;1R')
                    replies[0] += 1
            assert predicate(), (i, child.poll(), repr(bytes(output[-2000:])))
        try:
            until(lambda: b'using defaults' in output)
            toggled_at = len(output)
            os.write(master, b'4\x0c')
            until(lambda: b'4 PRs+' in output[toggled_at:] and b'using defaults' in output[toggled_at:])
            # Ctrl-L asks for a full frame, proving both the toggle and retained warning.
            os.write(master, b'q')
            until(lambda: child.poll() is not None)
            assert child.wait(timeout=5) == 0, (i, child.returncode, replies[0], repr(bytes(output[-4000:])))
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            os.close(master)
            config.chmod(0o600)
        assert config.read_bytes() == original, f'case {i}: config overwritten'
        assert config.stat().st_mtime_ns == before.st_mtime_ns, f'case {i}: config rewritten'
        assert config.stat().st_ino == before.st_ino, f'case {i}: original config replaced'
        assert set(config.parent.iterdir()) == {config}, f'case {i}: unexpected saved files'
print('PASS: real toggles preserve malformed, over-cap, oversized, unreadable and invalid panel config files')

# A real SaveConfig must migrate four, six, nine or ten choices (7-9, 0 and H start hidden), then reload all eleven.
F, T = False, True
for initial in [[F, T, F, T], [F, T, F, T, T, T], [F, T, F, T, T, T, F, F, F], [F, T, F, T, T, T, F, F, F, F], [F, T, F, T, T, T, F, F, F, F, F]]:
    with tempfile.TemporaryDirectory(prefix='taskr-config-migration-') as temp:
        root = pathlib.Path(temp)
        config = root / 'config/taskr/tui.json'
        config.parent.mkdir(parents=True)
        config.write_text(json.dumps({'panels': initial, 'repos': ['kept/private']}))
        tools = root / 'bin'; tools.mkdir()
        for name, body in [('taskr', "printf '{}\\n'"), ('gh', "printf '[]\\n'")]:
            tool = tools / name; tool.write_text('#!/bin/sh\n' + body + '\n'); tool.chmod(0o700)
        env = os.environ.copy()
        env.update(HOME=temp, XDG_CONFIG_HOME=str(root / 'config'), PATH=str(tools), TASKR_BIN=str(tools / 'taskr'), TERM='xterm-256color')
        env.pop('NO_COLOR', None)
        for key, expected in [(b'5', [F, T, F, T, F, T, F, F, F, F, F]), (b'6', [F, T, F, T, F, F, F, F, F, F, F]), (b'8', [F, T, F, T, F, F, F, T, F, F, F]), (b'0', [F, T, F, T, F, F, F, T, F, T, F]), (b'H', [F, T, F, T, F, F, F, T, F, T, T])]:
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
            child = subprocess.Popen([sys.argv[1], '--theme', 'dark'], stdin=slave, stdout=slave, stderr=slave, env=env, start_new_session=True)
            os.close(slave)
            output = bytearray(); replies = [0]
            def until(predicate):
                deadline = time.monotonic() + 5
                while not predicate() and time.monotonic() < deadline:
                    if not select.select([master], [], [], 0.1)[0]: continue
                    try: chunk = os.read(master, 65536)
                    except OSError:
                        # PTY EOF can precede the child's exit; reap before checking the predicate.
                        child.wait(timeout=5)
                        break
                    if not chunk:
                        child.wait(timeout=5)
                        break
                    output.extend(chunk)
                    while replies[0] < output.count(b'\x1b[6n'):
                        os.write(master, b'\x1b[1;1R'); replies[0] += 1
                assert predicate(), (child.poll(), repr(bytes(output[-2000:])))
            try:
                until(lambda: b'4 PRs+' in output and b'5 Shipped' in output)
                assert b'using defaults' not in output
                if key == b'6': assert b'5 Shipped-' in output, 'six booleans did not survive restart'
                if key == b'8': assert b'6 Build-' in output and b'8 Fleet-' in output, 'nine booleans did not survive restart'
                if key == b'0': assert b'8 Fleet+' in output and b'0 Issues-' in output, 'ten booleans did not survive restart'
                if key == b'H': assert b'0 Issues+' in output and b'H Health-' in output, 'eleven booleans did not survive restart'
                os.write(master, key + b'\x0c')
                until(lambda: json.loads(config.read_text())['panels'] == expected)
                os.write(master, b'q')
                until(lambda: child.poll() is not None)
                assert child.wait(timeout=5) == 0
            finally:
                if child.poll() is None: child.kill(); child.wait()
                os.close(master)
            assert json.loads(config.read_text()) == {'panels': expected, 'repos': ['kept/private'], 'issues_repo': None, 'health_repo': None, 'health_root': None}
            assert config.stat().st_mode & 0o777 == 0o600
            assert set(config.parent.iterdir()) == {config}
print('PASS: real four/six/nine/ten/eleven config saves and restarts retain choices, repos and 0600 permissions')
