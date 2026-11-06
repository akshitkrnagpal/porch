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
formula = load('porch_formula', ROOT / 'scripts/render_formula.py')


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
                self.assertTrue(any(name.endswith('/LICENSE') for name in names))
            self.assertTrue((output / 'Porch.app/Contents/Info.plist').exists())

    def test_install_from_archive_into_isolated_prefix(self):
        with tempfile.TemporaryDirectory(prefix='porch-archive-install-') as temporary:
            root = Path(temporary)
            binaries, output = root / 'bin', root / 'out'
            binaries.mkdir()
            for name in ['porch', 'porch-tray']:
                (binaries / name).write_bytes(b'#!/bin/sh\necho porch 0.2.0\n')
                (binaries / name).chmod(0o755)
            subprocess.run([sys.executable, str(ROOT / 'scripts/package.py'), '--target', 'aarch64-apple-darwin',
                            '--binaries', str(binaries), '--output', str(output)], check=True, capture_output=True)
            archive, = output.glob('*.tar.gz')
            prefix, apps = root / 'home/.local', root / 'home/Applications'
            env = dict(os.environ, PATH='/usr/bin:/bin')
            result = subprocess.run([sys.executable, str(ROOT / 'scripts/install.py'), '--archive', str(archive),
                                     '--prefix', str(prefix), '--app-dir', str(apps)],
                                    check=True, capture_output=True, text=True, env=env)
            for name in ['porch', 'porch-tray', 'porch-login']:
                installed = prefix / 'bin' / name
                self.assertTrue(installed.is_file(), name)
                self.assertTrue(os.access(installed, os.X_OK), name)
            self.assertIn(f'export PATH="{prefix / "bin"}:$PATH"', result.stdout)
            if sys.platform == 'darwin':
                self.assertTrue((apps / 'Porch.app/Contents/MacOS/porch-tray').is_file())
            else:
                self.assertFalse(apps.exists())

    def test_archive_extraction_rejects_traversal(self):
        import io
        with tempfile.TemporaryDirectory(prefix='porch-evil-archive-') as temporary:
            root = Path(temporary)
            archive = root / 'evil.tar.gz'
            with tarfile.open(archive, 'w:gz') as tar:
                data = b'owned'
                info = tarfile.TarInfo('../escape')
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
            (root / 'x').mkdir()
            with self.assertRaisesRegex(RuntimeError, 'unsafe path'):
                installer.extract_archive(archive, root / 'x')
            self.assertFalse((root / 'escape').exists())

    def test_path_hint(self):
        home = Path.home()
        self.assertIsNone(installer.path_hint(home / '.local/bin', f'/usr/bin:{home}/.local/bin'))
        self.assertIsNone(installer.path_hint(home / '.local/bin', '/usr/bin:~/.local/bin'))
        self.assertEqual(installer.path_hint(home / '.local/bin', '/usr/bin'),
                         'export PATH="$HOME/.local/bin:$PATH"')
        self.assertEqual(installer.path_hint(Path('/opt/porch/bin'), '/usr/bin'),
                         'export PATH="/opt/porch/bin:$PATH"')


class FormulaTests(unittest.TestCase):
    DIGESTS = {target: f'{index:x}' * 64 for index, target in enumerate(formula.TARGETS, 1)}

    def sums(self, version='0.3.0'):
        return ''.join(f'{digest}  porch-{version}-{target}.tar.gz\n' for target, digest in self.DIGESTS.items()) + \
            f'{"f" * 64} *SHA256SUMS.extra\n'

    def test_renders_every_platform_with_its_digest(self):
        template = (ROOT / 'packaging/homebrew/porch.rb').read_text()
        rendered = formula.render(template, 'v0.3.0', formula.parse_sums(self.sums()))
        self.assertIn('class Porch < Formula', rendered)
        self.assertIn('version "0.3.0"', rendered)
        self.assertNotRegex(rendered, r'@[A-Z0-9_]+@')
        for target, digest in self.DIGESTS.items():
            url = f'https://github.com/akshitkrnagpal/porch/releases/download/v0.3.0/porch-0.3.0-{target}.tar.gz'
            line = rendered.index(f'url "{url}"')
            self.assertEqual(rendered[line:].splitlines()[1].strip(), f'sha256 "{digest}"')
        self.assertIn('bin.install "porch"', rendered)
        self.assertIn('porch --version', rendered)

    def test_rejects_missing_targets_and_bad_input(self):
        template = (ROOT / 'packaging/homebrew/porch.rb').read_text()
        partial = formula.parse_sums(self.sums().split('\n', 1)[1])
        with self.assertRaisesRegex(ValueError, 'no entry'):
            formula.render(template, '0.3.0', partial)
        with self.assertRaisesRegex(ValueError, 'no entry'):
            formula.render(template, '0.3.1', formula.parse_sums(self.sums()))
        with self.assertRaisesRegex(ValueError, 'invalid version'):
            formula.render(template, 'latest', {})
        with self.assertRaisesRegex(ValueError, 'malformed'):
            formula.parse_sums('nothex  file\n')

    def test_cli_writes_output_file(self):
        with tempfile.TemporaryDirectory(prefix='porch-formula-') as temporary:
            root = Path(temporary)
            (root / 'SHA256SUMS').write_text(self.sums())
            subprocess.run([sys.executable, str(ROOT / 'scripts/render_formula.py'), '--version', '0.3.0',
                            '--sums', str(root / 'SHA256SUMS'), '--output', str(root / 'Formula/porch.rb')],
                           check=True, capture_output=True)
            self.assertIn('version "0.3.0"', (root / 'Formula/porch.rb').read_text())
            failed = subprocess.run([sys.executable, str(ROOT / 'scripts/render_formula.py'), '--version', '9.9.9',
                                     '--sums', str(root / 'SHA256SUMS')], capture_output=True, text=True)
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn('no entry', failed.stderr)


if __name__ == '__main__':
    unittest.main()
