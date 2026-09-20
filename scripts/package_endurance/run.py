"""One bounded model segment; reuses the tested campaign ledger and cleanup."""
import argparse
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from runtime_endurance_incremental_runner import RunnerConfig, Pricing, build_env, run_segment
from limits import CampaignLedger, campaign_lock, counts, deadline

REPO = Path(__file__).resolve().parents[2]


class ByteReservedPricing(Pricing):
    def reserve_estimate_usd(self, request_body_bytes, max_output_tokens):
        # No bytes/4 assumption: use every UTF-8 wire byte as one input token,
        # plus a template allowance. This is a conservative local reservation,
        # not a prediction of the provider tokenizer or final account bill.
        return ((request_body_bytes + 8192) * self.input_per_mtoken_usd +
                max_output_tokens * self.output_per_mtoken_usd) / 1_000_000


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('campaign', type=Path)
    ap.add_argument('segment')
    ap.add_argument('--mode', choices=['work', 'feedback', 'resume'], default='feedback')
    ap.add_argument('--rounds', type=int, default=24)
    ap.add_argument('--instruction', required=True, type=Path)
    args = ap.parse_args()
    campaign = args.campaign.resolve()
    index = json.loads((campaign / 'fixture-index.json').read_text(encoding='utf-8'))
    used_decisions, used_tools = counts(campaign)
    if used_decisions + args.rounds > 240 or used_tools >= 720:
        raise SystemExit('shared decision/tool budget exhausted')
    env = build_env(REPO)
    env['MAINTENANCE_MAX_CALLS_PER_MAINTAIN'] = '0'
    py = env.get('AGENT_PYTHON') or sys.executable
    instruction = args.instruction.read_text(encoding='utf-8')
    prompt = (f'Use the existing task in this workspace and follow SPEC.md. '
              f'For process.run, argv[0] MUST be {py}; include the executable, '
              'not just -m or -c. Discover/load process.run before calling it. '
              'Run public checks with that executable and -m unittest discover -s tests -v. '
              'Modify only app/. Do not read external controller files or Runtime private state. '
              'No provider/network calls except the supplied local receiver. '
              'Create meaningful app/tests and deliver actual results, not plans alone.\n\n' + instruction)
    cfg = RunnerConfig(segment='model-' + args.segment, campaign_dir=campaign,
                       api_protocol='responses',
                       mode=args.mode, prompt_text=prompt, rounds=args.rounds,
                       max_cost_usd=3.0, max_output_tokens=8192,
                       protected_files=tuple(index['immutable']), env=env,
                       pricing=ByteReservedPricing(), ledger_class=CampaignLedger,
                       child_wait_timeout_s=min(780, max(1, deadline(campaign)-time.time()-55)))
    with campaign_lock(campaign):
        if time.time() >= deadline(campaign):
            raise SystemExit('original four-hour campaign deadline exhausted')
        return run_segment(cfg)

if __name__ == '__main__': raise SystemExit(main())
