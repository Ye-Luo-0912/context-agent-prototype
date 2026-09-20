"""One bounded paid audit-CLI slice on an explicitly assisted baseline."""
import argparse
import json
import sys
import time
from pathlib import Path

REPO=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(REPO/'scripts'))
sys.path.insert(0,str(REPO/'scripts/package_endurance'))
from runtime_endurance_incremental_runner import BudgetLedger, RunnerConfig, build_env, run_segment
from limits import counts, campaign_lock
from run import ByteReservedPricing
from common import campaign_limits


class V3Ledger(BudgetLedger):
    def _reserve_policy(self):
        return dict(strategy='wire_bytes_plus_8192_and_full_output',max_output_tokens=self.max_output_tokens,
                    **campaign_limits(self.path.parent))

    def reserve(self,number,body_len,segment):
        with self.lock:
            limits=campaign_limits(self.path.parent)
            admitted=[a for a in self.data['attempts'] if a['status']!='rejected_cap']
            decisions,tool_calls=counts(self.path.parent)
            reason=None
            if any(a['status'] in ('reserved','unknown') for a in admitted):reason='unsettled attempt'
            elif time.time()>=limits['deadline_epoch']:reason='campaign deadline'
            elif decisions>limits['main_decisions'] or tool_calls+32>limits['tool_attempts']:reason='decision/tool cap'
            elif len(admitted)>=limits['provider_attempts']:reason='attempt cap'
            elif sum(a.get('input_tokens',0) for a in admitted)+body_len+8192>2000000:reason='input token cap'
            elif sum(a.get('output_tokens',0) for a in admitted)+self.max_output_tokens>200000:reason='output token cap'
            if reason:self.data['cap_stopped']=True
            result=super().reserve(number,body_len,segment)
            if reason:self.data['attempts'][-1]['detail']=reason;self._save()
            return result


def main():
    p=argparse.ArgumentParser();p.add_argument('stage',type=Path);p.add_argument('segment')
    p.add_argument('--rounds',type=int,default=24);p.add_argument('--feedback',type=Path)
    a=p.parse_args();stage=a.stage.resolve();limits=campaign_limits(stage)
    with campaign_lock(stage):
        used,_=counts(stage)
        if not 1<=a.rounds<=limits['main_decisions']-used:raise SystemExit('decision allocation refused')
        if time.time()>=limits['deadline_epoch']:raise SystemExit('deadline expired')
        env=build_env(REPO);env['MAINTENANCE_MAX_CALLS_PER_MAINTAIN']='0'
        py=env.get('AGENT_PYTHON') or sys.executable
        prompt=(f'Add a read-only repository auditor as app/audit.py. Use Python stdlib only. '
                f'Discover/load process.run and use argv[0]={py}. '
                'Only create app/audit.py and app/tests/test_audit.py; existing application, fixtures, SPEC and public tests are protected. '
                'The existing assisted app handles deployment; do not redesign it. CLI: python -m app.audit --root PATH. '
                'It prints one JSON object {ok:bool,errors:list[str],receipts:int}, exits0 on sound data and exits1 on corruption. '
                'Open repo.sqlite read-only, enumerate all committed receipts, verify each corresponding canonical manifest SHA256, '
                'each referenced blob SHA256, and each deployments/tenant/environment/current.json against its matching durable receipt. '
                'Detect missing/corrupt blobs, manifests, missing DB receipts and pointer mismatch. Never repair or write the repository. '
                'Reject unsafe manifest paths/digest names and malformed shapes. Do not execute payloads or access network. '
                'Add tests with real temporary SQLite/files. Use small writes with valid JSON tool arguments, then run actual tests. '
                'Report exact test results; ordinary final is not operator acceptance.')
        if a.feedback:prompt+='\n'+a.feedback.read_text(encoding='utf-8')
        protected=tuple(json.loads((stage/'baseline-lock.json').read_bytes())['files'])
        cfg=RunnerConfig(segment='model-'+a.segment,campaign_dir=stage,mode='feedback' if used else 'work',
                         api_protocol='responses',
                         rounds=a.rounds,prompt_text=prompt,env=env,protected_files=protected,
                         max_cost_usd=limits['estimated_cost_usd'],max_output_tokens=8192,
                         pricing=ByteReservedPricing(),ledger_class=V3Ledger,
                         child_wait_timeout_s=min(780,max(1,limits['deadline_epoch']-time.time()-55)))
        return run_segment(cfg)


if __name__=='__main__':raise SystemExit(main())
