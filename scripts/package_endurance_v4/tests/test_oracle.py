import importlib.util
import io
import json
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

MODULE = Path(__file__).resolve().parents[1] / 'verify.py'
spec = importlib.util.spec_from_file_location('snapshot_independent_oracle', MODULE)
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)


class OracleGuards(unittest.TestCase):
    def test_case_artifacts_work_under_a_long_controller_output_path(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            output = root / ('x' * max(8, 125-len(str(root))))
            output.mkdir()
            folder = oracle.case_directory(output, 1)
            source, members, archive = oracle.fixture_case(folder)
            oracle.check_restored(source, members)
            oracle.check_archive(archive, members)
            self.assertLess(len(str(next((source/'manifests').iterdir()))), 260)

    def test_current_sql_scope_and_canonical_json_are_checked(self):
        for mutation in ("UPDATE current SET tenant='x-'||tenant",
                         "UPDATE current SET receipt_json=receipt_json||' '"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as raw:
                root = Path(raw) / 'restored'
                members = oracle.fixture(root)
                with sqlite3.connect(root/'repo.sqlite') as db:
                    db.execute(mutation)
                db.close()
                with self.assertRaisesRegex(AssertionError, 'current SQL scope'):
                    oracle.check_restored(root, members)

    def test_declared_zip_metadata_without_writer_version_overconstraint(self):
        with tempfile.TemporaryDirectory() as raw:
            folder = Path(raw)
            members = oracle.fixture(folder/'source')
            archive = folder/'version10.zip'
            with zipfile.ZipFile(archive, 'w', allowZip64=False) as target:
                for name, data in sorted(members.items()):
                    info = zipfile.ZipInfo(name, (1980,1,1,0,0,0))
                    info.create_system = 3
                    info.external_attr = 0o100644 << 16
                    info.create_version = info.extract_version = 10
                    target.writestr(info, data)
            self.assertNotEqual(archive.read_bytes(), oracle.zip_bytes(members))
            oracle.check_archive(archive, members)
            output = io.BytesIO()
            with zipfile.ZipFile(archive) as original, zipfile.ZipFile(output, 'w') as target:
                for info in original.infolist():
                    info.date_time = (2020,1,1,0,0,0)
                    target.writestr(info, original.read(info.filename))
            archive.write_bytes(output.getvalue())
            with self.assertRaises(AssertionError):
                oracle.check_archive(archive, members)
            with zipfile.ZipFile(archive, 'w') as target:
                for name, data in sorted(members.items()):
                    info = zipfile.ZipInfo(name, (1980,1,1,0,0,0))
                    info.create_system = 3
                    info.external_attr = 0o100644 << 16
                    with target.open(info, 'w', force_zip64=True) as entry:
                        entry.write(data)
            with self.assertRaisesRegex(AssertionError, 'local ZIP extra'):
                oracle.check_archive(archive, members)

    def test_cli_printed_success_without_export_is_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            folder = Path(raw)
            def run(*args, **kwargs):
                with zipfile.ZipFile(folder/'reference.zip') as archive:
                    members = {name:archive.read(name) for name in archive.namelist()}
                return subprocess.CompletedProcess(args[0], 0, json.dumps(dict(ok=True, **oracle.summary(members))), '')
            with patch.object(oracle.subprocess, 'run', side_effect=run):
                with self.assertRaisesRegex(AssertionError, 'did not create'):
                    oracle.check_cli_contract(folder, folder)

    def test_cli_disk_effects_and_readonly_inspection_are_checked(self):
        for mutate in (False, True):
            with self.subTest(mutate=mutate), tempfile.TemporaryDirectory() as raw:
                folder = Path(raw)
                def run(command, **kwargs):
                    op = command[4]
                    path = Path(command[command.index('--archive')+1])
                    if path.exists() and path.read_bytes() == b'bad zip':
                        return subprocess.CompletedProcess(command, 1, '{"ok":false,"error":"bad zip"}', '')
                    with zipfile.ZipFile(folder/'reference.zip') as archive:
                        members = {name:archive.read(name) for name in archive.namelist()}
                    if op == 'export':path.write_bytes(oracle.zip_bytes(members))
                    elif op == 'inspect' and mutate:path.write_bytes(path.read_bytes()+b'unrequested change')
                    elif op == 'restore':oracle.fixture(Path(command[command.index('--destination')+1]))
                    return subprocess.CompletedProcess(command, 0, json.dumps(dict(ok=True, **oracle.summary(members))), '')
                with patch.object(oracle.subprocess, 'run', side_effect=run):
                    if mutate:
                        with self.assertRaisesRegex(AssertionError, 'changed input'):
                            oracle.check_cli_contract(folder, folder)
                    else:
                        self.assertEqual(oracle.check_cli_contract(folder, folder)['cli_operations'], 4)

    def test_early_exit73_without_complete_stage_is_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            folder = Path(raw)
            with patch.object(oracle.subprocess, 'run', return_value=subprocess.CompletedProcess([],73,'','')):
                with self.assertRaisesRegex(AssertionError, 'complete sibling stage'):
                    oracle.check_crash_contract(folder, folder)

    def test_crash_stage_must_survive_retry_unchanged(self):
        for adopt in (False, True):
            with self.subTest(adopt=adopt), tempfile.TemporaryDirectory() as raw:
                folder = Path(raw);stage = folder/'owned-abandoned-stage'
                def run(command, **kwargs):
                    oracle.fixture(stage)
                    return subprocess.CompletedProcess(command, 73, '', '')
                def restore(work, op, **kwargs):
                    destination = Path(kwargs['destination'])
                    if adopt:
                        stage.rename(destination)
                        with zipfile.ZipFile(kwargs['archive']) as archive:
                            members = {name:archive.read(name) for name in archive.namelist()}
                    else:members = oracle.fixture(destination)
                    return dict(ok=True, value=oracle.summary(members))
                with patch.object(oracle.subprocess, 'run', side_effect=run), patch.object(oracle, 'invoke_candidate', side_effect=restore):
                    if adopt:
                        with self.assertRaisesRegex(AssertionError, 'changed input'):
                            oracle.check_crash_contract(folder, folder)
                    else:
                        self.assertTrue(oracle.check_crash_contract(folder, folder)['complete_stage_verified'])

    def test_real_disk_fixture_passes_but_corrupt_referenced_blob_fails(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / 'source#one'
            members = oracle.fixture(root)
            oracle.check_restored(root, members)
            summary = oracle.summary(members)
            self.assertEqual((summary['receipts'], summary['current']), (5, 3))
            victim = next((root / 'objects').iterdir())
            victim.write_bytes(b'changed bytes')
            with self.assertRaises(AssertionError):
                oracle.check_restored(root, members)

    def test_missing_candidate_cannot_pass_negative_only_acceptance(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            work = root / 'workspace'
            (work / 'app').mkdir(parents=True)
            report = oracle.verify(work, root / 'results')
            self.assertEqual(report['status'], 'FAIL')
            self.assertEqual(report['results'][0]['case'], 'candidate_available')
            self.assertEqual(report['results'][0]['status'], 'FAIL')


if __name__ == '__main__':
    unittest.main()
