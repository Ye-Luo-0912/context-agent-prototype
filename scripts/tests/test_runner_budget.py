"""BR6 regressions: persistent budget ledger, reserve-before-accept, strict usage."""
from __future__ import annotations

import io
import contextlib
import json
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

import _support as support

runner = support.runner


def expected_usage_cost() -> float:
    pricing = runner.Pricing()
    return pricing.attempt_cost_usd(1000, 0, 100)


def expected_reserve(max_output_tokens: int = 8192) -> float:
    return runner.Pricing().reserve_estimate_usd(len(support.REQUEST_BODY), max_output_tokens)


def chat_sse(usage):
    return b'data: ' + json.dumps({'choices': [], 'usage': usage}).encode() + b'\n\ndata: [DONE]\n\n'


class ChatUpstream:
    """Only a loopback fixture; capture the unmodified path/body and emit Chat SSE."""
    def __init__(self, usage, padding_events=0):
        self.requests = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
                owner.requests.append((self.path, body))
                padding = b'data: ' + json.dumps({'choices': [{'delta': {'content': 'x' * 1024},
                                                               'finish_reason': None}]}).encode() + b'\n\n'
                response = b'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\n' + chat_sse(usage)
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(response) + padding_events * len(padding)))
                self.end_headers()
                for _ in range(padding_events):
                    self.wfile.write(padding)
                self.wfile.write(response)

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        self.url = f'http://127.0.0.1:{self.server.server_port}'
        return self

    def __exit__(self, *_):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


CHAT_CHILD = '''import json,os,sys,urllib.request
from pathlib import Path
protocol=os.environ['OPENAI_API_PROTOCOL']
Path(sys.argv[1]).write_text(json.dumps({'api_protocol':protocol,
    'chat_thinking':os.environ.get('OPENAI_CHAT_THINKING'),
    'responses_reasoning':os.environ.get('OPENAI_RESPONSES_REASONING_EFFORT')}))
body=json.dumps({'model':'stub','stream':True,'messages':[{'role':'user','content':'fixture'}]}).encode()
endpoint='/chat/completions' if protocol=='chat' else '/responses'
request=urllib.request.Request(os.environ['OPENAI_BASE_URL']+endpoint,data=body,headers={'Content-Type':'application/json'})
with urllib.request.urlopen(request,timeout=5) as response:response.read()
'''


class RunnerChatProtocolTests(unittest.TestCase):
    def test_incremental_multiline_sse_keeps_bounded_buffers(self):
        capture = runner.SseUsageCapture('chat', max_event_bytes=256)
        payload = (b'data: {"choices":[{"finish_reason":"stop"}]}\r\n\r\n'
                   b'data: {"usage": {"prompt_tokens":10,\r\n'
                   b'data: "completion_tokens":2,"prompt_cache_hit_tokens":4}}\r\n\r\n'
                   b'data: [DONE]\r\n\r\n')
        for offset in range(0, len(payload), 3):
            capture.feed(payload[offset:offset+3])
        usage, problem = capture.result()
        self.assertIsNone(problem)
        self.assertEqual(usage['cached_input_tokens'], 4)
        self.assertTrue(capture.complete)
        self.assertLessEqual(capture.max_buffered_bytes, 512)
        oversized = runner.SseUsageCapture('chat', max_event_bytes=256)
        for _ in range(100):
            oversized.feed(b'x' * 64)
        oversized.feed(b'\n\n' + payload)
        self.assertIsNotNone(oversized.result()[0])
        self.assertEqual(oversized.oversized_events, 1)
        self.assertFalse(oversized.complete)
        self.assertLessEqual(oversized.max_buffered_bytes, 512)

    def test_responses_terminal_is_complete_without_done_but_rejects_following_data(self):
        payload = b'data: ' + json.dumps({'type': 'response.completed', 'response': {
            'usage': support.USAGE_FULL}}).encode() + b'\n\n'
        complete = runner.SseUsageCapture('responses')
        complete.feed(payload)
        self.assertIsNotNone(complete.result()[0])
        self.assertTrue(complete.complete)
        invalid = runner.SseUsageCapture('responses')
        invalid.feed(payload + b'data: {"type":"response.output_text.delta","delta":"later"}\n\n')
        invalid.result()
        self.assertFalse(invalid.complete)

    def test_upstream_origin_omits_credentials_path_and_query(self):
        self.assertEqual(runner._upstream_origin('https://user:credential@example.invalid:8443/private/token?api_key=hidden'),
                         {'scheme': 'https', 'hostname': 'example.invalid', 'port': 8443})

    def test_tail_usage_beyond_two_mib_is_settled_through_real_relay(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / 'campaign')
            with ChatUpstream(dict(prompt_tokens=1000, completion_tokens=100, prompt_cache_hit_tokens=40),
                              padding_events=2100) as upstream:
                env = support.minimal_env(upstream.url)
                env['OPENAI_API_PROTOCOL'] = 'chat'
                cfg = support.base_config(campaign, env=env,
                    child_command=[sys.executable, '-c', CHAT_CHILD, str(root / 'protocol.txt')])
                with contextlib.redirect_stdout(io.StringIO()):
                    code = runner.run_segment(cfg)
            self.assertGreater((campaign / 'seg-001/response-001.sse').stat().st_size, 2 * 1024 * 1024)
            self.assertEqual(code, runner.EXIT_OK)
            ledger = support.read_json(campaign / 'budget-ledger.json')
            self.assertEqual(ledger['attempts'][0]['status'], 'committed')
            self.assertEqual(ledger['unknown_usd'], 0)
            self.assertAlmostEqual(ledger['committed_usd'], runner.Pricing().attempt_cost_usd(1000, 40, 100))

    def test_chat_missing_or_invalid_usage_remains_unknown(self):
        valid = dict(prompt_tokens=10, completion_tokens=2, prompt_cache_hit_tokens=4)
        variants = [dict(valid, prompt_tokens=value) for value in (True, -1, 1.5, '10')]
        variants += [dict(valid, prompt_cache_hit_tokens=11), dict(valid, completion_tokens=-1)]
        variants += [{key: value for key, value in valid.items() if key != missing}
                     for missing in valid]
        for usage in variants:
            with self.subTest(usage=usage):
                parsed, problem = runner.parse_usage_strict(chat_sse(usage), api_protocol='chat')
                self.assertIsNone(parsed)
                self.assertTrue(problem.startswith(('usage_incomplete:', 'usage_invalid:')))
        self.assertIsNone(runner.parse_usage_strict(chat_sse(valid))[0], 'default remains Responses')
        self.assertIsNone(runner.parse_usage_strict(support._sse(support.USAGE_FULL), api_protocol='chat')[0])

    def test_chat_usage_accepts_deepseek_and_standard_cache_shapes(self):
        for cache, shape in (({'prompt_cache_hit_tokens': 4}, 'prompt_cache_hit_tokens'),
                             ({'prompt_tokens_details': {'cached_tokens': 4}}, 'prompt_tokens_details.cached_tokens')):
            with self.subTest(shape=shape):
                usage, problem = runner.parse_usage_strict(
                    chat_sse(dict(prompt_tokens=10, completion_tokens=2, **cache)), api_protocol='chat')
                self.assertIsNone(problem)
                self.assertEqual(usage, dict(input_tokens=10, output_tokens=2,
                                            cached_input_tokens=4, usage_shape=shape))

    def test_chat_environment_and_explicit_override_reach_child_relay_and_receipts(self):
        for explicit in (False, True):
            with self.subTest(explicit=explicit), tempfile.TemporaryDirectory() as raw:
                root = Path(raw)
                campaign = support.make_campaign(root / 'campaign')
                marker = root / 'child-protocol.txt'
                with ChatUpstream(dict(prompt_tokens=1000, completion_tokens=100, prompt_cache_hit_tokens=40)) as upstream:
                    env = support.minimal_env(upstream.url)
                    env['OPENAI_API_PROTOCOL'] = 'responses' if explicit else 'chat'
                    env['OPENAI_CHAT_THINKING'] = 'disabled'
                    env['OPENAI_RESPONSES_REASONING_EFFORT'] = 'none'
                    cfg = support.base_config(campaign, env=env,
                        child_command=[sys.executable, '-c', CHAT_CHILD, str(marker)])
                    if explicit:
                        cfg.api_protocol = 'chat'
                    with contextlib.redirect_stdout(io.StringIO()):
                        self.assertEqual(runner.run_segment(cfg), runner.EXIT_OK)
                self.assertEqual(json.loads(marker.read_text()), dict(api_protocol='chat',
                    chat_thinking='disabled', responses_reasoning=None))
                self.assertEqual(len(upstream.requests), 1)
                self.assertEqual(upstream.requests[0][0], '/chat/completions')
                self.assertEqual(upstream.requests[0][1], (campaign / 'seg-001/request-001.json').read_bytes())
                ledger = support.read_json(campaign / 'budget-ledger.json')
                self.assertEqual(ledger['api_protocol'], 'chat')
                self.assertEqual(ledger['attempts'][0]['api_protocol'], 'chat')
                self.assertEqual(ledger['attempts'][0]['status'], 'committed')
                self.assertAlmostEqual(ledger['committed_usd'], runner.Pricing().attempt_cost_usd(1000, 40, 100))
                for name in ('metadata.json', 'summary.json', 'usage-ledger.json'):
                    self.assertEqual(support.read_json(campaign / 'seg-001' / name)['api_protocol'], 'chat')
                origin = support.read_json(campaign / 'seg-001/metadata.json')['upstream_origin']
                self.assertEqual(origin, dict(scheme='http', hostname='127.0.0.1', port=upstream.server.server_port))

    def test_chat_refuses_legacy_responses_ledger_without_modifying_it(self):
        with tempfile.TemporaryDirectory() as raw:
            campaign = support.make_campaign(Path(raw) / 'campaign')
            path = campaign / 'budget-ledger.json'
            path.write_text(json.dumps(dict(schema=1, attempts=[], committed_usd=0,
                                            reserved_usd=0, unknown_usd=0, cap_stopped=False)))
            before = path.read_bytes()
            env = support.minimal_env('http://127.0.0.1:9')
            env['OPENAI_API_PROTOCOL'] = 'chat'
            cfg = support.base_config(campaign, env=env, child_command=[sys.executable, '-c', 'pass'])
            with mock.patch.object(runner, '_spawn_child', side_effect=AssertionError('child admitted')) as spawn, \
                    mock.patch.object(runner, '_Relay', side_effect=AssertionError('relay admitted')) as relay, \
                    contextlib.redirect_stdout(io.StringIO()):
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            spawn.assert_not_called()
            relay.assert_not_called()
            self.assertEqual(path.read_bytes(), before)
            self.assertFalse((campaign / 'seg-001').exists())

    def test_chat_missing_cache_settles_reservation_as_unknown(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / 'campaign')
            with ChatUpstream(dict(prompt_tokens=1000, completion_tokens=100)) as upstream:
                env = support.minimal_env(upstream.url)
                env['OPENAI_API_PROTOCOL'] = 'chat'
                cfg = support.base_config(campaign, env=env,
                    child_command=[sys.executable, '-c', CHAT_CHILD, str(root / 'protocol.txt')])
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(runner.run_segment(cfg), runner.EXIT_BUDGET_INCOMPLETE)
            ledger = support.read_json(campaign / 'budget-ledger.json')
            attempt = ledger['attempts'][0]
            self.assertEqual(attempt['status'], 'unknown')
            self.assertEqual(attempt['detail'], 'usage_incomplete:cached_input_tokens')
            self.assertGreater(ledger['unknown_usd'], 0)
            self.assertEqual(ledger['committed_usd'], 0)
            self.assertEqual(ledger['reserved_usd'], 0)

    def test_ledger_protocol_is_immutable_across_reloads(self):
        with tempfile.TemporaryDirectory() as raw:
            for protocol in ('chat', 'responses'):
                with self.subTest(protocol=protocol):
                    path = Path(raw) / f'{protocol}.json'
                    ledger, inherited = runner.BudgetLedger.load(path, 1.5, runner.Pricing(), 8192,
                                                                 api_protocol=protocol)
                    self.assertFalse(inherited)
                    attempt, _, _ = ledger.reserve(1, 100, 'segment')
                    ledger.settle(attempt, 'committed', support.USAGE_FULL)
                    ledger, inherited = runner.BudgetLedger.load(path, 1.5, runner.Pricing(), 8192,
                                                                 api_protocol=protocol)
                    self.assertTrue(inherited)
                    self.assertEqual(ledger.data['attempts'][0]['api_protocol'], protocol)
                    before = path.read_bytes()
                    other = 'responses' if protocol == 'chat' else 'chat'
                    with self.assertRaisesRegex(runner._BudgetLedgerError, 'protocol mismatch'):
                        runner.BudgetLedger.load(path, 1.5, runner.Pricing(), 8192, api_protocol=other)
                    self.assertEqual(path.read_bytes(), before)

    def test_unknown_environment_protocol_refuses_before_ledger_or_relay(self):
        with tempfile.TemporaryDirectory() as raw:
            campaign = support.make_campaign(Path(raw) / 'campaign')
            env = support.minimal_env('http://127.0.0.1:9')
            env['OPENAI_API_PROTOCOL'] = 'unsupported'
            cfg = support.base_config(campaign, env=env)
            with mock.patch.object(runner, '_Relay', side_effect=AssertionError('relay admitted')) as relay, \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(runner.run_segment(cfg), runner.EXIT_USAGE)
            relay.assert_not_called()
            self.assertFalse((campaign / 'budget-ledger.json').exists())


class RunnerBudgetTests(unittest.TestCase):
    def test_full_usage_settles_committed(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("ok") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, 0)
            ledger = support.read_json(campaign / "budget-ledger.json")
            self.assertAlmostEqual(ledger["committed_usd"], expected_usage_cost(), places=12)
            self.assertAlmostEqual(ledger["reserved_usd"], 0.0, places=12)
            self.assertAlmostEqual(ledger["unknown_usd"], 0.0, places=12)
            self.assertFalse(ledger["cap_stopped"])
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["id"], 1)
            self.assertEqual(attempt["request"], 1)
            self.assertEqual(attempt["status"], "committed")
            out = campaign / "seg-001"
            usage_ledger = support.read_json(out / "usage-ledger.json")
            self.assertTrue(usage_ledger["amounts_are_estimates"])
            self.assertEqual(usage_ledger["rows"][0]["status"], "committed")
            self.assertIn("reserve_policy", usage_ledger)
            metadata = support.read_json(out / "metadata.json")
            self.assertFalse(metadata["budget"]["inherited_ledger"])
            self.assertIn("reserve_policy", metadata["budget"])

    def test_missing_usage_field_settles_unknown_without_zero_fill(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("missing_cached") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            ledger = support.read_json(campaign / "budget-ledger.json")
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["status"], "unknown")
            # the reserved estimate stays occupied as unknown, never guessed to zero
            self.assertAlmostEqual(ledger["unknown_usd"], expected_reserve(), places=12)
            self.assertNotIn("input_tokens", attempt)
            self.assertNotIn("output_tokens", attempt)
            self.assertIn("cached_input_tokens", attempt["detail"])
            self.assertEqual(len(upstream.requests), 1)

    def test_nested_details_usage_shape_settles_committed(self):
        # DeepSeek's Responses-compatible serving reports the cache-hit
        # bucket as input_tokens_details.cached_tokens (observed live on the
        # 2026-09-19 paid run); it must settle committed, never unknown.
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("nested_cached") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, 0)
            ledger = support.read_json(campaign / "budget-ledger.json")
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["status"], "committed")
            self.assertEqual(attempt["usage_shape"], "input_tokens_details.cached_tokens")
            self.assertEqual(attempt["input_tokens"], 1000)
            self.assertEqual(attempt["cached_input_tokens"], 40)
            expected = runner.Pricing().attempt_cost_usd(1000, 40, 100)
            self.assertAlmostEqual(ledger["committed_usd"], expected, places=12)
            self.assertAlmostEqual(ledger["unknown_usd"], 0.0, places=12)
            self.assertEqual(len(upstream.requests), 1)

    def test_parse_usage_accepts_nested_and_flat_cache_shapes(self):
        nested, reason = runner.parse_usage_strict(
            support._sse({"input_tokens": 10, "input_tokens_details": {"cached_tokens": 4}, "output_tokens": 2})
        )
        self.assertIsNone(reason)
        self.assertEqual(nested["cached_input_tokens"], 4)
        self.assertEqual(nested["usage_shape"], "input_tokens_details.cached_tokens")
        flat, reason = runner.parse_usage_strict(support._sse(dict(support.USAGE_FULL)))
        self.assertIsNone(reason)
        self.assertEqual(flat["cached_input_tokens"], 0)
        self.assertEqual(flat["usage_shape"], "cached_input_tokens")
        missing, reason = runner.parse_usage_strict(support._sse({"input_tokens": 10, "output_tokens": 2}))
        self.assertIsNone(missing)
        self.assertIn("cached_input_tokens", reason)

    def test_reserve_rejects_request_over_cap_before_forwarding(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("ok") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    max_cost_usd=0.005,  # below a single reserve estimate (~0.0099)
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            self.assertEqual(upstream.requests, [], "over-cap request must be refused before upstream contact")
            ledger = support.read_json(campaign / "budget-ledger.json")
            self.assertTrue(ledger["cap_stopped"])
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["status"], "rejected_cap")
            self.assertEqual(attempt["settled_usd"], 0.0)

    def test_rejected_attempts_are_not_billed(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("ok") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    max_cost_usd=0.005,
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "2", "0.3"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            ledger = support.read_json(campaign / "budget-ledger.json")
            statuses = [attempt["status"] for attempt in ledger["attempts"]]
            self.assertEqual(statuses, ["rejected_cap", "rejected_cap"])
            self.assertAlmostEqual(ledger["committed_usd"], 0.0, places=12)
            self.assertAlmostEqual(ledger["reserved_usd"], 0.0, places=12)
            self.assertAlmostEqual(ledger["unknown_usd"], 0.0, places=12)
            self.assertEqual(upstream.requests, [], "429 rejections must not be forwarded nor double-billed")

    def test_concurrent_reserve_mutual_exclusion(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            # first request stalls ~2.5s upstream, second fires after 1.2s while
            # the first reservation is still open; the cap only fits one.
            cap = expected_reserve() * 1.5
            with support.StubUpstream("delay", delay_s=2.5) as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    max_cost_usd=cap,
                    upstream_timeout_s=10.0,
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "2", "1.2"],
                )
                code = runner.run_segment(cfg)
            ledger = support.read_json(campaign / "budget-ledger.json")
            statuses = sorted(attempt["status"] for attempt in ledger["attempts"])
            self.assertEqual(statuses, ["committed", "rejected_cap"])
            self.assertAlmostEqual(ledger["committed_usd"], expected_usage_cost(), places=12)
            self.assertAlmostEqual(ledger["reserved_usd"], 0.0, places=12)
            self.assertAlmostEqual(ledger["unknown_usd"], 0.0, places=12)

    def test_ledger_inherited_across_processes(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            # est(max_output_tokens=100) ~ 2.0e-4; cap leaves 1.8e-4 after run 1,
            # so the second process's reservation must be refused.
            cap = 0.0006
            with support.StubUpstream("ok") as upstream:
                first = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    segment="seg-001",
                    max_cost_usd=cap,
                    max_output_tokens=100,
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code_first = runner.run_segment(first)
                second = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    segment="seg-002",
                    max_cost_usd=cap,
                    max_output_tokens=100,
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code_second = runner.run_segment(second)
            self.assertEqual(code_first, 0)
            self.assertEqual(code_second, runner.EXIT_BUDGET_INCOMPLETE)
            ledger = support.read_json(campaign / "budget-ledger.json")
            self.assertAlmostEqual(ledger["committed_usd"], expected_usage_cost(), places=12)
            self.assertEqual([attempt["id"] for attempt in ledger["attempts"]], [1, 2], "attempt ids stay monotonic across processes")
            self.assertEqual(ledger["attempts"][0]["segment"], "seg-001")
            self.assertEqual(ledger["attempts"][1]["segment"], "seg-002")
            self.assertEqual(ledger["attempts"][1]["status"], "rejected_cap")
            self.assertEqual(len(upstream.requests), 1, "second process's refused request must not reach upstream")
            metadata_second = support.read_json(campaign / "seg-002" / "metadata.json")
            self.assertTrue(metadata_second["budget"]["inherited_ledger"])
            self.assertTrue(metadata_second["budget"]["cap_stopped"])

    def test_boundary_last_request_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            pricing = runner.Pricing()
            # remaining budget 0.01: an estimate of ~0.0097 fits, 0.0201 does not
            fitting, _ = runner.BudgetLedger.load(root / "ledger-a.json", 0.03, pricing, 8000)
            fitting.data["committed_usd"] = 0.02
            attempt_id, estimate, rejected = fitting.reserve(1, 40, "seg-b")
            self.assertFalse(rejected)
            self.assertAlmostEqual(estimate, 0.0096798, places=9)
            self.assertIsNotNone(attempt_id)
            oversized, _ = runner.BudgetLedger.load(root / "ledger-b.json", 0.03, pricing, 16666)
            oversized.data["committed_usd"] = 0.02
            attempt_id, estimate, rejected = oversized.reserve(1, 40, "seg-b")
            self.assertTrue(rejected, "estimate 0.0201 must be refused with 0.01 remaining")
            self.assertAlmostEqual(estimate, 0.020079, places=9)
            self.assertTrue(oversized.data["cap_stopped"])

    def test_upstream_stall_settles_unknown_without_crash(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("stall", stall_s=30.0) as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    upstream_timeout_s=0.8,
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            ledger = support.read_json(campaign / "budget-ledger.json")
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["status"], "unknown")
            self.assertTrue(attempt["detail"].startswith("upstream_failure:"))
            self.assertAlmostEqual(ledger["unknown_usd"], expected_reserve(), places=12)
            summary = support.read_json(campaign / "seg-001" / "summary.json")
            self.assertEqual(summary["outcome"], "completed", "runner must not crash on a hung upstream")

    def test_upstream_429_not_billed(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("rate_limited") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, 0)
            ledger = support.read_json(campaign / "budget-ledger.json")
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["status"], "upstream_rate_limited")
            self.assertEqual(attempt["settled_usd"], 0.0)
            self.assertAlmostEqual(ledger["committed_usd"], 0.0, places=12)
            self.assertAlmostEqual(ledger["unknown_usd"], 0.0, places=12)
            self.assertFalse(ledger["cap_stopped"])

    def test_corrupt_ledger_refuses_to_run(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            (campaign / "budget-ledger.json").write_text("{broken", encoding="utf-8")
            cfg = support.base_config(campaign, child_command=[sys.executable, support.stub_file(root, "exit0.py", support.CHILD_EXIT_0)])
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            self.assertFalse((campaign / "seg-001").exists(), "must not start a segment on an unreadable ledger")
            status = json.loads(stdout.getvalue().strip().splitlines()[-1])
            self.assertEqual(status["status"], "budget_ledger_unreadable")

    def test_cli_rejects_invalid_budget_numbers(self):
        cases = [
            ["seg-001", "--max-cost-usd=nan"],
            ["seg-001", "--max-cost-usd=inf"],
            ["seg-001", "--max-cost-usd=-0.01"],
            ["seg-001", "--rounds=-3"],
            ["seg-001", "--max-output-tokens=-5"],
        ]
        for argv in cases:
            with self.subTest(argv=argv):
                self.assertEqual(runner.main(argv), 2)


if __name__ == "__main__":
    unittest.main()
