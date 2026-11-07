#!/usr/bin/env python3
"""Measure the CPU time a Porch proxy spends per request; Unix only.

  python3 scripts/proxy_cpu.py BASELINE_PORCH CANDIDATE_PORCH [--rounds 2]

Each binary gets a throwaway state directory, an HTTP proxy on a free port and
a local backend. It then measures the proxy process's user+system CPU time for
(a) keep-alive HTTP requests and (b) new HTTPS connections, each a full TLS
handshake. CPU time, unlike requests per second, does not depend on how fast
the load generator or the backend is, so a size-optimized build that is slower
shows up directly. Binaries alternate between rounds to spread machine noise.
Nothing outside the temporary directories is touched; `--https` uses
`--skip-trust`, so no CA is trusted.
"""
import argparse
import http.client
import http.server
import os
from pathlib import Path
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time

BODY = b'x' * 1024


class Backend(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def do_GET(self):
        self.send_response(200)
        self.send_header('content-length', str(len(BODY)))
        self.end_headers()
        self.wfile.write(BODY)

    def log_message(self, *args):
        pass


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def cpu_seconds(pid):
    """User+system CPU time of a process: /proc on Linux, else `ps` (macOS)."""
    stat = Path(f'/proc/{pid}/stat')
    if stat.exists():
        fields = stat.read_text().rsplit(')', 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / os.sysconf('SC_CLK_TCK')
    text =subprocess.run(['ps', '-o', 'time=', '-p', str(pid)], capture_output=True, text=True, check=True).stdout
    total = 0.0
    for part in text.strip().replace('-', ':').split(':'):
        total = total * 60 + float(part)
    return total


def run_threads(count, work):
    errors = []

    def guarded():
        try:
            work()
        except Exception as error:  # reported below; a failed run must not look fast
            errors.append(error)
    threads = [threading.Thread(target=guarded) for _ in range(count)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    if errors:
        raise RuntimeError(f'{len(errors)} load threads failed: {errors[0]!r}')


def keep_alive_load(port, requests, threads=8):
    def work():
        connection = http.client.HTTPConnection('127.0.0.1', port)
        for _ in range(requests // threads):
            connection.request('GET', '/', headers={'Host': f'bench.localhost:{port}'})
            response = connection.getresponse()
            if response.status != 200 or len(response.read()) != len(BODY):
                raise RuntimeError(f'unexpected response {response.status}')
        connection.close()
    run_threads(threads, work)


def tls_load(port, connections, threads=8):
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE  # throwaway CA in a temporary state directory

    def work():
        for _ in range(connections // threads):
            with socket.create_connection(('127.0.0.1', port)) as raw, \
                    context.wrap_socket(raw, server_hostname='bench.localhost') as stream:
                stream.sendall(f'GET / HTTP/1.1\r\nHost: bench.localhost:{port}\r\nConnection: close\r\n\r\n'.encode())
                data = b''
                while chunk := stream.recv(65536):
                    data += chunk
                if not data.startswith(b'HTTP/1.1 200'):
                    raise RuntimeError(f'unexpected response {data[:40]!r}')
    run_threads(threads, work)


def measure(binary, backend_port, https, amount, repeats):
    with tempfile.TemporaryDirectory(prefix='porch-cpu-') as state:
        # Ignore the caller's Porch/Portless configuration and never touch the
        # shared hosts file: only the temporary state directory is used.
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(('PORCH_', 'PORTLESS_'))}
        env.update(PORCH_NO_UPDATE_CHECK='1', PORCH_SYNC_HOSTS='0', PORTLESS_SYNC_HOSTS='0')
        port = free_port()
        porch = [str(binary), '--state-dir', state]
        mode = ['--https', '--skip-trust'] if https else ['--no-tls']
        subprocess.run(porch + ['proxy', 'start', '--port', str(port)] + mode, env=env, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            subprocess.run(porch + ['alias', 'bench', str(backend_port)], env=env, check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            pid = int((Path(state) / 'proxy.ready').read_text().strip())
            load = tls_load if https else keep_alive_load
            load(port, max(amount // 10, 8))  # warm up
            samples = []
            for _ in range(repeats):
                before = cpu_seconds(pid)
                load(port, amount)
                samples.append(cpu_seconds(pid) - before)
            return samples
        finally:
            subprocess.run(porch + ['proxy', 'stop'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('binaries', type=Path, nargs='+', help='porch executables to compare')
    parser.add_argument('--rounds', type=int, default=2)
    parser.add_argument('--requests', type=int, default=40000, help='keep-alive HTTP requests per sample')
    parser.add_argument('--verbose', action='store_true', help='also print every sample')
    parser.add_argument('--handshakes', type=int, default=2000, help='HTTPS connections per sample')
    args = parser.parse_args(argv)
    if os.name == 'nt':
        parser.exit(1, 'proxy_cpu: Unix only (uses ps)\n')
    http.server.ThreadingHTTPServer.request_queue_size = 256
    backend = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Backend)
    threading.Thread(target=backend.serve_forever, daemon=True).start()
    results = {binary: {'http': [], 'https': []} for binary in args.binaries}
    for _ in range(args.rounds):
        for binary in args.binaries:
            results[binary]['http'] += measure(binary, backend.server_port, False, args.requests, 3)
            results[binary]['https'] += measure(binary, backend.server_port, True, args.handshakes, 3)
    print(f'| Binary | Proxy CPU s / {args.requests:,} keep-alive HTTP requests (median) | '
          f'Proxy CPU s / {args.handshakes:,} HTTPS handshakes (median) | Samples |')
    print('| --- | ---: | ---: | ---: |')
    for binary, data in results.items():
        median = {key: sorted(values)[len(values) // 2] for key, values in data.items()}
        print(f'| `{binary}` | {median["http"]:.2f} | {median["https"]:.2f} | {len(data["http"])} each |')
        if args.verbose:
            print(f'<!-- {binary}: http {data["http"]} https {data["https"]} -->')
    sys.stdout.flush()


if __name__ == '__main__':
    main()
