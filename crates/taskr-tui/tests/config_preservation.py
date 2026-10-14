"""Real keyboard toggles must never replace a config that failed to load."""
import fcntl, json, os, pathlib, pty, select, struct, subprocess, sys, tempfile, termios, time

cases = [b'{bad json', json.dumps({'repos': [f'owner/repo{i}' for i in range(9)]}).encode(), b' ' * 16385]
if os.geteuid() != 0:
    cases.append(b'{"repos":["private/kept-unreadable"]}')
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
            until(lambda: b'4 PRs-' in output[toggled_at:] and b'using defaults' in output[toggled_at:])
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
print('PASS: real toggles preserve malformed, over-cap, oversized and unreadable config files')
