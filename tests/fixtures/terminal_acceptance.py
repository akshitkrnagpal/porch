"""Real controlling-PTY acceptance. Only temporary state and owned processes."""
import errno
import fcntl
import json
import os
import pathlib
import pty
import select
import shutil
import signal
import socket
import subprocess
import struct
import sys
import tempfile
import termios
import time

BINARY = str(pathlib.Path(sys.argv[1]).resolve())


# Private port slots claimed by tests/pty.rs (see tests/common/mod.rs), one per
# Fixture. Ephemeral ports would be released before Porch binds them, and a
# concurrent test could take them in between.
SLOTS = [int(port) for port in os.environ.get('PORCH_TEST_SLOTS', '').split(',') if port]
SLOT_SIZE, APP_OFFSET = 256, 224


def claim_slot():
    assert SLOTS, 'more fixtures than PORCH_TEST_SLOTS; raise FIXTURES in tests/pty.rs'
    return SLOTS.pop(0)


def stopped_in_tree(root):
    """Processes under `root` stopped by job control (state T): the hazard
    these tests exist to catch, which would otherwise look like a hang."""
    table = subprocess.run(['ps', '-axo', 'pid=,ppid=,stat='],
                           capture_output=True, text=True).stdout
    children, states = {}, {}
    for line in table.splitlines():
        pid, ppid, stat = line.split(None, 2)
        children.setdefault(int(ppid), []).append(int(pid))
        states[int(pid)] = stat
    found, todo = [], [root]
    while todo:
        pid = todo.pop()
        if states.get(pid, '').startswith('T'):
            found.append(pid)
        todo.extend(children.get(pid, []))
    return found


def alive(pid):
    status = subprocess.run(['ps', '-p', str(pid), '-o', 'stat='],
                            capture_output=True, text=True).stdout.strip()
    return bool(status) and not status.startswith('Z')


class Fixture:
    def __init__(self):
        self.temp = tempfile.TemporaryDirectory(prefix='porch-pty-')
        self.root = pathlib.Path(self.temp.name)
        self.env = os.environ.copy()
        for name in list(self.env):
            if name.startswith(('PORCH_', 'PORTLESS_')) or name in ('PORCH', 'PORTLESS'):
                self.env.pop(name)
        slot = claim_slot()
        self.env.update(PORCH_STATE_DIR=str(self.root / 'state'),
                        PORCH_PORT=str(slot), PORCH_HTTPS='0',
                        PORCH_APP_PORTS=f'{slot + APP_OFFSET}-{slot + SLOT_SIZE - 1}',
                        PORCH_SYNC_HOSTS='0', PORCH_SKIP_TRUST='1',
                        PORCH_PREFIX='', PORCH_CLEAN_URLS='0', TERM='xterm-256color')
        self.pid = None
        self.fd = None
        self.status = None
        self.output = b''

    def start(self, args):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
            os.chdir(self.root)
            os.execve(BINARY, [BINARY] + args, self.env)
        os.set_blocking(self.fd, False)

    def read(self):
        if self.fd is not None and select.select([self.fd], [], [], .05)[0]:
            try:
                self.output += os.read(self.fd, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
        if self.status is None:
            waited, status = os.waitpid(self.pid, os.WNOHANG)
            if waited:
                self.status = os.waitstatus_to_exitcode(status)

    # Waits end on what they wait for, or on a stopped or exited process; the
    # timeouts are watchdogs for a broken run that a passing run never reaches.
    def wait_for(self, marker, timeout=60):
        deadline = time.monotonic() + timeout
        while marker.encode() not in self.output and time.monotonic() < deadline:
            self.read()
            if self.status is not None:
                break
            stopped = stopped_in_tree(self.pid)
            assert not stopped, f'stopped by the terminal: {stopped}; output={self.output!r}'
        assert marker.encode() in self.output, (f'missing {marker}; exit={self.status}; '
                                                f'output={self.output.decode(errors="replace")}')

    def wait(self, expected=0):
        deadline = time.monotonic() + 60
        while self.status is None and time.monotonic() < deadline:
            self.read()
            if self.status is None:
                stopped = stopped_in_tree(self.pid)
                assert not stopped, f'stopped by the terminal: {stopped}; output={self.output!r}'
        assert self.status == expected, f'exit={self.status}; output={self.output!r}'

    def close(self):
        if self.pid and self.status is None:
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(self.pid, 0)
        # Failure cleanup of the dead launcher's apps goes through Porch, which
        # signals a recorded process group only if its identity still matches:
        # a raw killpg of a recorded id could reach a process that reused it.
        subprocess.run([BINARY, 'prune', '--force'], env=self.env, cwd=self.root,
                       capture_output=True, timeout=60)
        subprocess.run([BINARY, 'proxy', 'stop'], env=self.env, cwd=self.root,
                       capture_output=True, timeout=60)
        if self.fd is not None:
            os.close(self.fd)
        self.temp.cleanup()


def case(name, args):
    fixture = Fixture()
    try:
        fixture.start(args)
        fixture.wait_for('PTY_OK')
        fixture.wait()
        print(f'PASS {name}', flush=True)
    finally:
        fixture.close()


case('stty raw inherited terminal',
     ['ttytest', 'sh', '-c', 'stty raw -echo && printf PTY_OK && stty sane'])
node = shutil.which('node')
assert node, 'PTY acceptance requires Node.js for setRawMode regression'
case('Node setRawMode inherited terminal',
     ['rawtest', node, '-e', "process.stdin.setRawMode(true); console.log('PTY_OK'); process.stdin.setRawMode(false)"])

# Verify the foreground Porch process receives Ctrl-C, forwards it into its
# detached child group, and cleans up descendants on both SIGINT and SIGTERM.
for sig in [signal.SIGINT, signal.SIGTERM]:
    fixture = Fixture()
    try:
        (fixture.root / 'signals.py').write_text('''import os, pathlib, signal, time
child = os.fork()
if child == 0:
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    while True: time.sleep(.1)
pathlib.Path('descendant.pid').write_text(str(child))
def stop(sig, frame):
    pathlib.Path('signal.txt').write_text(str(sig))
    raise SystemExit(0)
signal.signal(signal.SIGINT, stop)
signal.signal(signal.SIGTERM, stop)
print('PTY_OK', flush=True)
while True: time.sleep(.1)
''')
        fixture.start(['signals', 'python3', 'signals.py'])
        fixture.wait_for('PTY_OK')
        descendant = int((fixture.root / 'descendant.pid').read_text())
        if sig == signal.SIGINT:
            os.write(fixture.fd, b'\x03')
        else:
            os.kill(fixture.pid, sig)
        fixture.wait(128 + sig)
        assert (fixture.root / 'signal.txt').read_text() == str(sig), 'signal not forwarded'
        # Porch killed the group before exiting; this only waits for the kernel.
        deadline = time.monotonic() + 60
        while alive(descendant) and time.monotonic() < deadline:
            time.sleep(.02)
        assert not alive(descendant), f'descendant {descendant} survived {sig}'
        print(f'PASS signal {sig} forwarding and descendant cleanup', flush=True)
    finally:
        fixture.close()

# A real Turbo TUI exercises the coordinator's separate process-launch path.
# CI installs the pinned binary and makes absence a failure; local users may
# run the core PTY cases without downloading a package in their test suite.
turbo = os.environ.get('PORCH_TEST_TURBO') or shutil.which('turbo')
if turbo:
    version = subprocess.run([turbo, '--version'], capture_output=True, text=True).stdout.strip()
    if not version.startswith('2.'):
        turbo = None
if not turbo:
    assert not os.environ.get('PORCH_TEST_TURBO_REQUIRED'), 'Turbo 2 required for TUI acceptance'
    print('SKIP real Turbo TUI (install Turbo 2 or set PORCH_TEST_TURBO)', flush=True)
else:
    fixture = Fixture()
    try:
        fixture.env.pop('CI', None)
        fixture.env['TURBO_TELEMETRY_DISABLED'] = '1'
        (fixture.root / 'package.json').write_text(json.dumps({
            'name': 'pty-workspace', 'private': True, 'packageManager': 'npm@10.0.0',
            'workspaces': ['apps/*'], 'scripts': {'dev': f'"{turbo}" run dev --env-mode=loose'}}))
        (fixture.root / 'turbo.json').write_text(json.dumps({
            'ui': 'tui', 'tasks': {'dev': {'persistent': True, 'cache': False}}}))
        packages = {'': {'name': 'pty-workspace', 'workspaces': ['apps/*']}}
        for app in ['web', 'api']:
            path = fixture.root / 'apps' / app
            path.mkdir(parents=True)
            (path / 'package.json').write_text(json.dumps({
                'name': app, 'version': '1.0.0', 'scripts': {'dev': 'node server.cjs'}}))
            (path / 'server.cjs').write_text("const fs=require('fs');require('http').createServer((q,s)=>s.end('" + app + "')).listen(Number(process.env.PORT),'127.0.0.1',()=>fs.writeFileSync('ready',process.env.PORT));")
            packages['apps/' + app] = {'name': app, 'version': '1.0.0'}
            packages['node_modules/' + app] = {'resolved': 'apps/' + app, 'link': True}
        (fixture.root / 'package-lock.json').write_text(json.dumps({
            'name': 'pty-workspace', 'lockfileVersion': 3, 'packages': packages}))
        fixture.start([])
        # Fail on what this guards against, a terminal stop in Turbo, npm or its
        # git probe, as soon as it shows; a slow start is not a failure. The
        # deadline is a watchdog for a broken run.
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            fixture.read()
            if all((fixture.root / 'apps' / app / 'ready').exists() for app in ['web', 'api']):
                break
            assert fixture.status is None, f'Turbo exited: {fixture.output!r}'
            stopped = stopped_in_tree(fixture.pid)
            assert not stopped, f'stopped by the terminal: {stopped}; {fixture.output!r}'
        for app in ['web', 'api']:
            ready = fixture.root / 'apps' / app / 'ready'
            assert ready.exists(), f'Turbo {app} stopped before ready: {fixture.output!r}'
            with socket.create_connection(('127.0.0.1', int(fixture.env['PORCH_PORT'])), timeout=3) as conn:
                conn.sendall(f'GET / HTTP/1.1\r\nHost: {app}.pty-workspace.localhost\r\nConnection: close\r\n\r\n'.encode())
                response = b''
                while True:
                    chunk = conn.recv(65536)
                    if not chunk:
                        break
                    response += chunk
                assert b'200 OK' in response and response.endswith(app.encode()), response
        os.kill(fixture.pid, signal.SIGTERM)
        fixture.wait(143)
        assert b'\x1b[' in fixture.output, f'Turbo did not render TUI: {fixture.output!r}'
        print('PASS real Turbo TUI workspace and HTTP routes', flush=True)
    finally:
        fixture.close()
