#!/usr/bin/env python3
"""Build the porch-cli npm packages from release archives; never publish.

  python3 scripts/build_npm.py --version 0.5.0 --dist dist --output npm-dist

Writes npm-dist/porch-cli (the launcher) and one npm-dist/porch-cli-<os>-<cpu>
package per platform, each holding only that platform's `porch` binary.
"""
import argparse
import json
import shutil
import sys
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# npm package suffix -> (release target, npm os, npm cpu, executable name)
PLATFORMS = {
    'darwin-arm64': ('aarch64-apple-darwin', 'darwin', 'arm64', 'porch'),
    'darwin-x64': ('x86_64-apple-darwin', 'darwin', 'x64', 'porch'),
    # The static builds run on every distribution, glibc or musl.
    'linux-x64': ('x86_64-unknown-linux-musl', 'linux', 'x64', 'porch'),
    'linux-arm64': ('aarch64-unknown-linux-musl', 'linux', 'arm64', 'porch'),
    'win32-x64': ('x86_64-pc-windows-msvc', 'win32', 'x64', 'porch.exe'),
}


def extract_binary(archive, member_name, destination):
    with tarfile.open(archive) as tar:
        for member in tar.getmembers():
            parts = Path(member.name).parts
            if len(parts) == 2 and parts[1] == member_name and member.isfile():
                source = tar.extractfile(member)
                destination.write_bytes(source.read())
                destination.chmod(0o755)
                return
    raise SystemExit(f'{archive} has no {member_name}')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--version', required=True)
    parser.add_argument('--dist', type=Path, required=True, help='directory with porch-VERSION-TARGET.tar.gz')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        shutil.rmtree(args.output)
    launcher = json.loads((ROOT / 'npm/porch-cli/package.json').read_text())
    launcher['version'] = args.version
    for name in launcher['optionalDependencies']:
        launcher['optionalDependencies'][name] = args.version
    out = args.output / 'porch-cli'
    shutil.copytree(ROOT / 'npm/porch-cli', out)
    (out / 'package.json').write_text(json.dumps(launcher, indent=2) + '\n')
    for suffix, (target, os_name, cpu, exe) in PLATFORMS.items():
        name = f'porch-cli-{suffix}'
        package = args.output / name
        package.mkdir(parents=True)
        archive = args.dist / f'porch-{args.version}-{target}.tar.gz'
        if not archive.is_file():
            raise SystemExit(f'missing {archive}')
        extract_binary(archive, exe, package / exe)
        (package / 'package.json').write_text(json.dumps({
            'name': name,
            'version': args.version,
            'description': f'The Porch binary for {os_name} {cpu}. Install porch-cli instead.',
            'license': 'MIT',
            'repository': launcher['repository'],
            'os': [os_name],
            'cpu': [cpu],
            'files': [exe],
            'preferUnplugged': True,
        }, indent=2) + '\n')
        shutil.copy2(ROOT / 'LICENSE', package / 'LICENSE')
        print(package)
    print(out)


if __name__ == '__main__':
    sys.exit(main())
