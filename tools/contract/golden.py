"""Shared record/expect support; the fixtures contain synthetic producer observations."""
import hashlib
import gzip
import http.client
import http.server
import json
from pathlib import Path
import re
import socket
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[2]
DIRECTORY = ROOT / 'testdata/contract/golden'
TEMP_PATH = re.compile('(?:' + '|'.join(re.escape(str(base)) for base in (ROOT / '.scratch', (ROOT / '.scratch').resolve(), Path(tempfile.gettempdir()), Path(tempfile.gettempdir()).resolve()))
                       + r')/taskr-[A-Za-z0-9_-]+(?=/|$)')
LOCAL_HOST = socket.gethostname().split('.')[0].lower()
session = None


def normalize(value):
    if isinstance(value, bytes):
        value = value.decode('utf-8', errors='surrogateescape')
    if isinstance(value, str):
        if session and session.name == 'run' and 'taskr-' not in value and LOCAL_HOST not in value:
            return value
        # Preserve all suffixes within a scratch tree and the production :7788.
        if session:
            for binary in (session.oracle, session.rust):
                value = value.replace(str(binary), '<binary>')
        value = TEMP_PATH.sub('<tmp>', value)
        if session:
            value = re.sub(r'http://\[::1\]:(\d+)',
                           lambda match: 'http://[::1]:<port>' if int(match[1]) in session.ports else match[0], value)
        if session:
            value = re.sub(r'http://(127\.0\.0\.1|hub\.example\.ts\.net):(\d+)',
                           lambda match: f'http://{match[1]}:<port>' if int(match[2]) in session.ports else match[0], value)
        if session and session.name in ('run', 'hub_child_context'):
            value = re.sub(r'("(?:server_host|source_host)"\s*:\s*")' + re.escape(LOCAL_HOST) + r'(")', r'\1<local-host>\2', value)
            value = re.sub(r'(caller is |task \d+ is on )' + re.escape(LOCAL_HOST) + r'(?=[",])', r'\1<local-host>', value)
        value = re.sub(r'("(?:pid|old_pid|new_pid)"\s*:\s*)[1-9]\d*', r'\1"<pid>"', value)
        value = re.sub(r'(lock names pid |ledger records daemon pid )([1-9]\d*)', r'\1<pid>', value)
        value = re.sub(r'pid [1-9]\d*(?= is not running `taskr daemon`)', 'pid <pid>', value)
        if session and session.name not in ('run', 'daemon_cell'):
            value = re.sub(r'("heartbeat_age_ms"\s*:\s*)-?\d+', r'\1"<age>"', value)
            value = re.sub(r'\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z',
                           lambda match: match[0] if match[0] in ('2026-10-08T00:00:00Z', '2026-10-08T00:00:00.000Z') else '<time>', value)
        if session:
            for binary in (session.oracle, session.rust):
                value = value.replace(str(binary), '<binary>')
        return value
    if isinstance(value, dict):
        return {key: '<pid>' if key in ('pid', 'old_pid', 'new_pid') and type(item) is int and item > 0
                else '<pid>' if key == 'daemon_pid' and str(item).isdigit() and int(item) > 0
                else '<pid>\n' if key == 'lock' and str(item).strip().isdigit() and int(str(item).strip()) > 0
                else '<proc-start>' if key in ('daemon_proc_start', 'start_time')
                else '<uid>' if key == 'uid' else normalize(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [normalize(item) for item in value]
    return value


def digest(value):
    return hashlib.sha256(json.dumps(normalize(value), sort_keys=True, ensure_ascii=True,
                                     separators=(',', ':')).encode()).hexdigest()


def parse(parser, file):
    global session
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument('--record', type=Path, help='record Go-compatible observations from BIN')
    modes.add_argument('--expect', type=Path, help='check the Rust binary against DIR, without Go')
    required = {action.dest for action in parser._actions if action.required}
    for action in parser._actions:
        if action.dest in ('go', 'rust', 'out'):
            action.required = False
    args = parser.parse_args()
    if not args.record and not args.expect:
        for name in required:
            if getattr(args, name, None) is None:
                parser.error(f'differential mode requires --{name}')
    if not getattr(args, 'rust', None):
        args.rust = ROOT / 'target/debug/taskr'
    args.rust = args.rust.resolve()
    if args.record or args.expect:
        if args.go:
            parser.error('--go cannot be combined with --record/--expect')
        oracle = args.record.resolve() if args.record else args.rust
        needs_rust = Path(file).stem != 'run' or not args.record
        if not oracle.is_file() or (needs_rust and not args.rust.is_file()):
            parser.error('record/expect requires explicit existing binaries (--rust defaults to target/debug/taskr)')
        args.go = oracle
        if not getattr(args, 'out', None):
            out = ROOT / '.scratch/golden-results'
            out.mkdir(parents=True, exist_ok=True)
            args.out = out / (Path(file).stem + '.json')
        session = Session(Path(file).stem, args.record is not None,
                          DIRECTORY if args.record else args.expect, oracle, args.rust)
    elif not getattr(args, 'go', None) and Path(file).stem not in ('hub_cell', 'net_cell'):
        parser.error('differential mode requires --go')
    return args


class Session:
    def __init__(self, name, recording, directory, oracle, rust):
        self.name, self.recording, self.directory = name, recording, directory
        self.oracle, self.rust = oracle, rust
        self.oracle_is_rust = rust.is_file() and oracle.samefile(rust)
        self.ports = set()
        self.observations = []
        self.expected = None
        self.require_all = True
        path = directory / (name + ('.json.gz' if name == 'run' else '.json'))
        if path.exists():
            raw = gzip.decompress(path.read_bytes()) if path.suffix == '.gz' else path.read_bytes()
            self.expected = json.loads(raw)['observations']
        self.by_label = {json.dumps(entry['label']): entry for entry in self.expected or []}


def observe(label, value, provenance=None):
    if session is None:
        return normalize(value)
    entry = normalize({'label': label, 'value': value})
    index = len(session.observations)
    expected = session.by_label.get(json.dumps(entry['label'])) if session.name == 'run' else session.expected[index] if session.expected and index < len(session.expected) else None
    unchanged = expected and digest({'label': expected['label'], 'value': expected['value']}) == digest(entry)
    if not session.recording:
        assert expected is not None, f'missing golden observation: {session.name}: {label}'
        assert unchanged, {'cell': session.name, 'index': index, 'expected': expected, 'actual': entry}
    if unchanged:
        entry['label'], entry['value'] = expected['label'], expected['value']
    source = provenance or ('production Go' if session.name in ('daemon_restart_cell', 'hub_child_context') else 'Go oracle')
    entry['provenance'] = expected.get('provenance', source) if unchanged and (session.oracle_is_rust or not session.recording) else 'Rust rerecord' if session.oracle_is_rust and session.recording else source
    session.observations.append(entry)
    return entry['value']


def replay(label, replacements):
    """Restore a frozen producer record with this fixture's volatile identities."""
    assert session and session.expected, f'missing producer fixture: {label}'
    entry = session.by_label[json.dumps(normalize(label))] if session.name == 'run' else session.expected[len(session.observations)-1]
    assert entry['label'] == normalize(label), 'replay must follow its observation'
    value = entry['value']
    def restore(item):
        if isinstance(item, str):
            if item in replacements:
                return replacements[item]
            for token, replacement in replacements.items():
                item = item.replace(token, str(replacement))
            return item
        if isinstance(item, dict):
            return {key: restore(field) for key, field in item.items()}
        if isinstance(item, list):
            return [restore(field) for field in item]
        return item
    return restore(value)


def finish():
    if session is None:
        return
    if session.recording and session.observations:
        session.directory.mkdir(parents=True, exist_ok=True)
        path = session.directory / (session.name + ('.json.gz' if session.name == 'run' else '.json'))
        lines = [json.dumps(entry, ensure_ascii=True, separators=(',', ':')) for entry in session.observations]
        raw = ('{\n"observations":[\n' + ',\n'.join(lines) + '\n]}\n').encode()
        pending = path.with_name(path.name + '.tmp')
        pending.write_bytes(gzip.compress(raw, mtime=0) if session.name == 'run' else raw)
        pending.replace(path)
    elif not session.recording and session.require_all:
        assert len(session.observations) == len(session.expected or []), 'golden observations were not all exercised'
    print(f'{session.name}: {len(session.observations)} golden observations; runtime assertions passed')


class LegacyProxy:
    """Expose the response fields recorded from Go, with current fixture values."""
    def __init__(self, upstream, fields):
        port = int(upstream.rsplit(':', 1)[1])

        class Server(http.server.ThreadingHTTPServer):
            address_family = socket.AF_INET6
            daemon_threads = True

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers['Content-Length']))
                request = json.loads(body)
                client = http.client.HTTPConnection('::1', port, timeout=75)
                try:
                    client.request('POST', self.path, body, {'Content-Type': 'application/json', 'X-Taskr-RPC': '1'})
                    reply = client.getresponse()
                    data = reply.read()
                    if '_host' in request['argv'] and 'observe' in request['argv'] and reply.status == 200:
                        envelope = json.loads(data)
                        value = json.loads(envelope['stdout'])
                        envelope['stdout'] = json.dumps({k: value[k] for k in fields}, sort_keys=True, separators=(',', ':')) + '\n'
                        data = json.dumps(envelope, separators=(',', ':')).encode()
                    self.send_response(reply.status)
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                finally:
                    client.close()

            def log_message(self, *_args):
                pass

        self.server = Server(('::1', 0), Handler)
        self.url = 'http://[::1]:' + str(self.server.server_address[1])
        if session: session.ports.add(self.server.server_address[1])
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
