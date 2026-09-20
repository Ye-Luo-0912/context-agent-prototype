import importlib.util
import json
from pathlib import Path
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

MODULE=Path(__file__).resolve().parents[1]/'load.py'
spec=importlib.util.spec_from_file_location('snapshot_load_controller',MODULE)
load=importlib.util.module_from_spec(spec);spec.loader.exec_module(load)


class LoadGuards(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.campaign=Path(self.temp.name);self.stage=self.campaign/'model';self.work=self.stage/'workspace'
        (self.work/'app').mkdir(parents=True)
        (self.work/'app/snapshot.py').write_text('# unimplemented hash-only guard fixture')
        (self.work/'SPEC.md').write_text('protected spec')
        now=time.time();self.caps=dict(kind='v4_real_snapshot_workflow',created_epoch=now,deadline_epoch=now+14400)
        (self.campaign/'campaign.json').write_text(json.dumps(self.caps))
        self.acceptance=self.campaign/'accepted.json'
        self.gate=dict(status='PASS',results=[dict(case='controller_fixture',status='PASS')],
                       candidate_sha256=load.oracle.sha((self.work/'app/snapshot.py').read_bytes()),
                       oracle_sha256=load.oracle.sha(Path(load.oracle.__file__).read_bytes()),
                       source_identity=load.identity(self.work))
        self.acceptance.write_text(json.dumps(self.gate))

    def test_only_current_independent_pass_can_start(self):
        self.assertEqual(load.preflight(self.stage,self.acceptance,300)[0],self.work)
        self.gate['results'][0]['status']='FAIL';self.acceptance.write_text(json.dumps(self.gate))
        with self.assertRaisesRegex(ValueError,'has not passed'):load.preflight(self.stage,self.acceptance,300)
        self.gate['results'][0]['status']='PASS';self.acceptance.write_text(json.dumps(self.gate))
        (self.work/'app/snapshot.py').write_text('# changed hash-only fixture')
        with self.assertRaisesRegex(ValueError,'changed after'):load.preflight(self.stage,self.acceptance,300)

    def test_duration_and_original_deadline_are_not_extended(self):
        for seconds in (0,301,True):
            with self.assertRaises(ValueError):load.preflight(self.stage,self.acceptance,seconds)
        self.caps['created_epoch']=time.time()-14300;self.caps['deadline_epoch']=self.caps['created_epoch']+14400
        (self.campaign/'campaign.json').write_text(json.dumps(self.caps))
        with self.assertRaisesRegex(ValueError,'does not cover'):load.preflight(self.stage,self.acceptance,300)

    def test_changed_oracle_or_fixture_cannot_reuse_a_prior_pass(self):
        self.gate['oracle_sha256']='0'*64;self.acceptance.write_text(json.dumps(self.gate))
        with self.assertRaisesRegex(ValueError,'oracle changed'):load.preflight(self.stage,self.acceptance,300)
        self.gate['oracle_sha256']=load.oracle.sha(Path(load.oracle.__file__).read_bytes())
        self.acceptance.write_text(json.dumps(self.gate));(self.work/'SPEC.md').write_text('changed contract')
        with self.assertRaisesRegex(ValueError,'workspace inputs changed'):load.preflight(self.stage,self.acceptance,300)

    def test_standalone_local_validation_preserves_expired_campaign_and_requires_acceptance(self):
        self.caps['created_epoch']=1;self.caps['deadline_epoch']=14401
        campaign=self.campaign/'campaign.json'
        campaign.write_text(json.dumps(self.caps));before=campaign.read_bytes()
        with self.assertRaisesRegex(ValueError,'does not cover'):
            load.preflight(self.stage,self.acceptance,300)
        work,deadline,_=load.preflight(self.stage,self.acceptance,300,local_only=True)
        self.assertEqual(work,self.work)
        self.assertLessEqual(deadline-time.time(),480)
        self.assertEqual(campaign.read_bytes(),before)
        for seconds in (0,301,True):
            with self.assertRaises(ValueError):load.preflight(self.stage,self.acceptance,seconds,local_only=True)
        self.gate['status']='FAIL';self.acceptance.write_text(json.dumps(self.gate))
        with patch.object(load,'WINDOWS',True),patch.object(load,'spawn_windows_owned') as spawn:
            result=load.run(self.stage,self.campaign/'local-load',self.acceptance,300,local_only=True)
        spawn.assert_not_called()
        self.assertEqual(result['validation_scope'],'standalone_local_only')
        self.assertEqual(result['provider_paid_calls'],0)

    def test_rejected_gate_records_failure_without_starting_processes(self):
        self.gate['status']='FAIL';self.acceptance.write_text(json.dumps(self.gate))
        with patch.object(load,'WINDOWS',True),patch.object(load,'spawn_windows_owned') as spawn:
            result=load.run(self.stage,self.campaign/'load',self.acceptance,300)
        spawn.assert_not_called();self.assertEqual(result['status'],'FAIL')
        self.assertEqual(result['provider_paid_calls'],0)

    def test_failed_launch_process_is_retried_without_losing_uncertainty(self):
        process=SimpleNamespace(stdin=None,stdout=None,stderr=None)
        failed=dict(tree_confirmed=False,outcome='unconfirmed',scope='windows_launch',errors={'wait':'injected'})
        error=load.WindowsLaunchCleanupError(OSError('launch failed'),process,failed);cleanups=[]
        with patch.object(load,'spawn_windows_owned',side_effect=error),patch.object(load,'stop_windows_owned',return_value=dict(tree_confirmed=True)) as stop:
            with self.assertRaises(load.WindowsLaunchCleanupError):
                load.Worker(self.work,self.work,self.campaign/'worker',cleanups)
        stop.assert_called_once_with(process,2,8)
        self.assertFalse(cleanups[0]['tree_confirmed']);self.assertTrue(cleanups[1]['tree_confirmed'])


if __name__=='__main__':unittest.main()
