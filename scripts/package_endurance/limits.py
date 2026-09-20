"""Campaign admission limits; no new authority over Runtime tasks or effects."""
import contextlib
import json
import os
import time
from pathlib import Path

from runtime_endurance_incremental_runner import BudgetLedger


def deadline(campaign):
    # Never reset the clock when resuming a segment or taking over a task.
    return (Path(campaign) / 'baseline-lock.json').stat().st_mtime + 4 * 3600


def counts(campaign):
    decisions = tools = 0
    for path in Path(campaign).glob('model-*/events.jsonl'):
        for line in path.read_text(encoding='utf-8').splitlines():
            if not line.strip():
                continue
            event = json.loads(line).get('event', {})
            decisions += event.get('type') == 'model_started'
            tools += event.get('type') == 'tool_started'
    return decisions, tools


@contextlib.contextmanager
def campaign_lock(campaign):
    path = Path(campaign) / 'controller.lock'
    with path.open('a+b') as handle:
        if path.stat().st_size == 0:
            handle.write(b'0'); handle.flush()
        handle.seek(0)
        if os.name == 'nt':
            import msvcrt
            msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            yield
        finally:
            handle.seek(0)
            if os.name == 'nt':
                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


class CampaignLedger(BudgetLedger):
    def _reserve_policy(self):
        return dict(strategy='wire_byte_upper_estimate',
                    input_estimate='request_body_bytes + 8192 tokens at cache-miss rate',
                    output_estimate='max_output_tokens at output rate',
                    max_output_tokens=self.max_output_tokens, estimated=True,
                    attempts_cap=300, input_tokens_cap=9000000,
                    output_tokens_cap=300000, decisions_cap=240,
                    tool_attempts_cap=720, tool_slots_reserved_per_request=32,
                    deadline_epoch=deadline(self.path.parent))

    def reserve(self, request_number, body_len, segment):
        with self.lock:
            attempts = self.data['attempts']
            admitted = [a for a in attempts if a['status'] != 'rejected_cap']
            decisions, tools = counts(self.path.parent)
            reason = None
            if any(a['status'] in ('unknown', 'reserved') for a in admitted):
                reason = 'unsettled prior attempt; no new paid admission'
            elif time.time() >= deadline(self.path.parent):
                reason = 'four-hour deadline'
            elif len(admitted) >= 300:
                reason = 'provider attempt cap'
            elif decisions > 240 or tools + 32 > 720:
                reason = 'decision/tool cap (32 tool slots reserved before response)'
            elif sum(a.get('input_tokens', 0) for a in admitted) + body_len + 8192 > 9000000:
                reason = 'input token cap'
            elif sum(a.get('output_tokens', 0) for a in admitted) + self.max_output_tokens > 300000:
                reason = 'output token cap'
            if reason:
                self.data['cap_stopped'] = True
            result = super().reserve(request_number, body_len, segment)
            if reason:
                self.data['attempts'][-1]['detail'] = reason
                self._save()
            return result
