#!/usr/bin/env python3
"""Render the Homebrew formula for a Porch release from its SHA256SUMS file.

Usage:
  python3 scripts/render_formula.py --version 0.3.0 --sums dist/SHA256SUMS \
      --output Formula/porch.rb

The output is written atomically; nothing is published, committed, or pushed.
"""
import argparse
import os
from pathlib import Path
import re
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
TEMPLATE = ROOT / 'packaging/homebrew/porch.rb'
DEFAULT_REPO = 'akshitkrnagpal/porch'
# Every target the formula references. Each must be present in SHA256SUMS.
TARGETS = ['aarch64-apple-darwin', 'x86_64-apple-darwin', 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-musl']
VERSION = re.compile(r'^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$')


def parse_sums(text):
    """Parse `sha256sum` output (`<hex>  <name>` or `<hex> *<name>`)."""
    sums = {}
    for number, line in enumerate(text.splitlines(), 1):
        line = line.strip()
        if not line or line.startswith('#'):
            continue
        parts = line.split(None, 1)
        if len(parts) != 2 or not re.fullmatch(r'[0-9a-fA-F]{64}', parts[0]):
            raise ValueError(f'SHA256SUMS line {number} is malformed: {line!r}')
        name = parts[1].strip().lstrip('*')
        if name in sums and sums[name] != parts[0].lower():
            raise ValueError(f'SHA256SUMS lists {name} twice with different digests')
        sums[name] = parts[0].lower()
    return sums


def render(template, version, sums, repo=DEFAULT_REPO):
    if version.startswith('v'):
        version = version[1:]
    if not VERSION.match(version):
        raise ValueError(f'invalid version {version!r}')
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repo):
        raise ValueError(f'invalid repository {repo!r}')
    values = {'@VERSION@': version, '@REPO@': repo}
    for target in TARGETS:
        asset = f'porch-{version}-{target}.tar.gz'
        if asset not in sums:
            raise ValueError(f'SHA256SUMS has no entry for {asset}')
        values[f'@SHA256_{target.upper().replace("-", "_")}@'] = sums[asset]
    output = template
    for token, value in values.items():
        output = output.replace(token, value)
    leftover = re.findall(r'@[A-Z0-9_]+@', output)
    if leftover:
        raise ValueError(f'unrendered template tokens: {sorted(set(leftover))}')
    return output


def write_atomic(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=f'.{path.name}.', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            stream.write(text)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o644)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--version', required=True, help='release version, with or without a leading v')
    parser.add_argument('--sums', type=Path, required=True, help='SHA256SUMS file from the release')
    parser.add_argument('--repo', default=DEFAULT_REPO, help=f'GitHub repository (default {DEFAULT_REPO})')
    parser.add_argument('--template', type=Path, default=TEMPLATE)
    parser.add_argument('--output', type=Path, help='destination; default stdout')
    args = parser.parse_args(argv)
    try:
        formula = render(args.template.read_text(), args.version, parse_sums(args.sums.read_text()), args.repo)
    except (OSError, ValueError) as error:
        parser.exit(1, f'render_formula: {error}\n')
    if args.output:
        write_atomic(args.output, formula)
        print(f'Wrote {args.output}', file=sys.stderr)
    else:
        sys.stdout.write(formula)


if __name__ == '__main__':
    main()
