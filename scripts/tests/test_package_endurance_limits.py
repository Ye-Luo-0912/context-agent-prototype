import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
sys.path.insert(0, str(SCRIPTS / 'package_endurance'))
from limits import CampaignLedger, campaign_lock, counts, deadline
from run import ByteReservedPricing


class PackageLimits(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'baseline-lock.json').write_text('{}')
        self.ledger, _ = CampaignLedger.load(self.root / 'budget-ledger.json', 3, ByteReservedPricing(), 8192)

    def admit(self):
        return self.ledger.reserve(1, 1200, 'model-test')

    def settle(self, identity):
        self.ledger.settle(identity, 'committed', dict(input_tokens=100, cached_input_tokens=20, output_tokens=10))

    def test_unknown_stops_next_paid_request_and_survives_reload(self):
        identity, _, rejected = self.admit()
        self.assertFalse(rejected)
        self.ledger.settle(identity, 'unknown')
        self.ledger, inherited = CampaignLedger.load(self.ledger.path, 3, ByteReservedPricing(), 8192)
        self.assertTrue(inherited)
        self.assertTrue(self.admit()[2])
        self.assertGreater(self.ledger.data['unknown_usd'], 0)

    def test_only_one_inflight_and_settled_next_request_allowed(self):
        identity, _, _ = self.admit()
        self.settle(identity)
        self.assertFalse(self.admit()[2])
        self.assertTrue(self.admit()[2])

    def test_expired_original_clock_refuses(self):
        old = time.time()-4*3600-5
        os.utime(self.root/'baseline-lock.json', (old, old))
        self.assertLess(deadline(self.root), time.time())
        self.assertTrue(self.admit()[2])

    def test_cumulative_token_limit_uses_full_output_reserve(self):
        self.ledger.data['attempts'] = [dict(id=0,status='committed',input_tokens=1,output_tokens=299000)]
        self.assertTrue(self.admit()[2])
        self.assertEqual(self.ledger.data['attempts'][-1]['detail'], 'output token cap')

    def test_tool_admission_reserves_full_runtime_batch(self):
        part = self.root/'model-first';part.mkdir()
        rows = [dict(event=dict(type='tool_started'))]*689
        (part/'events.jsonl').write_text(''.join(json.dumps(r)+'\n' for r in rows))
        self.assertEqual(counts(self.root), (0,689))
        self.assertTrue(self.admit()[2])

    def test_ledger_policy_matches_pricing_implementation(self):
        _, estimate, refused = self.admit()
        self.assertFalse(refused)
        self.assertAlmostEqual(estimate, ((1200+8192)*0.3+8192*1.2)/1000000)
        self.assertIn('request_body_bytes + 8192',self.ledger.data['reserve_policy']['input_estimate'])

    def test_campaign_lock_is_exclusive_and_releases(self):
        with campaign_lock(self.root):
            with self.assertRaises(OSError):
                with campaign_lock(self.root):
                    self.fail('concurrent controller admitted')
        with campaign_lock(self.root):
            pass


if __name__ == '__main__':
    unittest.main()
