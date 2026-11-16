import ctypes
import errno
import os
import shutil
import subprocess
import sys
import unittest
from libproc_sample import (
    RUSAGE_INFO_V0,
    RUSAGE_INFO_V4,
    RUsageV4,
    SampleWindowPeak,
    interval_cpu,
    library,
    sample,
    tree_sample,
)


def snapshot(identity=1, cpu=0, time=0):
    return {'status': 'ok', 'monotonic_ns': time, 'coverage_errors': [],
            'processes': [{'pid': 123, 'process_start_identity': identity, 'cpu_time_ns': cpu}]}

class IntervalTests(unittest.TestCase):
    def test_interval_uses_one_core_percent(self):
        self.assertEqual(interval_cpu(snapshot(), snapshot(cpu=500_000_000, time=1_000_000_000)), 50)

    def test_pid_reuse_does_not_count_other_process_cpu(self):
        self.assertIsNone(interval_cpu(snapshot(), snapshot(identity=2, cpu=500, time=1000)))

    def test_topology_change_and_partial_coverage_are_unmeasured(self):
        changed = snapshot(cpu=500, time=1000)
        changed['processes'].append({'pid': 124, 'process_start_identity': 2, 'cpu_time_ns': 100})
        self.assertIsNone(interval_cpu(snapshot(), changed))
        partial = snapshot(cpu=500, time=1000)
        partial['coverage_errors'] = [{'status': 'unavailable'}]
        self.assertIsNone(interval_cpu(snapshot(), partial))

    def test_initial_and_invalid_interval_are_unmeasured(self):
        self.assertIsNone(interval_cpu(None, snapshot()))
        self.assertIsNone(interval_cpu(snapshot(), snapshot(cpu=10)))

class FakeLibproc:
    cpu_numer = 1
    cpu_denom = 1

    def __init__(self, v4_errno=None, v0_errno=None):
        self.v4_errno = v4_errno
        self.v0_errno = v0_errno
        self.flavors = []

    def proc_pid_rusage(self, pid, flavor, output):
        self.flavors.append(flavor)
        if flavor == RUSAGE_INFO_V4:
            if self.v4_errno is not None:
                ctypes.set_errno(self.v4_errno)
                return -1
            usage = ctypes.cast(output, ctypes.POINTER(RUsageV4)).contents
            usage.ri_user_time = 3_000_000
            usage.ri_system_time = 2_000_000
            usage.ri_resident_size = 1_048_576
            usage.ri_phys_footprint = 2_097_152
            usage.ri_proc_start_abstime = 99
            usage.ri_lifetime_max_phys_footprint = 8_388_608
            return 0
        if flavor == RUSAGE_INFO_V0:
            if self.v0_errno is not None:
                ctypes.set_errno(self.v0_errno)
                return -1
            # The existing v0 call layout is 96 bytes, with the footprint
            # fields at the same offsets as the v4 prefix.
            usage = ctypes.cast(output, ctypes.POINTER(ctypes.c_uint64))
            usage[2] = 3_000_000
            usage[3] = 2_000_000
            usage[8] = 1_048_576
            usage[9] = 2_097_152
            usage[10] = 99
            return 0
        raise AssertionError(f'unexpected libproc flavor: {flavor}')

class SampleTests(unittest.TestCase):
    def test_v4_carries_lifetime_peak_as_a_per_process_value(self):
        lib = FakeLibproc()
        row = sample(lib, 123)
        self.assertEqual(lib.flavors, [RUSAGE_INFO_V4])
        self.assertEqual(row['status'], 'ok')
        self.assertEqual(row['usage_flavor'], 'v4')
        self.assertEqual(row['physical_footprint_bytes'], 2_097_152)
        self.assertEqual(row['process_lifetime_max_physical_footprint_bytes'], 8_388_608)
        self.assertEqual(row['process_lifetime_max_physical_footprint_status'], 'ok')

    def test_unsupported_v4_falls_back_to_v0_and_marks_peak_unsupported(self):
        lib = FakeLibproc(v4_errno=errno.EINVAL)
        row = sample(lib, 123)
        self.assertEqual(lib.flavors, [RUSAGE_INFO_V4, RUSAGE_INFO_V0])
        self.assertEqual(row['status'], 'ok')
        self.assertEqual(row['usage_flavor'], 'v0')
        self.assertEqual(row['physical_footprint_bytes'], 2_097_152)
        self.assertIsNone(row['process_lifetime_max_physical_footprint_bytes'])
        self.assertEqual(row['process_lifetime_max_physical_footprint_status'], 'unsupported')
        self.assertEqual(row['process_lifetime_max_physical_footprint_errno'], errno.EINVAL)

    def test_non_flavor_v4_error_is_unavailable_not_unsupported(self):
        row = sample(FakeLibproc(v4_errno=errno.EACCES), 123)
        self.assertEqual(row['usage_flavor'], 'v0')
        self.assertEqual(row['process_lifetime_max_physical_footprint_status'], 'unavailable')

    def test_sample_window_uses_current_complete_tree_footprints(self):
        window = SampleWindowPeak()
        first = {
            'status': 'ok', 'timestamp_utc': 't0', 'physical_footprint_bytes': 100,
            'coverage_errors': [],
            'processes': [
                {'process_lifetime_max_physical_footprint_bytes': 10_000},
                {'process_lifetime_max_physical_footprint_bytes': 20_000},
            ],
        }
        second = {**first, 'timestamp_utc': 't1', 'physical_footprint_bytes': 250}
        self.assertEqual(window.observe(first)['sample_window_peak_physical_footprint_bytes'], 100)
        result = window.observe(second)
        self.assertEqual(result['sample_window_peak_physical_footprint_bytes'], 250)
        self.assertEqual(result['sample_window_peak_status'], 'ok')
        self.assertEqual(result['sample_window_peak_sample_count'], 2)
        self.assertEqual(result['sample_window_started_at'], 't0')

    def test_incomplete_sample_marks_window_peak_partial(self):
        window = SampleWindowPeak()
        row = {
            'status': 'ok', 'timestamp_utc': 't0', 'physical_footprint_bytes': 100,
            'coverage_errors': [],
        }
        window.observe(row)
        result = window.observe({**row, 'timestamp_utc': 't1', 'coverage_errors': [{'status': 'unavailable'}]})
        self.assertEqual(result['sample_window_peak_physical_footprint_bytes'], 100)
        self.assertEqual(result['sample_window_peak_status'], 'partial')
        self.assertEqual(result['sample_window_peak_sample_count'], 1)
        self.assertEqual(result['sample_window_incomplete_sample_count'], 1)

@unittest.skipUnless(sys.platform == 'darwin' and shutil.which('xcrun'), 'macOS SDK compiler unavailable')
class SDKLayoutTests(unittest.TestCase):
    def test_v4_ctypes_layout_matches_installed_macos_sdk(self):
        checks = [
            f'_Static_assert(sizeof(struct rusage_info_v4) == {ctypes.sizeof(RUsageV4)}, "size");'
        ]
        for field, _ in RUsageV4._fields_:
            offset = getattr(RUsageV4, field).offset
            checks.append(
                f'_Static_assert(offsetof(struct rusage_info_v4, {field}) == {offset}, "{field} offset");'
            )
        source = '\n'.join([
            '#include <stddef.h>',
            '#include <sys/resource.h>',
            *checks,
            'int main(void) { return 0; }',
        ])
        result = subprocess.run(
            ['xcrun', '--sdk', 'macosx', 'clang', '-std=c11', '-D_DARWIN_C_SOURCE',
             '-fsyntax-only', '-x', 'c', '-'],
            input=source,
            text=True,
            capture_output=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

@unittest.skipUnless(sys.platform == 'darwin', 'libproc is macOS-specific')
class NativeTreeTests(unittest.TestCase):
    def test_own_child_is_present_without_commandline_capture(self):
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(10)'])
        try:
            row = tree_sample(library(), os.getpid(), True)
            self.assertEqual(row['status'], 'ok')
            self.assertIn(child.pid, [p['pid'] for p in row['processes']])
            self.assertTrue(all('command' not in p and 'uuid' not in p for p in row['processes']))
            self.assertEqual(row['rss_bytes'], sum(p['rss_bytes'] for p in row['processes']))
        finally:
            child.terminate()
            child.wait(timeout=3)

if __name__ == '__main__':
    unittest.main()
