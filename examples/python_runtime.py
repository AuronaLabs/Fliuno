"""Explicit Windows CPython fixture dependencies, without pip or directory grants.

This small demonstration bundle supports bundle_python/server.py. It is not a
general Python installation packager. Every runtime file becomes a pinned asset.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import zipfile


def stage_runtime(python, destination):
    if os.name != 'nt':
        # Linux system library directories are covered by the existing read-only
        # runtime policy. User/venv installations elsewhere require a bundle.
        return
    details = json.loads(subprocess.check_output([
        python, '-I', '-S', '-c',
        'import sys,json,sysconfig; print(json.dumps({'
        '"base":sys.base_prefix,"stdlib":sysconfig.get_path("stdlib"),'
        '"tag":f"python{sys.version_info.major}{sys.version_info.minor}",'
        '"version":list(sys.version_info[:2])}))',
    ], text=True, encoding='utf-8', timeout=10))
    if details['version'] != [3, 12]:
        raise RuntimeError('The Windows demonstration bundle requires CPython 3.12.')
    base, stdlib = Path(details['base']), Path(details['stdlib'])
    for name in [details['tag'] + '.dll', 'python3.dll', 'vcruntime140.dll', 'vcruntime140_1.dll']:
        shutil.copyfile(base / name, destination / name)
    for name in ['_socket.pyd', 'select.pyd']:
        shutil.copyfile(base / 'DLLs' / name, destination / name)
    # ZIP_STORED avoids introducing an undeclared zlib extension dependency.
    with zipfile.ZipFile(destination / 'stdlib.zip', 'w', compression=zipfile.ZIP_STORED) as archive:
        for name in ['__future__', 'abc', 'codecs', 'contextlib', 'copyreg', 'enum',
                     'functools', 'genericpath', 'io', 'keyword', 'ntpath', 'operator',
                     'os', 'posixpath', 'reprlib', 'selectors', 'socket', 'stat',
                     'struct', 'types', 'warnings', 'weakref']:
            archive.write(stdlib / (name + '.py'), name + '.py')
        for package in ['encodings', 'json', 'collections', 're']:
            for source in sorted((stdlib / package).glob('*.py')):
                archive.write(source, source.relative_to(stdlib).as_posix())
    # The shared-library-name ._pth overrides registry and environment paths,
    # and does not import site. The native executable is staged as program.exe.
    (destination / (details['tag'] + '._pth')).write_text('stdlib.zip\n.\n', encoding='ascii')
