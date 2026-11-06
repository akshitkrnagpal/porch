#!/usr/bin/env python3
"""Isolated installer/package checks; never write user startup directories."""
import importlib.util
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
sys.dont_write_bytecode = True


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


installer = load('porch_installer', ROOT / 'scripts/install.py')
packager = load('porch_packager', ROOT / 'scripts/package.py')


class PackageTests(unittest.TestCase):
    def test_atomic_install_keeps_old_open_inode_and_executable_mode(self):
        with tempfile.TemporaryDirectory(prefix='porch-install-test-') as temporary:
            root = Path(temporary)
            source, destination = root / 'new', root / 'bin/porch'
            destination.parent.mkdir()
            destination.write_bytes(b'old executable')
            source.write_bytes(b'new executable')
            with open(destination, 'rb') as running_inode:
                old_inode = os.fstat(running_inode.fileno()).st_ino
                installer.atomic_copy(source, destination)
                self.assertEqual(running_inode.read(), b'old executable')
                self.assertEqual(destination.read_bytes(), b'new executable')
                self.assertNotEqual(destination.stat().st_ino, old_inode)
                self.assertEqual(destination.stat().st_mode & 0o777, 0o755)
            self.assertEqual(sorted(path.name for path in destination.parent.iterdir()), ['porch'])

    def test_bundle_owns_cli_and_is_background_application(self):
        with tempfile.TemporaryDirectory(prefix='porch-bundle-test-') as temporary:
            root = Path(temporary)
            binaries = root / 'bin'
            binaries.mkdir()
            for name in ['porch', 'porch-tray']:
                (binaries / name).write_bytes(b'test binary')
                (binaries / name).chmod(0o755)
            bundle = root / 'Porch.app'
            packager.bundle(bundle, binaries, '0.2.0')
            with open(bundle / 'Contents/Info.plist', 'rb') as stream:
                metadata = plistlib.load(stream)
            self.assertTrue(metadata['LSUIElement'])
            self.assertEqual(metadata['CFBundleExecutable'], 'porch-tray')
            self.assertEqual(metadata['CFBundleIdentifier'], 'dev.porch.tray')
            self.assertTrue((bundle / 'Contents/MacOS/porch').is_file())
            self.assertEqual((bundle / 'Contents/Resources/Porch.icns').read_bytes()[:4], b'icns')
            foreign = root / 'Applications/Porch.app'
            (foreign / 'Contents').mkdir(parents=True)
            with open(foreign / 'Contents/Info.plist', 'wb') as stream:
                plistlib.dump({'CFBundleIdentifier': 'other.app'}, stream)
            with self.assertRaisesRegex(RuntimeError, 'unrelated app'):
                installer.install_app(bundle, foreign)
            with open(foreign / 'Contents/Info.plist', 'rb') as stream:
                self.assertEqual(plistlib.load(stream)['CFBundleIdentifier'], 'other.app')

    def test_package_is_complete_without_publishing_or_installing(self):
        with tempfile.TemporaryDirectory(prefix='porch-archive-test-') as temporary:
            root = Path(temporary)
            binaries, output = root / 'bin', root / 'out'
            binaries.mkdir()
            for name in ['porch', 'porch-tray']:
                (binaries / name).write_bytes(b'test binary')
                (binaries / name).chmod(0o755)
            subprocess.run([sys.executable, str(ROOT / 'scripts/package.py'), '--target', 'aarch64-apple-darwin',
                            '--binaries', str(binaries), '--output', str(output)], check=True, capture_output=True)
            archive, = output.glob('*.tar.gz')
            with tarfile.open(archive) as tar:
                names = tar.getnames()
                self.assertTrue(any(name.endswith('/Porch.app/Contents/MacOS/porch-tray') for name in names))
                self.assertTrue(any(name.endswith('/porch-login') for name in names))
                self.assertTrue(any(name.endswith('/install.py') for name in names))
                self.assertTrue(any(name.endswith('/README.md') for name in names))
            self.assertTrue((output / 'Porch.app/Contents/Info.plist').exists())


if __name__ == '__main__':
    unittest.main()
