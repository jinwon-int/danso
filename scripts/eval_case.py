#!/usr/bin/env python3
"""Frozen coding cases and isolated acceptance execution; no harness/model calls."""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import resource
import shutil
import signal
import stat
import sys

from harness_eval import digest, read_json, rendered, require

ROOT = Path(__file__).resolve().parent.parent
CASES = ROOT / 'examples/harness-eval/cases'
FILE_CAP = 1024 * 1024
TREE_CAP = 4 * FILE_CAP
OUTPUT_CAP = 65536
WALL_SECONDS = 3


def safe_path(path):
    path = Path(os.path.abspath(path))
    require(not any(part.is_symlink() for part in (path, *path.parents)))
    return path


def read_bytes(path):
    path = safe_path(path)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_size <= FILE_CAP)
        result = stream.read(FILE_CAP + 1)
        require(len(result) <= FILE_CAP)
        return result


def relative(name):
    require(type(name) is str and 0 < len(name) <= 200)
    p = PurePosixPath(name)
    require(bool(p.parts) and not p.is_absolute() and str(p) == name and '..' not in p.parts)
    require(all(part not in ('', '.') for part in p.parts))
    return name


def load_case(name):
    require(name in ('slug', 'csv_totals', 'paths'))
    case = read_json(CASES / f'{name}.json')
    require(set(case) == {'schema', 'id', 'prompt', 'files', 'entrypoint', 'checks'})
    require(case['schema'] == 'danso.eval.case.v1' and case['id'] == name)
    require(type(case['prompt']) is str and 0 < len(case['prompt']) <= 8192)
    require(type(case['files']) is dict and 0 < len(case['files']) <= 64)
    for path, content in case['files'].items():
        relative(path)
        require(type(content) is str and len(content.encode()) <= FILE_CAP)
    require(relative(case['entrypoint']) in case['files'])
    require(type(case['checks']) is list and 1 <= len(case['checks']) <= 32)
    for check in case['checks']:
        require(type(check) is dict and set(check) == {'input', 'expected'})
        require(len(rendered(check).encode()) <= OUTPUT_CAP)
    return case


def descriptor(case):
    # Bind the host-owned oracle implementation as well as vectors and entrypoint.
    oracle = {'entrypoint': case['entrypoint'], 'checks': case['checks'],
              'runner_sha256': hashlib.sha256(read_bytes(Path(__file__))).hexdigest(),
              'wall_seconds': WALL_SECONDS, 'output_cap': OUTPUT_CAP}
    return {'id': case['id'], 'prompt_sha256': hashlib.sha256(case['prompt'].encode()).hexdigest(),
            'input_sha256': digest(case['files']), 'acceptance_sha256': digest(oracle)}


def private_root(path):
    path = safe_path(path)
    path.mkdir(mode=0o700)  # Exclusive attempt: never overwrite or silently rerun.
    os.chmod(path, 0o700)
    return path


def write_new(path, content):
    path = safe_path(path)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'wb') as stream:
        os.fchmod(stream.fileno(), 0o600)
        stream.write(content)
        stream.flush()
        os.fsync(stream.fileno())


def prepare(case, destination):
    destination = private_root(destination)
    for path, content in case['files'].items():
        write_new(destination / path, content.encode())
    return descriptor(case)


def snapshot(workspace):
    workspace = safe_path(workspace)
    require(workspace.is_dir())
    files = {}
    total = 0
    directories = [workspace]
    seen = 0
    while directories:
        directory = directories.pop()
        safe_path(directory)
        with os.scandir(directory) as entries:
            for entry in entries:
                seen += 1
                require(seen <= 128 and not entry.is_symlink())
                path = Path(entry.path)
                name = relative(path.relative_to(workspace).as_posix())
                if entry.is_dir(follow_symlinks=False):
                    directories.append(path)
                else:
                    raw = read_bytes(path)
                    total += len(raw)
                    require(total <= TREE_CAP and len(files) < 64)
                    files[name] = raw.decode('utf-8')
    return files


def limits():
    for kind, cap in ((resource.RLIMIT_AS, 256 * 1024 * 1024),
                      (resource.RLIMIT_FSIZE, OUTPUT_CAP), (resource.RLIMIT_NOFILE, 64),
                      (resource.RLIMIT_CPU, 2), (resource.RLIMIT_CORE, 0)):
        resource.setrlimit(kind, (cap, cap))


def sandbox_command(workspace, entrypoint):
    binary = shutil.which('bwrap')
    require(binary is not None)
    command = [binary, '--unshare-all', '--die-with-parent', '--new-session',
               '--cap-drop', 'ALL', '--clearenv']
    for path in ('/usr', '/bin', '/lib', '/lib64'):
        if Path(path).exists():
            command += ['--ro-bind', path, path]
    command += ['--proc', '/proc', '--dev', '/dev', '--ro-bind', str(workspace), '/work',
                '--chdir', '/work', '--setenv', 'PATH', '/usr/bin:/bin',
                '--setenv', 'HOME', '/nonexistent', '--', '/usr/bin/python3', '-I', '-B', entrypoint]
    return command


async def execute(command, input_bytes):
    output = [bytearray(), bytearray()]
    process = await asyncio.create_subprocess_exec(
        *command, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE, env={'PATH': '/usr/bin:/bin'},
        start_new_session=True, preexec_fn=limits)
    status = 'exited'

    async def drain(stream, destination):
        while block := await stream.read(4096):
            if len(destination) + len(block) > OUTPUT_CAP:
                raise ValueError('output limit')
            destination.extend(block)

    async def send():
        try:
            process.stdin.write(input_bytes)
            await process.stdin.drain()
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            process.stdin.close()

    tasks = [asyncio.create_task(send()), asyncio.create_task(drain(process.stdout, output[0])),
             asyncio.create_task(drain(process.stderr, output[1])), asyncio.create_task(process.wait())]
    try:
        await asyncio.wait_for(asyncio.gather(*tasks), WALL_SECONDS)
    except asyncio.TimeoutError:
        status = 'timeout'
    except ValueError:
        status = 'output_limit'
    finally:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        await process.wait()
    return status, process.returncode, bytes(output[0]), bytes(output[1])


def accept(case, workspace, evidence):
    workspace, evidence = safe_path(workspace), safe_path(evidence)
    require(not evidence.is_relative_to(workspace) and not workspace.is_relative_to(evidence))
    files = snapshot(workspace)
    require(case['entrypoint'] in files)
    evidence = private_root(evidence)
    frozen = evidence / 'workspace'
    frozen.mkdir(mode=0o700)
    for name, content in files.items():
        write_new(frozen / name, content.encode())
    # Preflight prevents broken sandbox setup being mislabelled as task failure.
    command = sandbox_command(frozen, case['entrypoint'])
    probe = command[:-1] + ['-c', 'print("sandbox-ready")']
    state, code, out, _ = asyncio.run(execute(probe, b''))
    require(state == 'exited' and code == 0 and out == b'sandbox-ready\n')
    results = []
    for index, check in enumerate(case['checks']):
        state, code, out, err = asyncio.run(execute(command, rendered(check['input']).encode()))
        passed = False
        if state == 'exited' and code == 0:
            try:
                # Canonical JSON comparison keeps booleans distinct from integers.
                passed = digest(json.loads(out)) == digest(check['expected'])
            except (ValueError, UnicodeError, RecursionError):
                pass
        write_new(evidence / f'{index}.stdout', out)
        write_new(evidence / f'{index}.stderr', err)
        results.append({'check': index, 'passed': passed, 'state': state, 'exit_code': code,
                        'stdout_sha256': hashlib.sha256(out).hexdigest(),
                        'stderr_sha256': hashlib.sha256(err).hexdigest()})
    report = {'schema': 'danso.eval.acceptance.v1', 'case': descriptor(case),
              'candidate_sha256': digest(files), 'passed': all(r['passed'] for r in results),
              'checks': results, 'python_sha256': hashlib.sha256(Path('/usr/bin/python3').read_bytes()).hexdigest()}
    write_new(evidence / 'acceptance.json', rendered(report).encode())
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('catalog')
    p = sub.add_parser('prepare'); p.add_argument('case'); p.add_argument('workspace')
    p = sub.add_parser('accept'); p.add_argument('case'); p.add_argument('workspace'); p.add_argument('evidence')
    args = parser.parse_args()
    try:
        if args.command == 'catalog':
            result = [descriptor(load_case(name)) for name in ('slug', 'csv_totals', 'paths')]
        elif args.command == 'prepare':
            result = prepare(load_case(args.case), args.workspace)
        else:
            result = accept(load_case(args.case), args.workspace, args.evidence)
        print(rendered(result), end='')
        return 0 if args.command != 'accept' or result['passed'] else 1
    except (OSError, ValueError, TypeError, KeyError, RecursionError):
        print('Case execution failed validation; existing evidence is retained.', file=sys.stderr)
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
