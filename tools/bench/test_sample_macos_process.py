import stat
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from sample_macos_process import (
    ProcessRecord,
    open_private_output,
    parse_process_table,
    process_tree,
    sample,
)


class ProcessTableTests(unittest.TestCase):
    def test_parse_process_table(self):
        records = parse_process_table(
            "  100 1 2048 2.5 /Applications/Kick Client.app/Contents/MacOS/kick-client\n"
            "  101 100 512 0.0 /usr/libexec/worker\n"
            "  102 100 512 nan /usr/libexec/invalid-cpu\n"
            "  103 100 -1 0.0 /usr/libexec/invalid-rss\n"
            "bad row\n"
        )

        self.assertEqual(
            records[100],
            ProcessRecord(100, 1, 2048, 2.5, "kick-client"),
        )
        self.assertEqual(records[101].command, "worker")
        self.assertEqual(len(records), 2)

    def test_process_tree_includes_descendants_once(self):
        records = {
            100: ProcessRecord(100, 1, 1, 0.0, "root"),
            101: ProcessRecord(101, 100, 1, 0.0, "child"),
            102: ProcessRecord(102, 101, 1, 0.0, "grandchild"),
            103: ProcessRecord(103, 100, 1, 0.0, "sibling"),
        }

        self.assertEqual(process_tree(100, records), [100, 101, 103, 102])
        self.assertEqual(process_tree(999, records), [])

    def test_sample_sums_observed_tree_rss_and_cpu(self):
        ps_output = (
            "100 1 2048 2.5 /Applications/Kick Client.app/Contents/MacOS/kick-client\n"
            "101 100 512 1.0 /usr/libexec/worker\n"
            "102 101 256 0.5 /usr/libexec/worker-child\n"
            "200 1 100 0.0 /Applications/Other.app/Contents/MacOS/other\n"
        )

        commands = []

        def run(command, **kwargs):
            commands.append(command)
            if command[0] == '/bin/ps':
                return subprocess.CompletedProcess(command, 0, stdout=ps_output, stderr='')
            return subprocess.CompletedProcess(command, 0, stdout='100 bytes', stderr='')

        with patch('sample_macos_process.subprocess.run', side_effect=run):
            observation = sample(100, include_children=True, include_vmmap=False)

        self.assertEqual(observation['sample_status'], 'ok')
        self.assertEqual(observation['process_count'], 3)
        self.assertEqual(observation['total_rss_kb'], 2816)
        self.assertEqual(observation['total_cpu_percent'], 4.0)
        self.assertIn('decaying average', observation['cpu_percent_semantics'])
        self.assertEqual([row['pid'] for row in observation['processes']], [100, 101, 102])
        self.assertEqual(observation['physical_footprint_tree']['output'], '100 bytes')
        self.assertEqual(commands[1], [
            '/usr/bin/footprint', '--format', 'bytes', '--noCategories',
            '--pid', '100', '--pid', '101', '--pid', '102',
        ])

    def test_missing_root_is_not_reported_as_zero_use_sample(self):
        ps_output = '200 1 100 0.0 /Applications/Other.app/Contents/MacOS/other\n'
        with patch(
            'sample_macos_process.subprocess.run',
            return_value=subprocess.CompletedProcess(['/bin/ps'], 0, stdout=ps_output, stderr=''),
        ):
            observation = sample(100, include_children=True, include_vmmap=False)

        self.assertEqual(observation['sample_status'], 'root_missing')
        self.assertNotIn('total_rss_kb', observation)
        self.assertNotIn('total_cpu_percent', observation)

    def test_ps_launch_failure_is_recorded(self):
        with patch(
            'sample_macos_process.subprocess.run',
            side_effect=FileNotFoundError('ps unavailable'),
        ):
            observation = sample(100, include_children=False, include_vmmap=False)

        self.assertEqual(observation['sample_status'], 'ps_failed')
        self.assertEqual(observation['error'], 'ps unavailable')


class OutputSafetyTests(unittest.TestCase):
    def test_output_is_append_only_and_private(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'samples.jsonl'
            with open_private_output(path) as output:
                output.write('{"sample_status":"ok"}\n')
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

            path.chmod(0o644)
            with open_private_output(path) as output:
                output.write('{"sample_status":"root_missing"}\n')
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            self.assertEqual(path.read_text(encoding='utf-8').splitlines(), [
                '{"sample_status":"ok"}',
                '{"sample_status":"root_missing"}',
            ])

    def test_output_refuses_a_final_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'target.jsonl'
            link = Path(directory) / 'link.jsonl'
            target.write_text('keep this file\n', encoding='utf-8')
            link.symlink_to(target)

            with self.assertRaises(OSError):
                open_private_output(link)

            self.assertEqual(target.read_text(encoding='utf-8'), 'keep this file\n')

    def test_output_refuses_a_non_regular_target(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(OSError):
                open_private_output(Path(directory))


if __name__ == "__main__":
    unittest.main()
