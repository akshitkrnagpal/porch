#!/usr/bin/env python3
"""Build CLI + native tray release archives; never publish or install."""
import argparse
import os
from pathlib import Path
import plistlib
import shutil
import struct
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def icns():
    """Porch.icns from the committed app tile PNGs (see assets/icon/gen.py)."""
    blocks = b''
    for kind, size in ((b'ic08', 256), (b'ic09', 512), (b'ic10', 1024)):
        png = (ROOT / f'assets/icon/app-tile-{size}.png').read_bytes()
        blocks += kind + struct.pack('>I', len(png) + 8) + png
    return b'icns' + struct.pack('>I', len(blocks) + 8) + blocks


def bundle(destination, binaries, version, link=False):
    """Write Porch.app. With link=True the executables are hard links to the
    files in `binaries`, so the release tarball stores each payload once."""
    contents = destination / 'Contents'
    (contents / 'MacOS').mkdir(parents=True)
    (contents / 'Resources').mkdir()
    for name in ['porch', 'porch-tray']:
        if link:
            os.link(binaries / name, contents / 'MacOS' / name)
        else:
            shutil.copy2(binaries / name, contents / 'MacOS' / name)
    metadata = {'CFBundleName': 'Porch', 'CFBundleDisplayName': 'Porch',
                'CFBundleIdentifier': 'dev.porch.tray', 'CFBundleExecutable': 'porch-tray',
                'CFBundlePackageType': 'APPL', 'CFBundleShortVersionString': version,
                'CFBundleVersion': version, 'CFBundleIconFile': 'Porch.icns',
                'LSUIElement': True, 'NSHighResolutionCapable': True,
                'LSMinimumSystemVersion': '12.0'}
    with open(contents / 'Info.plist', 'wb') as output:
        plistlib.dump(metadata, output)
    (contents / 'Resources/Porch.icns').write_bytes(icns())


def write_archive(archive, package, name, first):
    """Tar `package` as `name/`. The root executables are added first, so they
    are the regular-file entries and any hard links in Porch.app refer to them."""
    first = [package / entry for entry in first]
    with tarfile.open(archive, 'w:gz') as tar:
        tar.add(package, arcname=name, recursive=False)
        for path in first + sorted(p for p in package.iterdir() if p not in first):
            tar.add(path, arcname=f'{name}/{path.name}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binaries', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--output', type=Path, default=ROOT / 'dist')
    parser.add_argument('--target', required=True, choices=['aarch64-apple-darwin', 'x86_64-apple-darwin', 'x86_64-unknown-linux-gnu', 'x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl', 'x86_64-pc-windows-msvc'])
    args = parser.parse_args()
    version = next(line.split('"')[1] for line in (ROOT / 'Cargo.toml').read_text().splitlines() if line.startswith('version ='))
    windows = 'windows' in args.target
    # The static musl builds are CLI-only: the tray needs the system GTK.
    cli_only = 'musl' in args.target
    executables = [name + ('.exe' if windows else '') for name in ['porch'] + ([] if cli_only else ['porch-tray'])]
    for name in executables:
        if not (args.binaries / name).is_file():
            parser.error(f'missing {args.binaries / name}; build both binaries with --features tray')
    args.output.mkdir(parents=True, exist_ok=True)
    name = f'porch-{version}-{args.target}'
    with tempfile.TemporaryDirectory(prefix='porch-package-') as temporary:
        package = Path(temporary) / name
        package.mkdir()
        for executable in executables:
            shutil.copy2(args.binaries / executable, package / executable)
        # porch-login drives launchd/systemd; Windows has no equivalent yet.
        for script in ['install.py'] + ([] if windows or cli_only else ['porch-login']):
            shutil.copy2(ROOT / 'scripts' / script, package / script)
        shutil.copy2(ROOT / 'packaging/README.md', package / 'README.md')
        shutil.copy2(ROOT / 'LICENSE', package / 'LICENSE')
        shutil.copytree(ROOT / 'packaging', package / 'packaging')
        (package / 'docs').mkdir()
        for document in ['discovery.md', 'reference.md', 'troubleshooting.md']:
            source = ROOT / 'docs' / document
            if source.exists():
                shutil.copy2(source, package / 'docs' / document)
        if 'apple' in args.target:
            # The app runs the same porch and porch-tray as the archive root.
            # Hard links make tar store each executable once; every extractor
            # (tar, Homebrew, install.py, porch self-update) still writes both
            # paths as ordinary files.
            bundle(package / 'Porch.app', package, version, link=True)
        archive = args.output / f'{name}.tar.gz'
        write_archive(archive, package, name, executables)
        if 'apple' in args.target:
            app = args.output / 'Porch.app'
            if app.exists(): shutil.rmtree(app)
            shutil.copytree(package / 'Porch.app', app)
        print(archive)


if __name__ == '__main__':
    main()
