#!/usr/bin/env python3
r"""Install Porch in user directories with atomic executable replacement.

Run it from an extracted release directory, or point it at a release tarball:

  python3 install.py                                   # from an extracted archive
  python3 install.py --archive porch-0.3.0-aarch64-apple-darwin.tar.gz

Binaries go to ~/.local/bin (created if needed), or %LOCALAPPDATA%\Programs\Porch\bin
on Windows; on macOS Porch.app goes to ~/Applications. No hosts, trust stores, running daemons, sharing, or login
startup are changed.
"""
import argparse
import os
from pathlib import Path
import shutil
import sys
import tempfile

WINDOWS = os.name == 'nt'
EXE = '.exe' if WINDOWS else ''


def default_prefix():
    if WINDOWS and os.environ.get('LOCALAPPDATA'):
        return Path(os.environ['LOCALAPPDATA']) / 'Programs' / 'Porch'
    return Path.home() / '.local'


def move_aside(destination):
    """Windows cannot replace a running executable but can rename it."""
    if not (WINDOWS and destination.exists()):
        return
    aside = destination.with_name(f'{destination.name}.old')
    try:
        if aside.exists():
            aside.unlink()
    except OSError:
        # An earlier copy is still running; leave it and use a unique name.
        aside = destination.with_name(f'{destination.name}.{os.getpid()}.old')
    os.replace(destination, aside)
    try:
        aside.unlink()
    except OSError:
        pass  # still running; removed by a later install


def atomic_copy(source, destination, mode=0o755):
    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=f'.{destination.name}.', dir=destination.parent)
    try:
        with os.fdopen(fd, 'wb') as output, open(source, 'rb') as original:
            shutil.copyfileobj(original, output)
            output.flush()
            if hasattr(os, 'fchmod'):
                os.fchmod(output.fileno(), mode)
            os.fsync(output.fileno())
        move_aside(destination)
        os.replace(temporary, destination)
        if not WINDOWS:
            directory = os.open(destination.parent, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def install_app(source, destination):
    # Build the complete bundle beside its destination; never copy over a running
    # executable in place. Roll back if final bundle rename fails.
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(prefix='.Porch-install-', dir=destination.parent)) / 'Porch.app'
    backup = destination.parent / f'.Porch-backup-{os.getpid()}.app'
    try:
        shutil.copytree(source, temporary)
        if destination.exists():
            import plistlib
            with open(destination / 'Contents/Info.plist', 'rb') as stream:
                metadata = plistlib.load(stream)
            if metadata.get('CFBundleIdentifier') != 'dev.porch.tray':
                raise RuntimeError(f'refusing to replace unrelated app: {destination}')
            os.replace(destination, backup)
        try:
            os.replace(temporary, destination)
        except BaseException:
            if backup.exists():
                os.replace(backup, destination)
            raise
        if backup.exists():
            shutil.rmtree(backup)
    finally:
        shutil.rmtree(temporary.parent, ignore_errors=True)


def extract_archive(archive, destination):
    """Extract a release tarball safely and return its package directory."""
    import tarfile
    with tarfile.open(archive, 'r:gz') as tar:
        members = tar.getmembers()
        root = destination.resolve()
        for member in members:
            target = (destination / member.name).resolve()
            if root != target and root not in target.parents:
                raise RuntimeError(f'unsafe path in archive: {member.name}')
            if member.issym() or member.islnk():
                link = (target.parent / member.linkname).resolve()
                if root != link and root not in link.parents:
                    raise RuntimeError(f'unsafe link in archive: {member.name}')
            elif not (member.isfile() or member.isdir()):
                raise RuntimeError(f'unsupported entry in archive: {member.name}')
        if hasattr(tarfile, 'data_filter'):
            tar.extractall(destination, filter='data')
        else:
            tar.extractall(destination)
    packages = [path for path in destination.iterdir()
                if (path / 'porch').is_file() or (path / 'porch.exe').is_file()]
    if len(packages) != 1:
        raise RuntimeError(f'{archive} is not a Porch release archive')
    return packages[0]


def path_hint(bin_dir, path=None, windows=WINDOWS):
    """Return the exact line to add to a shell profile, or None if on PATH."""
    path = os.environ.get('PATH', '') if path is None else path
    norm = os.path.normcase if windows else (lambda p: p)
    resolved = norm(os.path.realpath(bin_dir))
    separator = ';' if windows else os.pathsep
    if any(entry and norm(os.path.realpath(os.path.expanduser(entry))) == resolved
           for entry in path.split(separator)):
        return None
    if windows:
        return ("[Environment]::SetEnvironmentVariable('Path', "
                f"'{bin_dir};' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')")
    home = str(Path.home())
    shown = str(bin_dir)
    if shown == home or shown.startswith(home + os.sep):
        shown = '$HOME' + shown[len(home):]
    return f'export PATH="{shown}:$PATH"'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=Path(__file__).resolve().parent,
                        help='extracted package directory (or target/release)')
    parser.add_argument('--archive', type=Path,
                        help='release tarball (porch-VERSION-TARGET.tar.gz) to install from')
    parser.add_argument('--prefix', type=Path, default=default_prefix(),
                        help='binary install prefix; default ~/.local (Windows: %%LOCALAPPDATA%%\\Programs\\Porch)')
    parser.add_argument('--app-dir', type=Path, default=Path.home() / 'Applications',
                        help='macOS app destination; default ~/Applications')
    parser.add_argument('--no-app', action='store_true', help='install binaries only')
    parser.add_argument('--cli-only', action='store_true', help='omit optional desktop components')
    args = parser.parse_args()
    if args.archive:
        with tempfile.TemporaryDirectory(prefix='porch-install-') as temporary:
            try:
                args.source = extract_archive(args.archive, Path(temporary))
            except (OSError, RuntimeError) as error:
                parser.error(str(error))
            install(parser, args)
    else:
        install(parser, args)


def install(parser, args):
    source = args.source.resolve()
    names = [name + EXE for name in ['porch'] + ([] if args.cli_only else ['porch-tray'])]
    for name in names:
        binary = source / name
        if not binary.is_file():
            parser.error(f'{binary} is missing; extract a package or pass --source target/release')
    login = source / 'porch-login'
    if not login.is_file():
        login = Path(__file__).resolve().parent / 'porch-login'
    # porch-login drives launchd/systemd only.
    if not args.cli_only and not WINDOWS and login.is_file():
        atomic_copy(login, args.prefix / 'bin/porch-login')
    for name in names:
        atomic_copy(source / name, args.prefix / 'bin' / name)
        print(f'Installed {args.prefix / "bin" / name}')
    app = source / 'Porch.app'
    if sys.platform == 'darwin' and not args.cli_only and not args.no_app and app.is_dir():
        destination = args.app_dir / 'Porch.app'
        install_app(app, destination)
        print(f'Installed {destination}')
        print(f'Launch explicitly: open "{destination}"')
    print('Running apps and proxies are unchanged. Sharing and login startup remain unchanged.')
    hint = path_hint(args.prefix / 'bin')
    if hint and WINDOWS:
        print(f'{args.prefix / "bin"} is not on your PATH. Add it for your account in')
        print('PowerShell, then open a new terminal:')
        print(f'  {hint}')
    elif hint:
        print(f'{args.prefix / "bin"} is not on your PATH. Add this line to your shell profile')
        print('(~/.zshrc, ~/.bashrc, or ~/.profile), then open a new terminal:')
        print(f'  {hint}')


if __name__ == '__main__':
    main()
