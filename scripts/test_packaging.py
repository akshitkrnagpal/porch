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
sizes = load('porch_release_size', ROOT / 'scripts/release_size.py')


class PackageTests(unittest.TestCase):
    @unittest.skipIf(os.name == 'nt', 'inodes and Unix modes')
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

    def test_windows_package_ships_executables_without_login_script(self):
        with tempfile.TemporaryDirectory(prefix='porch-windows-archive-test-') as temporary:
            root = Path(temporary)
            binaries, output = root / 'bin', root / 'out'
            binaries.mkdir()
            for name in ['porch.exe', 'porch-tray.exe']:
                (binaries / name).write_bytes(b'test binary')
            subprocess.run([sys.executable, str(ROOT / 'scripts/package.py'), '--target', 'x86_64-pc-windows-msvc',
                            '--binaries', str(binaries), '--output', str(output)], check=True, capture_output=True)
            archive, = output.glob('*.tar.gz')
            self.assertTrue(archive.name.endswith('-x86_64-pc-windows-msvc.tar.gz'))
            with tarfile.open(archive) as tar:
                names = tar.getnames()
            for expected in ['/porch.exe', '/porch-tray.exe', '/install.py', '/LICENSE']:
                self.assertTrue(any(name.endswith(expected) for name in names), expected)
            self.assertFalse(any(name.endswith('/porch-login') or 'Porch.app' in name for name in names))
            self.assertFalse((output / 'Porch.app').exists())

    @unittest.skipIf(os.name == 'nt', 'Unix package layout and shell PATH hint')
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
                for name in ['porch', 'porch-tray']:
                    bundled = apps / 'Porch.app/Contents/MacOS' / name
                    self.assertEqual(bundled.read_bytes(), (prefix / 'bin' / name).read_bytes(), name)
                    self.assertTrue(os.access(bundled, os.X_OK), name)
            else:
                self.assertFalse(apps.exists())

    def package(self, root, target='aarch64-apple-darwin', payload=b'#!/bin/sh\necho porch 0.2.0\n'):
        binaries, output = root / 'bin', root / 'out'
        binaries.mkdir()
        windows = 'windows' in target
        for name in ['porch', 'porch-tray']:
            path = binaries / (name + ('.exe' if windows else ''))
            path.write_bytes(payload + name.encode())
            path.chmod(0o755)
        subprocess.run([sys.executable, str(ROOT / 'scripts/package.py'), '--target', target,
                        '--binaries', str(binaries), '--output', str(output)], check=True, capture_output=True)
        archive, = output.glob('*.tar.gz')
        return archive

    def test_macos_archive_stores_each_executable_once(self):
        with tempfile.TemporaryDirectory(prefix='porch-hardlink-archive-') as temporary:
            archive = self.package(Path(temporary))
            name = archive.name[:-len('.tar.gz')]
            with tarfile.open(archive) as tar:
                members = {member.name: member for member in tar.getmembers()}
            for executable in ['porch', 'porch-tray']:
                root = members[f'{name}/{executable}']
                bundled = members[f'{name}/Porch.app/Contents/MacOS/{executable}']
                self.assertTrue(root.isfile(), executable)
                self.assertTrue(bundled.islnk(), executable)
                self.assertEqual(bundled.linkname, root.name)
            # Exactly one regular-file entry per executable name.
            stored = [m for m in members.values() if m.isfile() and m.name.rsplit('/', 1)[-1] in ('porch', 'porch-tray')]
            self.assertEqual(len(stored), 2)

    @unittest.skipIf(os.name == 'nt', 'Unix modes; Windows archives have no app bundle')
    def test_hard_linked_archive_extracts_to_real_files_everywhere(self):
        """install.py, system tar (porch self-update, Homebrew) and the size
        report all see the app executables as complete files."""
        import shutil
        with tempfile.TemporaryDirectory(prefix='porch-hardlink-extract-') as temporary:
            root = Path(temporary)
            archive = self.package(root)
            extractors = {'install.py': lambda d: installer.extract_archive(archive, d)}
            if shutil.which('tar'):
                def system_tar(destination):
                    subprocess.run(['tar', '-xzf', str(archive), '-C', str(destination)], check=True)
                    return destination / archive.name[:-len('.tar.gz')]
                extractors['tar'] = system_tar
            for label, extract in extractors.items():
                destination = root / f'x-{label}'
                destination.mkdir()
                package = extract(destination)
                for name in ['porch', 'porch-tray']:
                    top, bundled = package / name, package / 'Porch.app/Contents/MacOS' / name
                    self.assertEqual(bundled.read_bytes(), top.read_bytes(), f'{label}: {name}')
                    self.assertTrue(os.access(bundled, os.X_OK), f'{label}: {name}')
            table = sizes.report({'aarch64-apple-darwin': archive}, {'aarch64-apple-darwin': archive})
            self.assertIn('`Porch.app/Contents/MacOS/porch` (hard link)', table)
            self.assertIn('**archives total**', table)

    def test_hard_link_outside_archive_is_rejected(self):
        import io
        with tempfile.TemporaryDirectory(prefix='porch-evil-link-') as temporary:
            root = Path(temporary)
            archive = root / 'evil.tar.gz'
            with tarfile.open(archive, 'w:gz') as tar:
                info = tarfile.TarInfo('porch-1/porch')
                info.type = tarfile.LNKTYPE
                info.linkname = '../../etc/passwd'
                tar.addfile(info)
            (root / 'x').mkdir()
            with self.assertRaisesRegex(RuntimeError, 'unsafe link'):
                installer.extract_archive(archive, root / 'x')

    def test_symlink_chains_are_rejected_before_extraction(self):
        """Symlinks that point inside the archive one by one, but escape
        together, must fail even on Pythons without tarfile.data_filter."""
        import io
        with tempfile.TemporaryDirectory(prefix='porch-evil-symlinks-') as temporary:
            root = Path(temporary)
            archive = root / 'evil.tar.gz'
            with tarfile.open(archive, 'w:gz') as tar:
                for name, target in [('a', 'b'), ('b', '.'), ('a/c', '..')]:
                    info = tarfile.TarInfo(name)
                    info.type, info.linkname = tarfile.SYMTYPE, target
                    tar.addfile(info)
                info = tarfile.TarInfo('pkg/porch')
                info.type, info.linkname = tarfile.LNKTYPE, 'c/victim'
                tar.addfile(info)
                data = b'owned'
                info = tarfile.TarInfo('pkg/porch')
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
            (root / 'x').mkdir()
            (root / 'victim').write_bytes(b'safe')
            with self.assertRaisesRegex(RuntimeError, 'unsupported entry|unsafe'):
                installer.extract_archive(archive, root / 'x')
            self.assertEqual((root / 'victim').read_bytes(), b'safe')
            self.assertEqual(list((root / 'x').iterdir()), [])

    def test_size_report_finds_archives_in_artifact_subdirectories(self):
        with tempfile.TemporaryDirectory(prefix='porch-size-report-') as temporary:
            root = Path(temporary)
            archive = self.package(root)
            for side in ['before', 'after']:
                (root / side / 'porch-aarch64-apple-darwin').mkdir(parents=True)
                (root / side / 'porch-aarch64-apple-darwin' / archive.name).write_bytes(archive.read_bytes())
            result = subprocess.run([sys.executable, str(ROOT / 'scripts/release_size.py'),
                                     str(root / 'before'), str(root / 'after')],
                                    check=True, capture_output=True, text=True)
            self.assertIn('| aarch64-apple-darwin | **archive (.tar.gz)** |', result.stdout)
            (root / 'after' / 'copy').mkdir()
            (root / 'after' / 'copy' / archive.name).write_bytes(archive.read_bytes())
            duplicate = subprocess.run([sys.executable, str(ROOT / 'scripts/release_size.py'),
                                        str(root / 'before'), str(root / 'after')], capture_output=True, text=True)
            self.assertNotEqual(duplicate.returncode, 0)
            self.assertIn('two archives', duplicate.stderr)

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

    @unittest.skipIf(os.name == 'nt', 'Unix shell PATH hint')
    def test_path_hint(self):
        home = Path.home()
        self.assertIsNone(installer.path_hint(home / '.local/bin', f'/usr/bin:{home}/.local/bin', windows=False))
        self.assertIsNone(installer.path_hint(home / '.local/bin', '/usr/bin:~/.local/bin', windows=False))
        self.assertEqual(installer.path_hint(home / '.local/bin', '/usr/bin', windows=False),
                         'export PATH="$HOME/.local/bin:$PATH"')
        self.assertEqual(installer.path_hint(Path('/opt/porch/bin'), '/usr/bin', windows=False),
                         'export PATH="/opt/porch/bin:$PATH"')

    def test_windows_path_hint_is_case_insensitive_powershell(self):
        bin_dir = Path('/Users/me/AppData/Local/Programs/Porch/bin')
        self.assertIsNone(installer.path_hint(bin_dir, f'/windows;{bin_dir}', windows=True))
        hint = installer.path_hint(bin_dir, '/windows', windows=True)
        self.assertIn("SetEnvironmentVariable('Path', ", hint)
        self.assertIn(f"'{bin_dir};'", hint)


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
