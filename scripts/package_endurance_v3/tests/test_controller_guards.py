import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch
from types import SimpleNamespace

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from common import create_campaign,campaign_limits,identity,oracle_identity,write_json
import load
import journey
from runner_process_tree import WindowsLaunchCleanupError


class ControllerGuards(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.campaign=Path(self.temp.name);self.stage=self.campaign/'stage';self.work=self.stage/'workspace'
        (self.work/'app').mkdir(parents=True);(self.work/'SPEC.md').write_text('fixed contract')
        (self.work/'app/source.py').write_text('version=1')
        create_campaign(self.campaign,time.time())
        write_json(self.stage/'verification/accepted/receipt.json',dict(status='PASS',source_identity=identity(self.work),oracle_identity=oracle_identity()))

    def test_touch_cannot_reset_content_deadline(self):
        first=campaign_limits(self.stage)['deadline_epoch'];path=self.campaign/'campaign.json'
        os.utime(path,(time.time()+18000,time.time()+18000))
        self.assertEqual(campaign_limits(self.stage)['deadline_epoch'],first)

    def test_old_pass_cannot_authorize_modified_source(self):
        (self.work/'app/source.py').write_text('version=2')
        with patch.object(load,'Worker') as worker:
            with self.assertRaisesRegex(ValueError,'identity changed'):load.run(self.stage,20,.2)
            worker.assert_not_called()
        self.assertFalse((self.stage/'load').exists())

    def test_changed_oracle_refuses_before_launch(self):
        p=self.stage/'verification/accepted/receipt.json';r=json.loads(p.read_bytes());r['oracle_identity']={};write_json(p,r)
        with patch.object(load,'Worker') as worker:
            with self.assertRaisesRegex(AssertionError,'oracle changed'):load.run(self.stage,20,.2)
            worker.assert_not_called()

    def launch_failure(self, process=True):
        child=SimpleNamespace(stdin=None,stdout=None) if process else None
        cleanup=dict(outcome='unconfirmed',tree_confirmed=False,scope='windows_launch',
                     errors={'job_close':'injected close failure'})
        return WindowsLaunchCleanupError(OSError('injected resume failure'),child,cleanup)

    def test_worker_adopts_failed_launch_process_and_retains_cleanup(self):
        failure=self.launch_failure()
        stopped=dict(outcome='terminated_confirmed',tree_confirmed=True,scope='windows_job')
        with patch.object(load,'spawn_windows_owned',side_effect=failure),patch.object(load,'stop_windows_owned',return_value=stopped) as stop:
            with self.assertRaises(WindowsLaunchCleanupError):
                load.Worker(self.work,self.stage/'db',self.stage,'failed-owned')
        stop.assert_called_once_with(failure.process,0,10)
        receipt=json.loads((self.stage/'failed-owned.startup-cleanup.json').read_bytes())
        self.assertFalse(receipt['launch']['tree_confirmed'])
        self.assertTrue(receipt['retry']['tree_confirmed'])

    def test_load_reports_failed_launch_cleanup_as_unconfirmed(self):
        (self.work/'fixtures').mkdir();(self.work/'fixtures/catalog.json').write_text('{}')
        write_json(self.stage/'verification/accepted/receipt.json',dict(status='PASS',source_identity=identity(self.work),oracle_identity=oracle_identity()))
        failure=self.launch_failure()
        with patch.object(load,'Worker',side_effect=failure):
            receipt=load.run(self.stage,20,.2)
        self.assertEqual(receipt['status'],'CLEANUP_UNCONFIRMED')
        self.assertEqual(receipt['cleanup'][0]['scope'],'windows_launch')
        self.assertFalse(receipt['cleanup'][0]['tree_confirmed'])

    def test_host_adopts_failed_launch_process_and_retains_cleanup(self):
        failure=self.launch_failure()
        stopped=dict(outcome='terminated_confirmed',tree_confirmed=True,scope='windows_job')
        out=self.stage/'host-failed'
        with patch.object(journey,'spawn_windows_owned',side_effect=failure),patch.object(journey,'stop_windows_owned',return_value=stopped) as stop:
            with self.assertRaises(WindowsLaunchCleanupError):
                journey.OwnedHost(self.work,out,12345)
        stop.assert_called_once_with(failure.process,2,10)
        receipt=json.loads((out/'cleanup.json').read_bytes())
        self.assertFalse(receipt['launch']['tree_confirmed'])
        self.assertTrue(receipt['tree_confirmed'])

    def test_host_failed_launch_without_process_does_not_claim_clean_exit(self):
        failure=self.launch_failure(process=False);out=self.stage/'host-no-child'
        with patch.object(journey,'spawn_windows_owned',side_effect=failure),patch.object(journey,'stop_windows_owned') as stop:
            with self.assertRaises(WindowsLaunchCleanupError):
                journey.OwnedHost(self.work,out,12345)
        stop.assert_not_called()
        receipt=json.loads((out/'cleanup.json').read_bytes())
        self.assertFalse(receipt['tree_confirmed'])
        self.assertEqual(receipt['scope'],'windows_launch')

    def test_launch_cleanup_retry_error_preserves_owned_failure(self):
        for module in (load,journey):
            with self.subTest(controller=module.__name__):
                failure=self.launch_failure();label='retry-'+module.__name__
                with patch.object(module,'spawn_windows_owned',side_effect=failure),patch.object(module,'stop_windows_owned',side_effect=OSError('retry close failed')):
                    with self.assertRaises(WindowsLaunchCleanupError) as caught:
                        if module is load:load.Worker(self.work,self.stage/'db',self.stage,label)
                        else:journey.OwnedHost(self.work,self.stage/label,12345)
                self.assertIs(caught.exception,failure)
                if module is load:
                    receipt=json.loads((self.stage/(label+'.startup-cleanup.json')).read_bytes())['retry']
                else:
                    receipt=json.loads((self.stage/label/'cleanup.json').read_bytes())
                self.assertFalse(receipt['tree_confirmed'])
                self.assertIn('retry close failed',receipt['error'])

    def test_host_journey_reports_failed_launch_cleanup_as_unconfirmed(self):
        failure=self.launch_failure()
        with patch.object(journey,'OwnedHost',side_effect=failure),patch.object(journey,'ThreadingHTTPServer') as server:
            server.return_value.server_port=12345
            receipt=journey.run(self.stage,'failed-launch')
        self.assertEqual(receipt['status'],'CLEANUP_UNCONFIRMED')
        self.assertEqual(receipt['launch_cleanup']['scope'],'windows_launch')
        self.assertFalse(receipt['launch_cleanup']['tree_confirmed'])

    @unittest.skipUnless(os.name=='nt','Windows Job ownership')
    def test_failed_startup_reaps_worker_before_constructor_raises(self):
        owned=[];spawn=load.spawn_windows_owned
        def start(_command,**kwargs):
            p=spawn([sys.executable,'-c','import time;time.sleep(60)'],**kwargs);owned.append(p);return p
        with patch.object(load,'spawn_windows_owned',start),patch.object(load.Worker,'next',side_effect=TimeoutError('no startup handshake')):
            with self.assertRaisesRegex(TimeoutError,'no startup'):load.Worker(self.work,self.stage/'db',self.stage,'failed')
        self.assertEqual(len(owned),1);self.assertIsNotNone(owned[0].poll())
        self.assertIsNone(owned[0]._runner_job.handle)


if __name__=='__main__':unittest.main()
