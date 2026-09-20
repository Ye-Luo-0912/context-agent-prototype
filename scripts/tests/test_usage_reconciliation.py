"""Temporary-only reconciliation fixtures; never touch campaign evidence or APIs."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import _support as support

MODULE = Path(__file__).resolve().parents[1] / 'package_endurance_v4/reconcile_usage.py'
SPEC = importlib.util.spec_from_file_location('usage_reconciliation', MODULE)
reconcile = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(reconcile)


def write(path, value):
    path.write_text(json.dumps(value), encoding='utf-8')


class UsageReconciliationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.stage = Path(self.temp.name) / 'model'
        self.segment = self.stage / 'model-test'
        self.segment.mkdir(parents=True)
        self.ledger_path = self.stage / 'budget-ledger.json'
        ledger, _ = support.runner.BudgetLedger.load(self.ledger_path, 1.0, support.runner.Pricing(),
                                                     8192, api_protocol='chat')
        self.target, _, _ = ledger.reserve(1, 1000, self.segment.name)
        ledger.settle(self.target, 'unknown', detail='usage_missing', http_status=200)
        self.other, _, _ = ledger.reserve(2, 1000, self.segment.name)
        ledger.settle(self.other, 'unknown', detail='upstream_failure:TimeoutError', http_status=200)
        self.original_ledger = self.ledger_path.read_bytes()
        write(self.segment / 'metadata.json', dict(api_protocol='chat', model='stub', segment=self.segment.name,
            outcome='completed', child_exit_code=1, ledger_path=str(self.ledger_path),
            cleanup={'child': {'tree_confirmed': True}}))
        write(self.segment / 'summary.json', dict(api_protocol='chat', outcome='completed'))
        write(self.segment / 'usage-ledger.json', dict(api_protocol='chat', rows=copy.deepcopy(ledger.data['attempts'])))
        write(self.segment / 'request-001.json', dict(model='stub', stream=True,
            messages=[dict(role='user', content='fixture')], stream_options={'include_usage': True}))
        self.response = self.segment / 'response-001.sse'
        self.response.write_bytes(
            b'data: {"choices":[{"finish_reason":"length"}]}\n\n'
            b'data: {"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":10,"prompt_cache_hit_tokens":40}}\n\n'
            b'data: [DONE]\n\n')
        self.original_receipts = {name: (self.segment / name).read_bytes() for name in reconcile.RECEIPTS}

    def plan(self):
        return reconcile.build_plan(self.stage, self.segment.name, self.target)

    def test_plan_is_read_only_and_scan_never_reads_full_response(self):
        before = sorted(str(path.relative_to(self.stage)) for path in self.stage.rglob('*'))
        original_read = Path.read_bytes

        def read(path):
            self.assertNotEqual(path, self.response, 'response must use bounded reads')
            return original_read(path)

        with patch.object(Path, 'read_bytes', read):
            plan = self.plan()
        self.assertEqual(plan['bindings'][f'{self.segment.name}/response-001.sse'],
                         hashlib.sha256(self.response.read_bytes()).hexdigest())
        self.assertEqual(plan['usage']['output_tokens'], 10)
        self.assertEqual(before, sorted(str(path.relative_to(self.stage)) for path in self.stage.rglob('*')))
        self.assertEqual(self.ledger_path.read_bytes(), self.original_ledger)

    def test_apply_preserves_original_evidence_other_unknown_and_is_idempotent(self):
        plan = self.plan()
        before = json.loads(self.original_ledger)
        receipt = reconcile.apply_plan(plan)
        self.assertEqual(receipt['status'], 'APPLIED')
        ledger = json.loads(self.ledger_path.read_bytes())
        self.assertEqual(ledger['attempts'][1], before['attempts'][1])
        self.assertEqual(ledger['unknown_usd'], before['attempts'][1]['settled_usd'])
        self.assertEqual(ledger['committed_usd'], plan['settled_usd'])
        self.assertEqual(ledger['reserved_usd'], before['reserved_usd'])
        self.assertEqual(ledger['cap_stopped'], before['cap_stopped'])
        journal = self.stage / 'usage-reconciliations' / plan['plan_id']
        self.assertEqual((journal / 'budget-ledger.before.json').read_bytes(), self.original_ledger)
        for name, raw in self.original_receipts.items():
            self.assertEqual((self.segment / name).read_bytes(), raw)
            self.assertEqual((journal / (Path(name).stem + '.before.json')).read_bytes(), raw)
        after = self.ledger_path.read_bytes()
        self.assertTrue(reconcile.apply_plan(plan)['already_applied'])
        self.assertEqual(self.ledger_path.read_bytes(), after)

    def test_changed_capture_request_or_ledger_refuses_without_accounting_write(self):
        for path in (self.response, self.segment / 'request-001.json', self.ledger_path):
            with self.subTest(path=path.name):
                original = path.read_bytes()
                plan = self.plan()
                path.write_bytes(original + b'\n')
                before = self.ledger_path.read_bytes()
                with self.assertRaises(ValueError):
                    reconcile.apply_plan(plan)
                self.assertEqual(self.ledger_path.read_bytes(), before)
                self.assertFalse((self.stage / 'usage-reconciliations').exists())
                path.write_bytes(original)

    def test_other_unknown_reason_protocol_and_request_identity_cannot_be_reconciled(self):
        with self.assertRaisesRegex(ValueError, 'usage_missing'):
            reconcile.build_plan(self.stage, self.segment.name, self.other)
        request_path = self.segment / 'request-001.json'
        original = request_path.read_bytes()
        request = json.loads(original)
        request['model'] = 'different-model'
        write(request_path, request)
        with self.assertRaisesRegex(ValueError, 'model'):
            self.plan()
        request_path.write_bytes(original)
        ledger = json.loads(self.original_ledger)
        ledger['api_protocol'] = 'responses'
        write(self.ledger_path, ledger)
        with self.assertRaisesRegex(ValueError, 'protocol'):
            self.plan()

    def test_chat_requires_terminal_and_done_and_rejects_malformed_capture(self):
        original = self.response.read_bytes()
        variants = (original.replace(b'data: [DONE]\n\n', b''),
                    original.replace(b'data: {"choices":[{"finish_reason":"length"}]}\n\n', b''),
                    b'data: {broken}\n\n' + original)
        for raw in variants:
            with self.subTest(raw_length=len(raw)):
                self.response.write_bytes(raw)
                with self.assertRaisesRegex(ValueError, 'terminal'):
                    self.plan()
        self.assertEqual(self.ledger_path.read_bytes(), self.original_ledger)

    def test_responses_complete_terminal_without_done_can_be_reconciled(self):
        ledger = json.loads(self.original_ledger)
        ledger['api_protocol'] = 'responses'
        for row in ledger['attempts']:
            row['api_protocol'] = 'responses'
        write(self.ledger_path, ledger)
        for name in reconcile.RECEIPTS:
            path = self.segment / name
            value = json.loads(path.read_bytes())
            value['api_protocol'] = 'responses'
            if name == 'usage-ledger.json':
                value['rows'] = ledger['attempts']
            write(path, value)
        write(self.segment / 'request-001.json', dict(model='stub', stream=True, input='fixture'))
        self.response.write_bytes(b'data: ' + json.dumps(dict(type='response.completed', response={
            'usage': dict(input_tokens=100, output_tokens=10, cached_input_tokens=40)})).encode() + b'\n\n')
        self.assertEqual(self.plan()['api_protocol'], 'responses')
        self.response.write_bytes(self.response.read_bytes() + b'data: {"type":"response.output_text.delta"}\n\n')
        with self.assertRaisesRegex(ValueError, 'terminal'):
            self.plan()

    def test_retry_after_ledger_write_only_finishes_journal_without_double_settlement(self):
        plan = self.plan()
        write_json = reconcile._write_json

        def fail_final_receipt(path, payload):
            if path.name == 'receipt.json' and payload.get('status') == 'APPLIED':
                raise OSError('injected final journal write failure')
            return write_json(path, payload)

        with patch.object(reconcile, '_write_json', side_effect=fail_final_receipt):
            with self.assertRaisesRegex(OSError, 'journal write'):
                reconcile.apply_plan(plan)
        after = self.ledger_path.read_bytes()
        receipt = reconcile.apply_plan(plan)
        self.assertTrue(receipt['already_applied'])
        self.assertTrue(receipt['recovered_after_write'])
        self.assertEqual(receipt['settled_usd'], plan['settled_usd'])
        self.assertEqual(self.ledger_path.read_bytes(), after)


if __name__ == '__main__':
    unittest.main()
