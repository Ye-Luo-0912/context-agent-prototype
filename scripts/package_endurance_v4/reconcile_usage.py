"""Plan/apply one auditable usage_missing reconciliation; never contact a provider.

Plan is read-only. Apply requires the exact reviewed plan and the campaign lock,
preserves original ledger/segment receipts, and never rewrites segment evidence.
"""
import argparse
import copy
from decimal import Decimal
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
sys.path.insert(0, str(ROOT / 'scripts/package_endurance'))
from limits import campaign_lock
from runtime_endurance_incremental_runner import SseUsageCapture, _write_json

MAX_JSON_BYTES = 8 * 1024 * 1024
MAX_CAPTURE_BYTES = 256 * 1024 * 1024
RECEIPTS = ('metadata.json', 'summary.json', 'usage-ledger.json')


def _require(condition, message):
    if not condition:
        raise ValueError(message)


def _sha(raw):
    return hashlib.sha256(raw).hexdigest()


def _canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()


def _json(path):
    _require(path.stat().st_size <= MAX_JSON_BYTES, 'JSON evidence exceeds its size bound')
    raw = path.read_bytes()
    _require(len(raw) <= MAX_JSON_BYTES, 'JSON evidence changed beyond its size bound')
    value = json.loads(raw)
    _require(isinstance(value, dict), 'expected a JSON object')
    return raw, value


def _inside(stage, relative):
    path = stage / relative
    _require(path.resolve().is_relative_to(stage), 'evidence path escapes the campaign stage')
    return path


def _attempt(data, attempt_id):
    _require(type(attempt_id) is int and attempt_id > 0, 'invalid attempt identity')
    rows = [row for row in data.get('attempts', []) if isinstance(row, dict) and row.get('id') == attempt_id]
    _require(len(rows) == 1, 'attempt identity is missing or ambiguous')
    return rows[0]


def _amount(value):
    _require(type(value) in (int, float) and math.isfinite(value) and value >= 0, 'invalid accounting amount')
    return Decimal(str(value))


def _scan_response(path, protocol):
    _require(path.stat().st_size <= MAX_CAPTURE_BYTES, 'response capture exceeds scan bound')
    capture = SseUsageCapture(protocol)
    digest = hashlib.sha256()
    total = 0
    with path.open('rb') as source:
        while chunk := source.read(16384):
            total += len(chunk)
            _require(total <= MAX_CAPTURE_BYTES, 'response capture grew beyond scan bound')
            digest.update(chunk)
            capture.feed(chunk)
    usage, problem = capture.result()
    _require(usage is not None, f'capture has no trustworthy strict usage: {problem}')
    _require(capture.complete, 'capture lacks a complete clean protocol terminal (Chat also requires DONE)')
    return usage, digest.hexdigest(), total


def build_plan(stage, segment, attempt_id):
    """Return a reviewable plan bound to exact original files, with no writes."""
    stage = Path(stage).resolve()
    _require(isinstance(segment, str) and re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]*', segment),
             'invalid segment name')
    ledger_path = _inside(stage, 'budget-ledger.json')
    ledger_raw, ledger = _json(ledger_path)
    attempt = _attempt(ledger, attempt_id)
    _require(attempt.get('segment') == segment, 'attempt belongs to a different segment')
    _require(attempt.get('status') == 'unknown' and attempt.get('detail') == 'usage_missing',
             'only unknown usage_missing attempts can be reconciled')
    _require(not any(row.get('status') == 'reserved' for row in ledger['attempts']),
             'campaign has an in-flight reservation')
    request_number = attempt.get('request')
    _require(type(request_number) is int and request_number > 0, 'invalid request identity')
    _require(type(attempt.get('http_status')) is int and 200 <= attempt['http_status'] < 300,
             'capture was not a successful HTTP response')
    protocol = ledger.get('api_protocol')
    _require(protocol in ('chat', 'responses') and attempt.get('api_protocol') == protocol,
             'ledger and attempt protocol identities do not match')
    bindings = {'budget-ledger.json': _sha(ledger_raw)}
    receipts = {}
    for name in RECEIPTS:
        relative = f'{segment}/{name}'
        raw, receipts[name] = _json(_inside(stage, relative))
        bindings[relative] = _sha(raw)
        _require(receipts[name].get('api_protocol') == protocol, 'segment protocol does not match ledger')
    metadata = receipts['metadata.json']
    _require(metadata.get('segment') == segment and metadata.get('outcome') != 'running'
             and type(metadata.get('child_exit_code')) is int, 'segment is not a finalized matching execution')
    _require(Path(metadata.get('ledger_path', '')).resolve() == ledger_path.resolve(),
             'segment points at a different ledger')
    _require(metadata.get('cleanup', {}).get('child', {}).get('tree_confirmed') is True,
             'segment process cleanup is not confirmed')
    original_rows = receipts['usage-ledger.json'].get('rows', [])
    original = _attempt({'attempts': original_rows}, attempt_id)
    for name in ('id', 'segment', 'request', 'api_protocol', 'status', 'detail', 'http_status', 'reserved_usd', 'settled_usd'):
        _require(original.get(name) == attempt.get(name), 'segment receipt and original attempt identity differ')
    request_relative = f'{segment}/request-{request_number:03d}.json'
    request_raw, request = _json(_inside(stage, request_relative))
    bindings[request_relative] = _sha(request_raw)
    _require(request.get('model') == metadata.get('model') and request.get('stream') is True,
             'request model or stream identity does not match segment')
    if protocol == 'chat':
        stream_options = request.get('stream_options')
        _require(isinstance(request.get('messages'), list) and 'input' not in request
                 and isinstance(stream_options, dict) and stream_options.get('include_usage') is True,
                 'request does not have the recorded Chat streaming protocol')
    else:
        _require('input' in request and 'messages' not in request, 'request does not have the recorded Responses protocol')
    response_relative = f'{segment}/response-{request_number:03d}.sse'
    usage, response_sha, response_bytes = _scan_response(_inside(stage, response_relative), protocol)
    bindings[response_relative] = response_sha
    pricing = ledger.get('pricing', {})
    cost = (_amount(pricing.get('input_per_mtoken_usd')) * (usage['input_tokens'] - usage['cached_input_tokens'])
            + _amount(pricing.get('cached_input_per_mtoken_usd')) * usage['cached_input_tokens']
            + _amount(pricing.get('output_per_mtoken_usd')) * usage['output_tokens']) / Decimal(1000000)
    prior_unknown = _amount(attempt.get('settled_usd'))
    _require(prior_unknown == _amount(attempt.get('reserved_usd')) and prior_unknown > 0,
             'unknown amount does not match the original reservation')
    _require(cost <= prior_unknown, 'observed cost exceeds original reservation; separate review required')
    _require(_amount(ledger.get('unknown_usd')) >= prior_unknown, 'ledger unknown total is inconsistent')
    _amount(ledger.get('committed_usd'))
    _require(_sha(_json(ledger_path)[0]) == bindings['budget-ledger.json'], 'ledger changed during planning')
    plan = dict(schema=1, operation='reconcile_captured_usage', stage=str(stage), segment=segment,
                attempt_id=attempt_id, request=request_number, api_protocol=protocol,
                bindings=bindings, response_bytes=response_bytes, usage=usage,
                prior_unknown_usd=float(prior_unknown), settled_usd=float(cost), amounts_are_estimates=True)
    plan['plan_id'] = _sha(_canonical(plan))
    return plan


def _validate_plan(plan):
    unsigned = dict(plan)
    plan_id = unsigned.pop('plan_id', None)
    _require(isinstance(plan_id, str) and plan_id == _sha(_canonical(unsigned)), 'plan identity is invalid')
    _require(plan.get('schema') == 1 and plan.get('operation') == 'reconcile_captured_usage', 'unsupported plan')
    return plan_id


def _verify_backups(journal, plan):
    for relative in ('budget-ledger.json', *(f"{plan['segment']}/{name}" for name in RECEIPTS)):
        backup = journal / (Path(relative).stem + '.before.json')
        _require(_sha(_json(backup)[0]) == plan['bindings'][relative], 'original evidence backup is missing or changed')


def _receipt(plan, status, **extra):
    return dict(status=status, plan_id=plan['plan_id'], attempt_id=plan['attempt_id'],
                source_bindings=plan['bindings'], usage=plan['usage'], api_protocol=plan['api_protocol'],
                prior_unknown_usd=plan['prior_unknown_usd'], settled_usd=plan['settled_usd'],
                amounts_are_estimates=True, **extra)


def apply_plan(plan):
    """Apply exactly one reviewed plan; immutable source evidence is copied, never edited."""
    plan_id = _validate_plan(plan)
    stage = Path(plan['stage']).resolve()
    with campaign_lock(stage):
        ledger_path = _inside(stage, 'budget-ledger.json')
        ledger_raw, ledger = _json(ledger_path)
        attempt = _attempt(ledger, plan['attempt_id'])
        journal = _inside(stage, f'usage-reconciliations/{plan_id}')
        if attempt.get('reconciliation_id') == plan_id:
            _require(_json(journal / 'plan.json')[1] == plan, 'reconciliation journal does not match plan')
            _verify_backups(journal, plan)
            _require(attempt.get('status') == 'committed' and attempt.get('settled_usd') == plan['settled_usd'],
                     'previous reconciliation result changed')
            receipt_path = journal / 'receipt.json'
            _, receipt = _json(receipt_path)
            if receipt.get('status') == 'PREPARED':
                receipt.update(status='APPLIED', ledger_after_sha256=_sha(ledger_raw), recovered_after_write=True)
                _write_json(receipt_path, receipt)
            _require(receipt.get('status') == 'APPLIED', 'unrecognized reconciliation state')
            return dict(receipt, already_applied=True)

        fresh = build_plan(stage, plan['segment'], plan['attempt_id'])
        _require(fresh == plan, 'source evidence changed since the reviewed plan')
        if journal.exists():
            _require(_json(journal / 'plan.json')[1] == plan, 'existing journal belongs to another plan')
            _require(_json(journal / 'receipt.json')[1].get('status') == 'PREPARED', 'journal state is inconsistent')
        else:
            journal.mkdir(parents=True, exist_ok=False)
            _write_json(journal / 'plan.json', plan)
            for relative in ('budget-ledger.json', *(f"{plan['segment']}/{name}" for name in RECEIPTS)):
                raw, _ = _json(_inside(stage, relative))
                _require(_sha(raw) == plan['bindings'][relative], 'original evidence changed before backup')
                destination = journal / (Path(relative).stem + '.before.json')
                with destination.open('xb') as output:
                    output.write(raw)
                    output.flush()
                    os.fsync(output.fileno())
            _write_json(journal / 'receipt.json', _receipt(plan, 'PREPARED'))
        _verify_backups(journal, plan)

        updated = copy.deepcopy(ledger)
        target = _attempt(updated, plan['attempt_id'])
        target.update(status='committed', settled_usd=plan['settled_usd'], estimated_usd=plan['settled_usd'],
                      detail='usage_reconciled_from_complete_capture', reconciliation_id=plan_id,
                      **plan['usage'])
        updated['unknown_usd'] = float(_amount(ledger['unknown_usd']) - _amount(attempt['settled_usd']))
        updated['committed_usd'] = float(_amount(ledger['committed_usd']) + _amount(plan['settled_usd']))
        _write_json(ledger_path, updated)
        receipt = _receipt(plan, 'APPLIED', ledger_after_sha256=_sha(_json(ledger_path)[0]))
        _write_json(journal / 'receipt.json', receipt)
        return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='operation', required=True)
    inspect = sub.add_parser('plan')
    inspect.add_argument('stage', type=Path)
    inspect.add_argument('segment')
    inspect.add_argument('--attempt-id', type=int, required=True)
    apply = sub.add_parser('apply')
    apply.add_argument('plan', type=Path)
    args = parser.parse_args()
    result = (build_plan(args.stage, args.segment, args.attempt_id) if args.operation == 'plan'
              else apply_plan(_json(args.plan)[1]))
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
