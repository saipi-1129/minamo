#!/usr/bin/env python3
"""Read per-process macOS memory without ps/sysmond; emits no command lines."""
import argparse
import ctypes
import errno
import json
import os
import sys
import time
from datetime import datetime, timezone

RUSAGE_INFO_V0 = 0
RUSAGE_INFO_V4 = 4

class RUsageV0(ctypes.Structure):
    # SDK sys/resource.h: rusage_info_v0, RUSAGE_INFO_V0 = 0.
    _fields_ = [('uuid', ctypes.c_uint8 * 16)] + [
        (name, ctypes.c_uint64) for name in (
            'user_time', 'system_time', 'idle_wakeups', 'interrupt_wakeups',
            'pageins', 'wired_size', 'resident_size', 'physical_footprint',
            'process_start', 'process_exit')]

RUSAGE_V4_FIELDS = (
    'ri_user_time', 'ri_system_time', 'ri_pkg_idle_wkups',
    'ri_interrupt_wkups', 'ri_pageins', 'ri_wired_size',
    'ri_resident_size', 'ri_phys_footprint', 'ri_proc_start_abstime',
    'ri_proc_exit_abstime', 'ri_child_user_time', 'ri_child_system_time',
    'ri_child_pkg_idle_wkups', 'ri_child_interrupt_wkups',
    'ri_child_pageins', 'ri_child_elapsed_abstime', 'ri_diskio_bytesread',
    'ri_diskio_byteswritten', 'ri_cpu_time_qos_default',
    'ri_cpu_time_qos_maintenance', 'ri_cpu_time_qos_background',
    'ri_cpu_time_qos_utility', 'ri_cpu_time_qos_legacy',
    'ri_cpu_time_qos_user_initiated', 'ri_cpu_time_qos_user_interactive',
    'ri_billed_system_time', 'ri_serviced_system_time', 'ri_logical_writes',
    'ri_lifetime_max_phys_footprint', 'ri_instructions', 'ri_cycles',
    'ri_billed_energy', 'ri_serviced_energy',
    'ri_interval_max_phys_footprint', 'ri_runnable_time',
)

class RUsageV4(ctypes.Structure):
    # Match struct rusage_info_v4 in sys/resource.h, including its full size.
    _fields_ = [('ri_uuid', ctypes.c_uint8 * 16)] + [
        (name, ctypes.c_uint64) for name in RUSAGE_V4_FIELDS]

class Timebase(ctypes.Structure):
    _fields_ = [('numer', ctypes.c_uint32), ('denom', ctypes.c_uint32)]

def library():
    lib = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    lib.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    lib.proc_pid_rusage.restype = ctypes.c_int
    lib.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
    lib.proc_listchildpids.restype = ctypes.c_int
    system = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
    system.mach_timebase_info.argtypes = [ctypes.POINTER(Timebase)]
    timebase = Timebase()
    if system.mach_timebase_info(ctypes.byref(timebase)) != 0 or not timebase.denom:
        raise OSError('Mach timebase unavailable')
    lib.cpu_numer, lib.cpu_denom = timebase.numer, timebase.denom
    return lib

def _unsupported_rusage_flavor(error_number):
    unsupported_errors = {
        errno.EINVAL,
        getattr(errno, 'ENOTSUP', errno.EINVAL),
        getattr(errno, 'EOPNOTSUPP', errno.EINVAL),
    }
    return error_number in unsupported_errors

def sample(lib, pid):
    usage = RUsageV4()
    ctypes.set_errno(0)
    if lib.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(usage)) == 0:
        user_time, system_time = usage.ri_user_time, usage.ri_system_time
        resident_size = usage.ri_resident_size
        physical_footprint = usage.ri_phys_footprint
        process_start = usage.ri_proc_start_abstime
        lifetime_peak = usage.ri_lifetime_max_phys_footprint
        lifetime_status = 'ok'
        lifetime_errno = None
        flavor = 'v4'
    else:
        v4_errno = ctypes.get_errno()
        usage_v0 = RUsageV0()
        ctypes.set_errno(0)
        if lib.proc_pid_rusage(pid, RUSAGE_INFO_V0, ctypes.byref(usage_v0)) != 0:
            return {
                'status': 'unavailable',
                'errno': ctypes.get_errno(),
                'pid': pid,
                'process_lifetime_max_physical_footprint_bytes': None,
                'process_lifetime_max_physical_footprint_status': (
                    'unsupported' if _unsupported_rusage_flavor(v4_errno)
                    else 'unavailable'),
                'process_lifetime_max_physical_footprint_errno': v4_errno,
            }
        user_time, system_time = usage_v0.user_time, usage_v0.system_time
        resident_size = usage_v0.resident_size
        physical_footprint = usage_v0.physical_footprint
        process_start = usage_v0.process_start
        lifetime_peak = None
        lifetime_status = (
            'unsupported' if _unsupported_rusage_flavor(v4_errno)
            else 'unavailable')
        lifetime_errno = v4_errno
        flavor = 'v0'

    return {
        'status': 'ok',
        'pid': pid,
        'timestamp_utc': datetime.now(timezone.utc).isoformat(),
        'monotonic_ns': time.monotonic_ns(),
        'usage_flavor': flavor,
        'cpu_time_ns': (user_time + system_time) * lib.cpu_numer // lib.cpu_denom,
        'rss_bytes': resident_size,
        'physical_footprint_bytes': physical_footprint,
        'process_lifetime_max_physical_footprint_bytes': lifetime_peak,
        'process_lifetime_max_physical_footprint_status': lifetime_status,
        'process_lifetime_max_physical_footprint_errno': lifetime_errno,
        'process_start_identity': process_start,
    }

def tree_sample(lib, root, include_children):
    pending, seen, rows, errors = [root], set(), [], []
    while pending:
        pid = pending.pop()
        if pid in seen:
            continue
        seen.add(pid)
        row = sample(lib, pid)
        if row['status'] == 'ok':
            rows.append(row)
        else:
            errors.append(row)
        if include_children:
            ctypes.set_errno(0)
            size = lib.proc_listchildpids(pid, None, 0)
            if size < 0 or (size == 0 and ctypes.get_errno()):
                errors.append({'pid': pid, 'status': 'children_unavailable', 'errno': ctypes.get_errno()})
                continue
            capacity = max(32, size + 32)
            while capacity <= 65536:
                buffer = (ctypes.c_int * capacity)()
                count = lib.proc_listchildpids(pid, buffer, ctypes.sizeof(buffer))
                if count < 0:
                    errors.append({'pid': pid, 'status': 'children_unavailable'})
                    break
                if count < capacity:
                    pending.extend(p for p in list(buffer)[:count] if p > 0)
                    break
                capacity *= 2
            else:
                errors.append({'pid': pid, 'status': 'children_truncated'})
        if len(seen) > 65536:
            errors.append({'status': 'tree_limit'})
            break
    root_row = next((r for r in rows if r['pid'] == root), None)
    if root_row is None:
        return {'status': 'root_unavailable', 'pid': root, 'errors': errors}
    return {**root_row, 'include_children': include_children, 'processes': rows,
            'process_count': len(rows), 'coverage_errors': errors,
            'rss_bytes': sum(r['rss_bytes'] for r in rows),
            'physical_footprint_bytes': sum(r['physical_footprint_bytes'] for r in rows)}

def interval_cpu(previous, current):
    if not previous or current.get('status') != 'ok':
        return None
    before = {(r['pid'], r['process_start_identity']): r for r in previous.get('processes', [])}
    after = {(r['pid'], r['process_start_identity']): r for r in current.get('processes', [])}
    if before.keys() != after.keys() or current.get('coverage_errors') or previous.get('coverage_errors'):
        return None
    elapsed = current['monotonic_ns'] - previous['monotonic_ns']
    delta = sum(r['cpu_time_ns'] - before[key]['cpu_time_ns'] for key, r in after.items())
    return 100 * delta / elapsed if elapsed > 0 and delta >= 0 else None

class SampleWindowPeak:
    """Track the peak of complete current-footprint snapshots in this run."""
    def __init__(self):
        self.started_at = None
        self.sample_count = 0
        self.peak_sample_count = 0
        self.incomplete_sample_count = 0
        self.peak_bytes = None

    def observe(self, row):
        if row.get('status') != 'ok':
            return {}
        self.sample_count += 1
        if self.started_at is None:
            self.started_at = row.get('timestamp_utc')
        if row.get('coverage_errors'):
            self.incomplete_sample_count += 1
        else:
            self.peak_sample_count += 1
            value = row.get('physical_footprint_bytes')
            if value is not None:
                self.peak_bytes = max(self.peak_bytes or 0, value)
        status = 'unavailable' if self.peak_bytes is None else (
            'partial' if self.incomplete_sample_count else 'ok')
        return {
            'sample_window_started_at': self.started_at,
            'sample_window_sample_count': self.sample_count,
            'sample_window_peak_sample_count': self.peak_sample_count,
            'sample_window_incomplete_sample_count': self.incomplete_sample_count,
            'sample_window_peak_physical_footprint_bytes': self.peak_bytes,
            'sample_window_peak_status': status,
        }

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--include-children', action='store_true')
    parser.add_argument('--pid', type=int, default=os.getpid())
    parser.add_argument('--seconds', type=int, default=0)
    parser.add_argument('--interval', type=float, default=1)
    args = parser.parse_args()
    if args.pid <= 0 or not 0 <= args.seconds <= 1800 or not 0.1 <= args.interval <= 60:
        parser.error('invalid PID, duration (0–1800s), or interval (0.1–60s)')
    if sys.platform != 'darwin':
        parser.error('macOS only')
    lib = library()
    deadline = time.monotonic() + args.seconds
    identity = None
    previous = None
    window_peak = SampleWindowPeak()
    while True:
        row = tree_sample(lib, args.pid, args.include_children)
        row['interval_cpu_percent_one_core'] = interval_cpu(previous, row)
        if row['status'] == 'ok':
            if identity is not None and row['process_start_identity'] != identity:
                row = {'status': 'pid_reused', 'pid': args.pid}
            else:
                identity = row['process_start_identity']
        if row.get('status') == 'ok':
            row.update(window_peak.observe(row))
        print(json.dumps(row), flush=True)
        previous = row
        if row['status'] != 'ok':
            return 1
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return 0
        time.sleep(min(args.interval, remaining))

if __name__ == '__main__':
    raise SystemExit(main())
