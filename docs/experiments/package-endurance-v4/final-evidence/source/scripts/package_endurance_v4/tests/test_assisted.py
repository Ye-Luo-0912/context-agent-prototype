"""Regressions for assisted repairs; never attributed to model output."""
import importlib.util
from pathlib import Path
import tempfile
import shutil
import unittest
from unittest.mock import patch

path = Path(__file__).resolve().parents[1]/'assisted/app/snapshot.py'
spec = importlib.util.spec_from_file_location('assisted_snapshot', path)
app = importlib.util.module_from_spec(spec)
spec.loader.exec_module(app)


class PublicationTests(unittest.TestCase):
    def test_assisted_module_passes_independent_api_cli_and_crash_oracle(self):
        oracle_spec=importlib.util.spec_from_file_location('assisted_acceptance_oracle',path.parents[2]/'verify.py')
        oracle=importlib.util.module_from_spec(oracle_spec);oracle_spec.loader.exec_module(oracle)
        with tempfile.TemporaryDirectory() as raw:
            root=Path(raw);work=root/'work';(work/'app').mkdir(parents=True)
            shutil.copyfile(path,work/'app/snapshot.py')
            (work/'app/__init__.py').write_bytes(b'')
            result=oracle.verify(work,root/'oracle')
            self.assertEqual(result['status'],'PASS',[r for r in result['results'] if r['status']!='PASS'])
            self.assertEqual(len(result['results']),26)

    def test_windows_refused_rename_retries_only_same_stage(self):
        with tempfile.TemporaryDirectory() as raw:
            root=Path(raw);stage=root/'stage';destination=root/'destination'
            stage.mkdir();(stage/'payload').write_bytes(b'original')
            rename=app.os.rename;attempts=[]
            def refused_once(source, target):
                attempts.append((source,target))
                if len(attempts)==1:
                    error=PermissionError('transient sharing refusal');error.winerror=5
                    raise error
                rename(source,target)
            with patch.object(app.os,'rename',side_effect=refused_once),patch.object(app.time,'sleep') as sleep:
                app.publish_directory(stage,destination)
            self.assertEqual(len(attempts),2);sleep.assert_called_once_with(.01)
            self.assertEqual((destination/'payload').read_bytes(),b'original')

    def test_conflict_after_refusal_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as raw:
            root=Path(raw);stage=root/'stage';destination=root/'destination';stage.mkdir()
            def refuse(*args):
                destination.mkdir();(destination/'unrelated').write_bytes(b'keep')
                error=PermissionError('refused');error.winerror=32;raise error
            with patch.object(app.os,'rename',side_effect=refuse) as rename,patch.object(app.time,'sleep'):
                with self.assertRaisesRegex(ValueError,'destination appeared'):
                    app.publish_directory(stage,destination)
            rename.assert_called_once();self.assertTrue(stage.is_dir())
            self.assertEqual((destination/'unrelated').read_bytes(),b'keep')

    def test_permanent_or_unknown_refusal_is_bounded(self):
        for windows in (False,True):
            with self.subTest(windows=windows),tempfile.TemporaryDirectory() as raw:
                root=Path(raw);stage=root/'stage';stage.mkdir()
                error=PermissionError('permanent refusal')
                if windows:error.winerror=5
                with patch.object(app.os,'rename',side_effect=error) as rename,patch.object(app.time,'sleep') as sleep:
                    with self.assertRaises(PermissionError):app.publish_directory(stage,root/'dest')
                self.assertEqual(rename.call_count,7 if windows else 1)
                self.assertLessEqual(sum(c.args[0] for c in sleep.call_args_list),.63)
                self.assertTrue(stage.is_dir());self.assertFalse((root/'dest').exists())


if __name__=='__main__':unittest.main()
