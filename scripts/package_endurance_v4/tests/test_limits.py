import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest

MODULE = Path(__file__).resolve().parents[1] / 'run.py'
spec = importlib.util.spec_from_file_location('snapshot_campaign_runner', MODULE)
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class SnapshotLimits(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.campaign = Path(self.temp.name)
        self.stage = self.campaign / 'model'
        self.stage.mkdir()
        now = time.time()
        self.caps = dict(kind='v4_real_snapshot_workflow', created_epoch=now, deadline_epoch=now+4*3600,
                         main_decisions=96, provider_attempts=128, tool_attempts=384,
                         input_tokens=4000000, output_tokens=250000, estimated_cost_usd=1.0)
        (self.campaign / 'campaign.json').write_text(json.dumps(self.caps))
        self.ledger, _ = runner.SnapshotLedger.load(self.stage/'budget-ledger.json', 1.0,
                                                   runner.ByteReservedPricing(), 8192, api_protocol='chat')

    def reserve(self):
        return self.ledger.reserve(1, 1000, 'model-test')

    def test_unknown_blocks_further_attempts_and_survives_reload(self):
        ident, _, refused = self.reserve()
        self.assertFalse(refused)
        self.ledger.settle(ident, 'unknown')
        self.ledger, inherited = runner.SnapshotLedger.load(self.ledger.path, 1.0,
                                                           runner.ByteReservedPricing(), 8192, api_protocol='chat')
        self.assertTrue(inherited)
        self.assertTrue(self.reserve()[2])
        self.assertGreater(self.ledger.data['unknown_usd'], 0)

    def test_whole_campaign_limits_apply_to_current_journal(self):
        part = self.stage/'model-before'
        part.mkdir()
        events = [dict(event=dict(type='model_started')) for _ in range(97)]
        (part/'events.jsonl').write_text(''.join(json.dumps(e)+'\n' for e in events))
        self.assertTrue(self.reserve()[2])
        self.assertEqual(self.ledger.data['attempts'][-1]['detail'], 'decision/tool cap')

    def test_deadline_is_content_based_and_cannot_be_extended_by_touch(self):
        self.caps.update(created_epoch=time.time()-5*3600, deadline_epoch=time.time()-3600)
        self.caps['deadline_epoch'] = self.caps['created_epoch']+4*3600
        (self.campaign/'campaign.json').write_text(json.dumps(self.caps))
        self.assertTrue(self.reserve()[2])
        self.assertEqual(self.ledger.data['attempts'][-1]['detail'], 'campaign deadline')

    def test_output_reservation_and_protocol_identity_survive_reload(self):
        self.ledger.data['attempts'] = [dict(id=0, status='committed', input_tokens=1, output_tokens=245000)]
        self.assertTrue(self.reserve()[2])
        self.assertEqual(self.ledger.data['attempts'][-1]['detail'], 'output token cap')
        self.assertEqual(self.ledger.data['api_protocol'], 'chat')


if __name__ == '__main__':
    unittest.main()
