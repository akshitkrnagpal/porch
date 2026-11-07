#!/usr/bin/env python3
"""Compare the size of two sets of Porch release archives; read-only.

  python3 scripts/release_size.py BASELINE_DIR CANDIDATE_DIR

Each directory holds porch-VERSION-TARGET.tar.gz files (for example the
artifacts of two Release workflow runs). Archives are matched by target, never
extracted, and never executed. The report is Markdown on stdout: archive
bytes, the bytes of each executable, and how many bytes tar stores for them
(a hard link stores none).
"""
import argparse
from pathlib import Path
import re
import sys
import tarfile

ARCHIVE = re.compile(r'^porch-[0-9][^/]*?-([a-z0-9_]+-[a-z0-9_]+-[a-z0-9_]+(?:-[a-z0-9_]+)?)\.tar\.gz$')
EXECUTABLES = ('porch', 'porch-tray', 'porch.exe', 'porch-tray.exe')


def inspect(archive):
    """Sizes in one archive. Executables are keyed by their path below the
    package root (porch, Porch.app/Contents/MacOS/porch, ...)."""
    sizes, executables, stored = {}, {}, 0
    with tarfile.open(archive, 'r:gz') as tar:
        members = tar.getmembers()
    for member in members:
        if member.isfile():
            sizes[member.name] = member.size
    for member in members:
        relative = member.name.split('/', 1)[1] if '/' in member.name else member.name
        if relative.rsplit('/', 1)[-1] not in EXECUTABLES:
            continue
        if member.isfile():
            executables[relative] = (member.size, 'file')
            stored += member.size
        elif member.islnk():
            executables[relative] = (sizes.get(member.linkname, 0), 'hard link')
    return {'archive': Path(archive).stat().st_size, 'executables': executables,
            'stored': stored, 'unpacked': sum(size for size, _ in executables.values())}


def collect(directory):
    """Archives anywhere below `directory` (`gh run download` puts each
    artifact in its own subdirectory), keyed by target."""
    found = {}
    for path in sorted(Path(directory).rglob('*.tar.gz')):
        match = ARCHIVE.match(path.name)
        if not match:
            continue
        if match.group(1) in found:
            raise ValueError(f'two archives for {match.group(1)}: {found[match.group(1)]} and {path}')
        found[match.group(1)] = path
    return found


def change(before, after):
    if before == 0:
        return 'n/a'
    return f'{after - before:+,} ({(after - before) / before * 100:+.1f}%)'


def report(baseline, candidate):
    lines = ['| Target | Item | Baseline bytes | Candidate bytes | Change |',
             '| --- | --- | ---: | ---: | ---: |']
    totals = [0, 0]
    for target in sorted(set(baseline) | set(candidate)):
        if target not in baseline or target not in candidate:
            lines.append(f'| {target} | missing from {"baseline" if target not in baseline else "candidate"} | | | |')
            continue
        old, new = inspect(baseline[target]), inspect(candidate[target])
        for path in sorted(set(old['executables']) | set(new['executables'])):
            before = old['executables'].get(path, (0, 'absent'))
            after = new['executables'].get(path, (0, 'absent'))
            note = f' ({after[1]})' if after[1] != 'file' else ''
            lines.append(f'| {target} | `{path}`{note} | {before[0]:,} | {after[0]:,} | {change(before[0], after[0])} |')
        lines.append(f'| {target} | executable bytes stored in tar | {old["stored"]:,} | {new["stored"]:,} | '
                     f'{change(old["stored"], new["stored"])} |')
        lines.append(f'| {target} | **archive (.tar.gz)** | **{old["archive"]:,}** | **{new["archive"]:,}** | '
                     f'**{change(old["archive"], new["archive"])}** |')
        totals[0] += old['archive']
        totals[1] += new['archive']
    lines.append(f'| all targets | **archives total** | **{totals[0]:,}** | **{totals[1]:,}** | '
                 f'**{change(*totals)}** |')
    return '\n'.join(lines) + '\n'


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('baseline', type=Path)
    parser.add_argument('candidate', type=Path)
    args = parser.parse_args(argv)
    try:
        baseline, candidate = collect(args.baseline), collect(args.candidate)
    except ValueError as error:
        parser.exit(1, f'release_size: {error}\n')
    if not baseline or not candidate:
        parser.exit(1, 'release_size: each directory needs porch-VERSION-TARGET.tar.gz archives\n')
    sys.stdout.write(report(baseline, candidate))


if __name__ == '__main__':
    main()
