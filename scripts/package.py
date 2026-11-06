#!/usr/bin/env python3
"""Build private CLI + native tray archives; never publish or install."""
import argparse
from pathlib import Path
import plistlib
import shutil
import struct
import sys
import tarfile
import tempfile
import zlib

ROOT = Path(__file__).resolve().parent.parent


def icon_png(size):
    raw = bytearray()
    for y in range(size):
        raw.append(0)
        for x in range(size):
            nx, ny = x * 32 / size, y * 32 / size
            roof = 5 <= nx <= 27 and abs(ny - (6 + abs(nx - 16) / 2)) <= 1
            posts = (8 <= nx <= 10 or 22 <= nx <= 24) and 15 <= ny <= 26
            step = 6 <= nx <= 26 and 26 <= ny <= 28
            door = ((14 <= nx <= 15 or 19 <= nx <= 20) and 18 <= ny <= 25 or 14 <= nx <= 20 and 17 <= ny <= 18)
            raw.extend((53, 139, 190, 255) if roof or posts or step or door else (0, 0, 0, 0))
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data) & 0xffffffff)
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', size, size, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b'')


def bundle(destination, binaries, version):
    contents = destination / 'Contents'
    (contents / 'MacOS').mkdir(parents=True)
    (contents / 'Resources').mkdir()
    for name in ['porch', 'porch-tray']:
        shutil.copy2(binaries / name, contents / 'MacOS' / name)
    metadata = {'CFBundleName': 'Porch', 'CFBundleDisplayName': 'Porch',
                'CFBundleIdentifier': 'dev.porch.tray', 'CFBundleExecutable': 'porch-tray',
                'CFBundlePackageType': 'APPL', 'CFBundleShortVersionString': version,
                'CFBundleVersion': version, 'CFBundleIconFile': 'Porch.icns',
                'LSUIElement': True, 'NSHighResolutionCapable': True,
                'LSMinimumSystemVersion': '12.0'}
    with open(contents / 'Info.plist', 'wb') as output:
        plistlib.dump(metadata, output)
    icon = icon_png(256)
    block = b'ic08' + struct.pack('>I', len(icon) + 8) + icon
    (contents / 'Resources/Porch.icns').write_bytes(b'icns' + struct.pack('>I', len(block) + 8) + block)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binaries', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--output', type=Path, default=ROOT / 'dist')
    parser.add_argument('--target', required=True, choices=['aarch64-apple-darwin', 'x86_64-unknown-linux-gnu'])
    args = parser.parse_args()
    version = next(line.split('"')[1] for line in (ROOT / 'Cargo.toml').read_text().splitlines() if line.startswith('version ='))
    for name in ['porch', 'porch-tray']:
        if not (args.binaries / name).is_file():
            parser.error(f'missing {args.binaries / name}; build both binaries with --features tray')
    args.output.mkdir(parents=True, exist_ok=True)
    name = f'porch-{version}-{args.target}'
    with tempfile.TemporaryDirectory(prefix='porch-package-') as temporary:
        package = Path(temporary) / name
        package.mkdir()
        for executable in ['porch', 'porch-tray']:
            shutil.copy2(args.binaries / executable, package / executable)
        for script in ['install.py', 'porch-login']:
            shutil.copy2(ROOT / 'scripts' / script, package / script)
        shutil.copy2(ROOT / 'packaging/README.md', package / 'README.md')
        shutil.copytree(ROOT / 'packaging', package / 'packaging')
        (package / 'docs').mkdir()
        for document in ['discovery.md', 'discovery-verification.md', 'reference.md']:
            source = ROOT / 'docs' / document
            if source.exists():
                shutil.copy2(source, package / 'docs' / document)
        if 'apple' in args.target:
            bundle(package / 'Porch.app', args.binaries, version)
        archive = args.output / f'{name}.tar.gz'
        with tarfile.open(archive, 'w:gz') as tar:
            tar.add(package, arcname=name)
        if 'apple' in args.target:
            app = args.output / 'Porch.app'
            if app.exists(): shutil.rmtree(app)
            shutil.copytree(package / 'Porch.app', app)
        print(archive)


if __name__ == '__main__':
    main()
