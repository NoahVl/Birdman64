"""Run native visibility regressions with PW64_ZIG, without ROM data."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

GAME = Path(__file__).resolve().parents[1]
REPO = GAME.parents[1]


def patched(relative):
    original = (REPO / 'decomp' / relative).read_text()
    patch = GAME / 'patches' / (relative + '.patch')
    if not patch.exists():
        return original
    source = original.splitlines(keepends=True)
    result, cursor = [], 0
    for line in patch.read_text().splitlines(keepends=True):
        if line.startswith('@@'):
            start = int(re.match(r'@@ -(\d+)', line)[1]) - 1
            result.extend(source[cursor:start])
            cursor = start
        elif line.startswith(('---', '+++')):
            continue
        elif not line.strip():
            assert not source[cursor].strip(), (relative, cursor)
            result.append(source[cursor])
            cursor += 1
        elif line.startswith((' ', '-')):
            assert source[cursor].rstrip() == line[1:].rstrip(), (relative, cursor)
            if line[0] == ' ':
                result.append(source[cursor])
            cursor += 1
        elif line.startswith('+'):
            result.append(line[1:])
    return ''.join(result + source[cursor:])


with tempfile.TemporaryDirectory(prefix='pw64-visibility-') as directory:
    work = Path(directory)
    shutil.copytree(REPO / 'decomp/include', work / 'include')
    for patch in (GAME / 'patches/include').rglob('*.patch'):
        relative = str(patch.relative_to(GAME / 'patches'))[:-6]
        (work / relative).write_text(patched(relative))
    sources = []
    for name in ('sobj', 'dobj', 'chan'):
        dst = work / (name + '.c')
        dst.write_text(patched('src/kernel/' + name + '.c'))
        sources.append(str(dst))
    helper = GAME / 'native/src/pw64_visibility.c'
    if helper.exists():
        sources.append(str(helper))
    command = [os.environ['PW64_ZIG'], 'cc', '-O1', '-w', '-ffunction-sections',
               '-fdata-sections', '-Wl,--gc-sections', '-include', str(GAME / 'native/pw64_native.h')]
    for define in ('_LANGUAGE_C', 'VERSION_US', 'BUILD_VERSION=VERSION_D', '_FINALROM',
                   'TARGET_N64', 'NON_MATCHING', 'AVOID_UB', 'RECOMP_BUILD'):
        command.append('-D' + define)
    for include in (GAME / 'native/include', work / 'include', work / 'include/kernel',
                    work / 'include/libultra', work / 'include/libultra/PR',
                    work / 'include/libultra/compiler', REPO / 'decomp', REPO / 'decomp/src'):
        command.append('-I' + str(include))
    binary = work / 'visibility'
    subprocess.run(command + sources + [str(GAME / 'tests/visibility.c'), '-lm', '-o', str(binary)], check=True)
    failures = 0
    for lod, view in ((0, 0), (1, 0), (0, 1), (1, 1)):
        run = subprocess.run([str(binary), str(lod), str(view)])
        failures += run.returncode != 0
    for mode in (2, 3):
        run = subprocess.run([str(binary), str(mode), '0'])
        if run.returncode != 86:
            print('FAIL: packet capacity guard', mode, 'exit', run.returncode)
            failures += 1
    raise SystemExit(bool(failures))
