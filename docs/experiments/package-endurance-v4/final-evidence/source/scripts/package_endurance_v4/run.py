"""Bounded real-model segments; all task/effect authority stays in Runtime/Core."""
import argparse
import json
from pathlib import Path
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
sys.path.insert(0, str(ROOT / 'scripts/package_endurance'))
from runtime_endurance_incremental_runner import BudgetLedger, RunnerConfig, build_env, run_segment
from limits import campaign_lock, counts
from run import ByteReservedPricing


def limits(stage):
    value = json.loads((Path(stage).parent / 'campaign.json').read_bytes())
    assert value['kind'] == 'v4_real_snapshot_workflow'
    assert value['deadline_epoch'] == value['created_epoch'] + 4 * 3600
    return value


class SnapshotLedger(BudgetLedger):
    def _reserve_policy(self):
        return dict(strategy='wire_bytes_plus_8192_and_full_output',
                    max_output_tokens=self.max_output_tokens, **limits(self.path.parent))

    def reserve(self, number, body_len, segment):
        with self.lock:
            caps = limits(self.path.parent)
            admitted = [a for a in self.data['attempts'] if a['status'] != 'rejected_cap']
            decisions, tools = counts(self.path.parent)
            reason = None
            if any(a['status'] in ('unknown', 'reserved') for a in admitted):
                reason = 'unsettled prior attempt'
            elif time.time() >= caps['deadline_epoch']:
                reason = 'campaign deadline'
            elif decisions > caps['main_decisions'] or tools + 32 > caps['tool_attempts']:
                reason = 'decision/tool cap'
            elif len(admitted) >= caps['provider_attempts']:
                reason = 'provider attempt cap'
            elif sum(a.get('input_tokens', 0) for a in admitted) + body_len + 8192 > caps['input_tokens']:
                reason = 'input token cap'
            elif sum(a.get('output_tokens', 0) for a in admitted) + self.max_output_tokens > caps['output_tokens']:
                reason = 'output token cap'
            if reason:
                self.data['cap_stopped'] = True
            result = super().reserve(number, body_len, segment)
            if reason:
                self.data['attempts'][-1]['detail'] = reason
                self._save()
            return result


def run(stage, segment, mode, rounds, instruction):
    stage = Path(stage).resolve()
    caps = limits(stage)
    with campaign_lock(stage):
        used, _ = counts(stage)
        if not 1 <= rounds <= caps['main_decisions'] - used:
            raise ValueError('decision allocation exceeds remaining campaign budget')
        if time.time() >= caps['deadline_epoch']:
            raise ValueError('original campaign deadline expired')
        env = build_env(ROOT)
        env['AGENT_PYTHON'] = sys.executable
        env['MAINTENANCE_MAX_CALLS_PER_MAINTAIN'] = '0'
        env['OPENAI_API_PROTOCOL'] = 'chat'
        env['OPENAI_CHAT_THINKING'] = 'disabled'
        env.pop('OPENAI_RESPONSES_REASONING_EFFORT', None)
        prompt = (f'Implement the snapshot module described in SPEC.md in this existing task/workspace. '
                  f'For process.run use argv[0]={sys.executable}. Discover/load process.run first. '
                  'Use Python stdlib only. Only app/snapshot.py and app/tests/test_snapshot.py may change. '
                  'Existing app code, SPEC.md, fixtures/ and public tests are protected. '
                  'Do not inspect Runtime private files or external controller/oracle code. '
                  'Do not execute package payloads or access network. Read SPEC.md, inspect the actual SQLite/schema '
                  'and manifest fixtures, implement the APIs and CLI, and run real public and new app tests. '
                  'Prefer bounded file edits with valid tool arguments. Report concrete results and limitations; '
                  'ordinary final is not operator acceptance. Preserve earlier requirements across corrections.\n\n'
                  + instruction)
        if mode == 'work' and len(prompt) > 2000:
            raise ValueError('initial durable goal must fit the existing 2000-character limit')
        protected = tuple(json.loads((stage / 'baseline-lock.json').read_bytes())['files'])
        config = RunnerConfig(segment='model-' + segment, campaign_dir=stage, mode=mode,
                              rounds=rounds, prompt_text=prompt, env=env, api_protocol='chat',
                              protected_files=protected, max_cost_usd=caps['estimated_cost_usd'],
                              max_output_tokens=8192, pricing=ByteReservedPricing(),
                              ledger_class=SnapshotLedger,
                              child_wait_timeout_s=min(900, max(1, caps['deadline_epoch'] - time.time() - 55)))
        return run_segment(config)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('stage', type=Path)
    parser.add_argument('segment')
    parser.add_argument('--mode', choices=['work', 'feedback', 'resume'], default='feedback')
    parser.add_argument('--rounds', type=int, default=16)
    parser.add_argument('--instruction', type=Path, required=True)
    args = parser.parse_args()
    raise SystemExit(run(args.stage, args.segment, args.mode, args.rounds,
                         args.instruction.read_text(encoding='utf-8')))
