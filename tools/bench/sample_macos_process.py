#!/usr/bin/env python3
'''Sample macOS RSS, CPU and physical-footprint observations for a process tree.'''

from __future__ import annotations

import argparse
import json
import math
import os
import stat
import subprocess
import sys
import time
from collections import deque
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path


@dataclass(frozen=True)
class ProcessRecord:
    pid: int
    ppid: int
    rss_kb: int
    cpu_percent: float
    command: str


def parse_process_table(text: str) -> dict[int, ProcessRecord]:
    '''Parse `ps -axo pid= -o ppid= -o rss= -o %cpu= -o comm=` output.'''
    records: dict[int, ProcessRecord] = {}
    for line in text.splitlines():
        columns = line.split(maxsplit=4)
        if len(columns) != 5:
            continue
        try:
            pid = int(columns[0])
            ppid = int(columns[1])
            rss_kb = int(columns[2])
            cpu_percent = float(columns[3])
            if pid <= 0 or ppid < 0 or rss_kb < 0 or not math.isfinite(cpu_percent):
                continue
            record = ProcessRecord(
                pid=pid,
                ppid=ppid,
                rss_kb=rss_kb,
                cpu_percent=cpu_percent,
                command=Path(columns[4]).name,
            )
        except ValueError:
            continue
        records[record.pid] = record
    return records


def process_tree(root_pid: int, records: dict[int, ProcessRecord]) -> list[int]:
    '''Return root plus all currently observed descendants, once each.'''
    children: dict[int, list[int]] = {}
    for record in records.values():
        children.setdefault(record.ppid, []).append(record.pid)
    ordered: list[int] = []
    pending = deque([root_pid])
    seen: set[int] = set()
    while pending:
        pid = pending.popleft()
        if pid in seen:
            continue
        seen.add(pid)
        if pid in records:
            ordered.append(pid)
        pending.extend(children.get(pid, []))
    return ordered


def capture(command: list[str]) -> dict[str, object]:
    try:
        result = subprocess.run(command, capture_output=True, text=True, check=False)
    except OSError as error:
        return {'command': command[0], 'error': str(error)}
    if result.returncode != 0:
        return {
            'command': command[0],
            'exit_code': result.returncode,
            'error': result.stderr.strip() or 'command failed',
        }
    return {'command': command[0], 'output': result.stdout.strip()}


def open_private_output(path: Path):
    '''Open an append-only JSONL target without following a final symlink.'''
    nofollow = getattr(os, 'O_NOFOLLOW', None)
    if nofollow is None:
        raise OSError('secure output requires O_NOFOLLOW support')
    flags = os.O_WRONLY | os.O_APPEND | os.O_CREAT
    flags |= getattr(os, 'O_CLOEXEC', 0) | nofollow
    flags |= getattr(os, 'O_NONBLOCK', 0)
    fd = os.open(path, flags, 0o600)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise OSError(f'output target is not a regular file: {path}')
        # Existing files may have been created with a broader umask. These
        # logs are intended to stay local, so keep the selected target private.
        os.fchmod(fd, 0o600)
        return os.fdopen(fd, 'a', encoding='utf-8')
    except Exception:
        os.close(fd)
        raise


def sample(pid: int, include_children: bool, include_vmmap: bool) -> dict[str, object]:
    timestamp = datetime.now().astimezone().isoformat(timespec='milliseconds')
    try:
        ps = subprocess.run(
            ['/bin/ps', '-axo', 'pid=', '-o', 'ppid=', '-o', 'rss=', '-o', '%cpu=', '-o', 'comm='],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as error:
        return {
            'timestamp': timestamp,
            'root_pid': pid,
            'sample_status': 'ps_failed',
            'error': str(error),
        }
    if ps.returncode != 0:
        return {
            'timestamp': timestamp,
            'root_pid': pid,
            'sample_status': 'ps_failed',
            'error': ps.stderr.strip() or 'ps failed',
        }

    records = parse_process_table(ps.stdout)
    pids = process_tree(pid, records) if include_children else ([pid] if pid in records else [])
    if not pids:
        return {
            'timestamp': timestamp,
            'root_pid': pid,
            'include_children': include_children,
            'sample_status': 'root_missing',
            'error': 'root process was not present in the ps snapshot',
        }

    processes: list[dict[str, object]] = []
    for process_id in pids:
        process = records[process_id]
        row: dict[str, object] = {
            'pid': process.pid,
            'ppid': process.ppid,
            'command': process.command,
            'rss_kb': process.rss_kb,
            'cpu_percent': process.cpu_percent,
        }
        if include_vmmap:
            row['vmmap_summary'] = capture(['/usr/bin/vmmap', '-summary', str(process_id)])
        processes.append(row)

    footprint_command = ['/usr/bin/footprint', '--format', 'bytes', '--noCategories']
    for process_id in pids:
        footprint_command.extend(['--pid', str(process_id)])

    return {
        'timestamp': timestamp,
        'root_pid': pid,
        'include_children': include_children,
        'sample_status': 'ok',
        'cpu_percent_semantics': 'ps %cpu is a decaying average over up to 60 seconds, not interval CPU',
        'process_count': len(processes),
        'total_rss_kb': sum(int(row['rss_kb']) for row in processes),
        'total_cpu_percent': round(sum(float(row['cpu_percent']) for row in processes), 2),
        'physical_footprint_tree': {
            **capture(footprint_command),
            'scope': 'one footprint report for the PIDs in this ps snapshot; shared objects are de-duplicated',
        },
        'processes': processes,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('pid', type=int, help='PID of the app or browser process to sample')
    parser.add_argument('--duration', type=float, default=0, help='sampling duration in seconds (default: one sample)')
    parser.add_argument('--interval', type=float, default=5, help='sampling interval in seconds')
    parser.add_argument('--include-children', action='store_true', help='include currently observed descendant processes')
    parser.add_argument('--include-vmmap', action='store_true', help='include vmmap -summary for each sampled process')
    parser.add_argument('--output', type=Path, help='append JSON Lines to this local file; otherwise write to stdout')
    args = parser.parse_args()
    if (
        args.pid <= 0
        or not math.isfinite(args.duration)
        or not math.isfinite(args.interval)
        or args.duration < 0
        or args.interval <= 0
    ):
        parser.error('pid and interval must be positive; duration cannot be negative; values must be finite')

    try:
        stream = open_private_output(args.output) if args.output else sys.stdout
    except OSError as error:
        parser.error(f'cannot open output: {error}')
    deadline = time.monotonic() + args.duration
    try:
        while True:
            observation = sample(args.pid, args.include_children, args.include_vmmap)
            print(json.dumps(observation, ensure_ascii=False), file=stream, flush=True)
            if args.duration == 0 or time.monotonic() >= deadline:
                break
            time.sleep(min(args.interval, max(0, deadline - time.monotonic())))
    finally:
        if args.output:
            stream.close()
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
