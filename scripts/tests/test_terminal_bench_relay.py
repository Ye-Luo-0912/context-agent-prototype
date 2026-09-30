import asyncio
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from terminal_bench_pilot.credential_relay import (
    CredentialRelay, _repair_missing_tool_ids, _tool_alias,
)
from terminal_bench_pilot.harbor_agent import ContextAgentTerminalBench, AdapterConfigurationError


class RelayTests(unittest.TestCase):
    def test_single_file_edit_patch_schema_is_strictly_narrowed_on_responses_wire(self):
        received = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                received.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(
                    b'data: {"type":"response.completed","response":{"usage":'
                    b'{"input_tokens":3,"output_tokens":1}}}\n\n'
                )

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}/v1",
            credential="", model="gpt-6-luna", api_protocol="responses",
            api_safe_tool_names=True, allow_unauthenticated_loopback=True,
            single_file_edit_patch=True,
        )
        request_body = {
            "model": "gpt-6-luna", "stream": True,
            "max_output_tokens": 16, "input": "synthetic",
            "tools": [{
                "type": "function", "name": "edit.patch",
                "description": "Patch files.",
                "parameters": {
                    "type": "object", "required": ["files"],
                    "additionalProperties": False,
                    "properties": {"files": {
                        "type": "array", "maxItems": 16,
                        "items": {"type": "object"},
                    }},
                },
            }, {
                "type": "function", "name": "fs.read",
                "description": "Read.",
                "parameters": {"type": "object", "properties": {}},
            }],
        }
        request = urllib.request.Request(
            relay.base_url + "/responses",
            data=json.dumps(request_body, separators=(",", ":")).encode(),
            headers={"Authorization": "Bearer " + relay.token,
                     "Content-Type": "application/json",
                     "Accept": "text/event-stream"},
        )
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                self.assertIn(b"response.completed", response.read())
            patch_schema = received[0]["tools"][0]
            self.assertEqual(patch_schema["name"], _tool_alias("edit.patch"))
            self.assertTrue(patch_schema["strict"])
            self.assertEqual(
                patch_schema["parameters"]["properties"]["files"]["maxItems"], 1,
            )
            self.assertNotIn("strict", received[0]["tools"][1])
            self.assertEqual(relay.attempts[0]["state"], "completed")
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_responses_relay_forces_standard_cyber_program(self):
        received = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                received.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(
                    b'data: {"type":"response.completed","response":{"access_programs":'
                    b'{"cyber":"standard"},"usage":{"input_tokens":3,"output_tokens":1}}}\n\n'
                )

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}/v1",
            credential="", model="gpt-6-luna", api_protocol="responses",
            max_requests=1, max_output_tokens=16,
            allow_unauthenticated_loopback=True,
            standard_cyber_safeguards=True,
        )
        body = json.dumps({
            "model": "gpt-6-luna", "stream": True,
            "max_output_tokens": 16, "input": "synthetic",
            "access_programs": {"cyber": "daybreak_blue", "other": "preserved"},
        }).encode()
        request = urllib.request.Request(
            relay.base_url + "/responses", data=body,
            headers={"Authorization": "Bearer " + relay.token,
                     "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                self.assertIn(b"response.completed", response.read())
            self.assertEqual(len(received), 1)
            self.assertEqual(received[0]["access_programs"], {
                "cyber": "standard", "other": "preserved",
            })
            self.assertEqual(relay.attempts[0]["state"], "completed")
            self.assertTrue(relay.attempts[0]["response_completed"])
            self.assertEqual(relay.attempts[0]["observed_cyber_program"], "standard")
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_response_failed_terminal_is_recorded_without_error_details(self):
        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                event = {
                    "type": "response.failed",
                    "response": {
                        "status": "failed",
                        "error": {"message": "synthetic-detail-secret"},
                    },
                }
                self.wfile.write(b"data: " + json.dumps(event).encode() + b"\n\n")

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}/v1",
            credential="", model="gpt-6-luna", api_protocol="responses",
            max_requests=1, max_output_tokens=16,
            allow_unauthenticated_loopback=True,
        )
        body = json.dumps({
            "model": "gpt-6-luna", "stream": True,
            "max_output_tokens": 16, "input": "synthetic",
        }).encode()
        request = urllib.request.Request(
            relay.base_url + "/responses", data=body,
            headers={"Authorization": "Bearer " + relay.token,
                     "Content-Type": "application/json",
                     "Accept": "text/event-stream"},
        )
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                self.assertIn(b"response.failed", response.read())
            attempt = relay.attempts[0]
            self.assertEqual(attempt["response_terminal_event"], "response.failed")
            self.assertFalse(attempt["response_completed"])
            self.assertEqual(attempt["state"], "usage_unknown")
            self.assertTrue(relay.budget_unknown)
            with tempfile.TemporaryDirectory() as temp_dir:
                receipt_path = Path(temp_dir) / "relay.json"
                relay.save_receipt(receipt_path)
                receipt_text = receipt_path.read_text(encoding="utf-8")
                saved_attempt = json.loads(receipt_text)["attempts"][0]
            self.assertEqual(saved_attempt["response_terminal_event"], "response.failed")
            self.assertFalse(saved_attempt["response_completed"])
            self.assertNotIn("synthetic-detail-secret", receipt_text)
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_receipt_marks_provider_call_still_active_at_close_as_usage_unknown(self):
        relay = CredentialRelay(
            upstream="https://provider.invalid/v1",
            credential="synthetic-account-secret",
            model="test-model",
            max_requests=1,
            max_output_tokens=8,
        )
        relay.attempts.append({
            "attempt": 1,
            "state": "unknown",
            "usage": None,
            "input_tokens_reservation": 100,
            "output_cap": 8,
        })
        relay.input_tokens_reserved = 100
        relay.output_tokens_reserved = 8
        with relay.lock:
            relay.active_calls = 1
        with tempfile.TemporaryDirectory() as directory:
            receipt_path = Path(directory) / "relay.json"
            try:
                relay.save_receipt(receipt_path)
                receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
                self.assertTrue(receipt["budget_unknown"])
                self.assertTrue(receipt["non_retryable_unknown"])
                self.assertEqual(receipt["active_calls_after_close"], 1)
                self.assertEqual(receipt["attempts"][0]["state"], "usage_unknown")
                self.assertEqual(
                    receipt["attempts"][0]["local_error_code"], "active_call_at_close",
                )
                self.assertEqual(receipt["input_tokens_reserved"], 100)
                self.assertEqual(receipt["output_tokens_reserved"], 8)
            finally:
                with relay.lock:
                    relay.active_calls = 0
                relay.close()

    def test_anonymous_upstream_is_loopback_only_and_omits_authorization(self):
        received_auth = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                received_auth.append(self.headers.get("Authorization"))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(
                    b'data: {"type":"response.completed","response":{"usage":'
                    b'{"input_tokens":3,"output_tokens":1}}}\n\n'
                    b'data: [DONE]\n\n'
                )

        with self.assertRaisesRegex(ValueError, "anonymous upstream"):
            CredentialRelay(
                upstream="https://example.com/v1", credential="",
                model="gpt-6-luna", allow_unauthenticated_loopback=True,
            )
        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}/v1",
            credential="", model="gpt-6-luna", api_protocol="responses",
            max_requests=1, max_output_tokens=16,
            allow_unauthenticated_loopback=True,
        )
        try:
            request = urllib.request.Request(
                relay.base_url + "/responses",
                data=json.dumps({"model": "gpt-6-luna", "stream": True,
                                 "max_output_tokens": 16,
                                 "input": "synthetic"}).encode(),
                headers={"Authorization": "Bearer " + relay.token},
            )
            self.assertIn(b"[DONE]", urllib.request.urlopen(request).read())
            self.assertEqual(received_auth, [None])
            self.assertEqual(relay.attempts[0]["state"], "completed")
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_upstream_408_allows_one_exact_bounded_retry_and_retains_unknown_reservation(self):
        provider_calls = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                request_body = self.rfile.read(int(self.headers["Content-Length"]))
                provider_calls.append(request_body)
                if len(provider_calls) == 1:
                    self.send_response(408)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                payload = (
                    b'data: {"type":"response.completed","response":{"usage":'
                    b'{"input_tokens":7,"output_tokens":2}}}\n\n'
                )
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}/v1",
            credential="", model="gpt-6-luna", api_protocol="responses",
            max_requests=3, max_output_tokens=16,
            max_input_tokens_total=50_000, max_output_tokens_total=64,
            allow_unauthenticated_loopback=True,
            max_retryable_408_retries=1,
        )
        request_body = json.dumps({
            "model": "gpt-6-luna", "stream": True,
            "max_output_tokens": 16, "input": "same bounded body",
        }, separators=(",", ":")).encode()

        def request(body):
            return urllib.request.Request(
                relay.base_url + "/responses", data=body,
                headers={"Authorization": "Bearer " + relay.token,
                         "Content-Type": "application/json",
                         "Accept": "text/event-stream"},
            )

        try:
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(request(request_body))
            self.assertEqual(caught.exception.code, 503)
            self.assertEqual(
                caught.exception.headers.get("X-Relay-Stop-Reason"),
                "retryable_upstream_408",
            )
            self.assertEqual(relay.attempts[0]["state"], "unknown")
            self.assertEqual(relay.attempts[0]["upstream_status_code"], 408)
            self.assertTrue(relay.attempts[0]["retryable_upstream_408"])
            unknown_input_reservation = relay.input_tokens_reserved
            unknown_output_reservation = relay.output_tokens_reserved

            changed_body = request_body.replace(b"same bounded body", b"different body")
            with self.assertRaises(urllib.error.HTTPError) as mismatched:
                urllib.request.urlopen(request(changed_body), timeout=5)
            self.assertEqual(mismatched.exception.code, 423)
            self.assertEqual(
                mismatched.exception.headers.get("X-Relay-Stop-Reason"),
                "usage_unknown",
            )
            self.assertEqual(len(provider_calls), 1)

            with urllib.request.urlopen(request(request_body), timeout=5) as response:
                self.assertEqual(response.status, 200)
                self.assertIn(b"response.completed", response.read())
            self.assertEqual(relay.attempts[1]["state"], "completed")
            self.assertTrue(relay.attempts[1]["retry_of_unknown_408"])
            self.assertTrue(relay.budget_unknown)
            self.assertTrue(relay.retryable_408_recovered)
            self.assertFalse(relay.non_retryable_unknown)
            self.assertEqual(relay.input_tokens_reserved, unknown_input_reservation)
            self.assertEqual(relay.output_tokens_reserved, unknown_output_reservation)

            # One recovered 408 leaves its conservative reservation in place,
            # while an aggregate token/price budget can still admit new work.
            next_body = json.dumps({
                "model": "gpt-6-luna", "stream": True,
                "max_output_tokens": 16, "input": "next request",
            }, separators=(",", ":")).encode()
            with urllib.request.urlopen(request(next_body), timeout=5) as response:
                self.assertEqual(response.status, 200)
                self.assertIn(b"response.completed", response.read())
            self.assertEqual(len(provider_calls), 3)
            self.assertEqual(provider_calls[0], provider_calls[1])
            self.assertEqual(relay.attempts[2]["state"], "completed")
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_second_upstream_408_freezes_after_the_single_retry_credit(self):
        provider_calls = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                provider_calls.append(self.rfile.read(int(self.headers["Content-Length"])))
                self.send_response(408)
                self.send_header("Content-Length", "0")
                self.end_headers()

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}/v1",
            credential="", model="gpt-6-luna", api_protocol="responses",
            max_requests=4, max_output_tokens=16,
            allow_unauthenticated_loopback=True,
            max_retryable_408_retries=1,
        )
        body = json.dumps({
            "model": "gpt-6-luna", "stream": True,
            "max_output_tokens": 16, "input": "bounded retry probe",
        }, separators=(",", ":")).encode()
        request = lambda: urllib.request.Request(
            relay.base_url + "/responses", data=body,
            headers={"Authorization": "Bearer " + relay.token,
                     "Content-Type": "application/json"},
        )
        try:
            with self.assertRaises(urllib.error.HTTPError) as first:
                urllib.request.urlopen(request())
            self.assertEqual(first.exception.code, 503)
            with self.assertRaises(urllib.error.HTTPError) as second:
                urllib.request.urlopen(request())
            self.assertEqual(second.exception.code, 423)
            self.assertEqual(
                second.exception.headers.get("X-Relay-Stop-Reason"), "usage_unknown",
            )
            self.assertEqual(provider_calls, [body, body])
            self.assertEqual(len(relay.attempts), 2)
            self.assertTrue(relay.non_retryable_unknown)
            self.assertTrue(relay.budget_unknown)
            with relay.idle:
                relay.idle.wait_for(lambda: relay.active_calls == 0, timeout=1)
            self.assertEqual(relay.active_calls, 0)
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_missing_usage_after_recovered_408_freezes_new_requests(self):
        upstream_bodies = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                upstream_bodies.append(self.rfile.read(int(self.headers["Content-Length"])))
                if len(upstream_bodies) == 1:
                    self.send_response(408)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                response = {"type": "response.completed", "response": {}}
                if len(upstream_bodies) == 2:
                    response["response"]["usage"] = {
                        "input_tokens": 7, "output_tokens": 2,
                    }
                payload = b"data: " + json.dumps(response).encode() + b"\n\n"
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}/v1",
            credential="", model="gpt-6-luna", api_protocol="responses",
            max_requests=4, max_output_tokens=16,
            max_input_tokens_total=50_000, max_output_tokens_total=100,
            allow_unauthenticated_loopback=True,
            max_retryable_408_retries=1,
        )

        def request(message):
            body = json.dumps({
                "model": "gpt-6-luna", "stream": True,
                "max_output_tokens": 16, "input": message,
            }).encode()
            return urllib.request.Request(
                relay.base_url + "/responses", data=body,
                headers={"Authorization": "Bearer " + relay.token},
            )

        try:
            with self.assertRaises(urllib.error.HTTPError) as first:
                urllib.request.urlopen(request("first"), timeout=5)
            self.assertEqual(first.exception.code, 503)
            with urllib.request.urlopen(request("first"), timeout=5) as response:
                self.assertEqual(response.status, 200)
                response.read()
            self.assertTrue(relay.retryable_408_recovered)
            with urllib.request.urlopen(request("missing usage"), timeout=5) as response:
                self.assertEqual(response.status, 200)
                response.read()
            self.assertEqual(relay.attempts[2]["state"], "usage_unknown")
            self.assertTrue(relay.budget_unknown)
            with self.assertRaises(urllib.error.HTTPError) as next_call:
                urllib.request.urlopen(request("must stay local"), timeout=5)
            self.assertEqual(next_call.exception.code, 423)
            self.assertEqual(
                next_call.exception.headers.get("X-Relay-Stop-Reason"),
                "usage_unknown",
            )
            self.assertEqual(len(upstream_bodies), 3)
            self.assertEqual(len(relay.attempts), 3)
            self.assertTrue(relay.non_retryable_unknown)
            self.assertGreater(relay.input_tokens_reserved, 0)
            self.assertEqual(relay.output_tokens_reserved, 32)
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_malformed_sse_records_safe_category_and_freezes(self):
        provider_calls = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                provider_calls.append(self.rfile.read(int(self.headers["Content-Length"])))
                payload = b'data: {"type":"response.completed","secret":"not-json"\n\n'
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}/v1",
            credential="", model="gpt-6-luna", api_protocol="responses",
            api_safe_tool_names=True, allow_unauthenticated_loopback=True,
            max_requests=2, max_output_tokens=16,
        )

        def request():
            return urllib.request.Request(
                relay.base_url + "/responses",
                data=json.dumps({"model": "gpt-6-luna", "stream": True,
                                 "max_output_tokens": 16, "input": "synthetic"}).encode(),
                headers={"Authorization": "Bearer " + relay.token},
            )

        try:
            with urllib.request.urlopen(request(), timeout=5) as response:
                self.assertEqual(response.status, 200)
                response.read()
            self.assertEqual(relay.attempts[0]["state"], "unknown")
            self.assertEqual(relay.attempts[0]["local_error_code"], "malformed_sse_json")
            self.assertGreater(relay.attempts[0]["response_bytes_seen"], 0)
            self.assertNotIn("not-json", json.dumps(relay.attempts[0]))
            with self.assertRaises(urllib.error.HTTPError) as blocked:
                urllib.request.urlopen(request(), timeout=5)
            self.assertEqual(blocked.exception.code, 423)
            self.assertEqual(len(provider_calls), 1)
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_responses_relay_round_trips_function_names_and_usage(self):
        received = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                received.append((self.path, request))
                name = request["tools"][0]["name"]
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                events = [
                    {"type": "response.output_item.added", "output_index": 0,
                     "item": {"type": "function_call", "id": "fc-1",
                              "call_id": "call-1", "name": name, "arguments": ""}},
                    {"type": "response.function_call_arguments.delta", "output_index": 0,
                     "item_id": "fc-1", "delta": "{}"},
                    {"type": "response.function_call_arguments.done", "output_index": 0,
                     "item_id": "fc-1", "name": name, "arguments": "{}"},
                    {"type": "response.output_item.done", "output_index": 0,
                     "item": {"type": "function_call", "id": "fc-1",
                              "call_id": "call-1", "name": name, "arguments": "{}"}},
                    {"type": "response.completed", "response": {"usage": {
                        "input_tokens": 10, "output_tokens": 2, "total_tokens": 12,
                    }}},
                ]
                for event in events:
                    self.wfile.write(b"data: " + json.dumps(event).encode() + b"\n\n")

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}/v1",
            credential="synthetic-account-secret", model="gpt-6-luna",
            max_requests=1, max_output_tokens=16,
            max_input_tokens_total=10_000, max_output_tokens_total=16,
            input_price_milli_usd_per_million=100,
            output_price_milli_usd_per_million=500,
            api_safe_tool_names=True, api_protocol="responses",
        )
        body = {
            "model": "gpt-6-luna", "stream": True,
            "max_output_tokens": 16, "reasoning": {"effort": "max"},
            "input": [
                {"type": "function_call", "call_id": "call-old",
                 "name": "fs.read", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call-old", "output": "ok"},
            ],
            "tools": [{"type": "function", "name": "fs.read",
                        "parameters": {"type": "object"}}],
        }
        try:
            response = urllib.request.urlopen(urllib.request.Request(
                relay.base_url + "/responses", data=json.dumps(body).encode(),
                headers={"Authorization": "Bearer " + relay.token},
            )).read()
            self.assertEqual(received[0][0], "/v1/responses")
            sent = received[0][1]
            alias = sent["tools"][0]["name"]
            self.assertTrue(alias.startswith("t_"))
            self.assertEqual(sent["input"][0]["name"], alias)
            self.assertEqual(sent["reasoning"], {"effort": "max"})
            events = [json.loads(line[6:]) for line in response.splitlines()
                      if line.startswith(b"data: {")]
            self.assertEqual(events[0]["item"]["name"], "fs.read")
            self.assertEqual(events[2]["name"], "fs.read")
            self.assertEqual(events[3]["item"]["call_id"], "call-1")
            self.assertEqual(relay.attempts[0]["state"], "completed")
            self.assertEqual(relay.attempts[0]["input_tokens"], 10)
            self.assertEqual(relay.attempts[0]["output_tokens"], 2)
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_responses_cached_input_uses_proxy_read_price_and_peak_miss_gate(self):
        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                event = {
                    "type": "response.completed",
                    "response": {"usage": {
                        "input_tokens": 1_000,
                        "input_tokens_details": {"cached_tokens": 800},
                        "output_tokens": 200,
                    }},
                }
                self.wfile.write(
                    b"data: " + json.dumps(event).encode() + b"\n\n"
                    b"data: [DONE]\n\n"
                )

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}/v1",
            credential="synthetic-account-secret", model="gpt-6-luna",
            max_requests=1, max_output_tokens=200,
            max_input_tokens_total=10_000, max_output_tokens_total=200,
            max_peak_miss_usd=1,
            input_price_milli_usd_per_million=400,
            cached_input_price_milli_usd_per_million=40,
            output_price_milli_usd_per_million=2_000,
            api_protocol="responses",
        )
        body = json.dumps({
            "model": "gpt-6-luna", "stream": True,
            "max_output_tokens": 200, "input": "synthetic usage probe",
        }).encode()
        try:
            response = urllib.request.urlopen(urllib.request.Request(
                relay.base_url + "/responses", data=body,
                headers={"Authorization": "Bearer " + relay.token},
            )).read()
            self.assertIn(b"[DONE]", response)
            self.assertEqual(relay.attempts[0]["state"], "completed")
            self.assertEqual(relay.attempts[0]["cached_input_tokens"], 800)
            self.assertEqual(relay.attempts[0]["estimated_cost_nano_usd"], 512_000)
            self.assertEqual(relay.attempts[0]["peak_miss_cost_nano_usd"], 800_000)
            with tempfile.TemporaryDirectory() as directory:
                receipt_path = Path(directory) / "receipt.json"
                relay.save_receipt(receipt_path)
                receipt = json.loads(receipt_path.read_text())
                self.assertEqual(receipt["estimated_cost_nano_usd"], 512_000)
                self.assertEqual(receipt["peak_miss_cost_nano_usd"], 800_000)
                self.assertEqual(receipt["cached_input_tokens_committed"], 800)
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_close_waits_for_an_admitted_response_to_settle_before_receipt(self):
        entered = threading.Event()

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                entered.set()
                time.sleep(0.6)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(
                    b'data: {"usage":{"prompt_tokens":7,"completion_tokens":2}}\n\n'
                    b'data: [DONE]\n\n'
                )

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        provider_thread = threading.Thread(target=provider.serve_forever, daemon=True)
        provider_thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}",
            credential="synthetic-account-secret", model="test-model",
        )
        body = json.dumps({"model": "test-model", "stream": True,
                           "max_tokens": 8}).encode()

        def call():
            urllib.request.urlopen(urllib.request.Request(
                relay.base_url + "/chat/completions", data=body,
                headers={"Authorization": "Bearer " + relay.token},
            )).read()

        client_thread = threading.Thread(target=call)
        client_thread.start()
        try:
            self.assertTrue(entered.wait(2))
            relay.close()
            client_thread.join(timeout=2)
            self.assertFalse(client_thread.is_alive())
            self.assertEqual(relay.active_calls, 0)
            self.assertEqual(relay.attempts[0]["state"], "completed")
            self.assertEqual(relay.input_tokens_reserved, 0)
        finally:
            if not relay.closed:
                relay.close()
            provider.shutdown()
            provider.server_close()
            provider_thread.join()

    def test_provider_request_interval_paces_a_serial_trial(self):
        arrivals = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                arrivals.append(time.monotonic())
                self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(
                    b'data: {"usage":{"prompt_tokens":1,"completion_tokens":1}}\n\n'
                    b'data: [DONE]\n\n'
                )

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}",
            credential="synthetic-account-secret", model="test-model",
            max_requests=2, min_request_interval_ms=80,
        )
        body = json.dumps({"model": "test-model", "stream": True,
                           "max_tokens": 1}).encode()
        request = urllib.request.Request(
            relay.base_url + "/chat/completions", data=body,
            headers={"Authorization": "Bearer " + relay.token},
        )
        try:
            urllib.request.urlopen(request).read()
            urllib.request.urlopen(request).read()
            self.assertEqual(len(arrivals), 2)
            self.assertGreaterEqual(arrivals[1] - arrivals[0], 0.06)
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_mimo_tool_aliases_round_trip_without_changing_text_or_core_names(self):
        received = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                received.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(
                    b'data: {"choices":[{"delta":{"content":"ok","tool_calls":null}}]}\n\n'
                )
                self.wfile.write(b'data: {"choices":[{"delta":null}]}\n\n')
                event = {"choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "call-1", "type": "function",
                     "function": {"name": _tool_alias("fs.read"), "arguments": "{}"}},
                    {"index": 2, "type": "function",
                     "function": {"name": _tool_alias("fs.read"), "arguments": "{}"}},
                ]}}]}
                line = b"data: " + json.dumps(event).encode() + b"\n\n"
                self.wfile.write(line[:17])
                self.wfile.write(line[17:])
                self.wfile.write(
                    b'data: {"choices":null,"usage":{"prompt_tokens":10,"completion_tokens":2}}\n\n'
                    b'data: [DONE]\n\n'
                )

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}",
            credential="synthetic-account-secret", model="mimo-v2.6-flash",
            max_peak_miss_usd=1, input_price_milli_usd_per_million=140,
            output_price_milli_usd_per_million=280, api_safe_tool_names=True,
            prefer_max_completion_tokens=True, omit_stream_options=True,
            repair_missing_tool_call_ids=True,
        )
        request = {
            "model": "mimo-v2.6-flash", "stream": True, "max_tokens": 8,
            "stream_options": {"include_usage": True},
            "tools": [{"type": "function", "function": {"name": "fs.read",
                        "parameters": {"type": "object"}}}],
            "messages": [
                {"role": "user", "content": "keep fs.read in user text"},
                {"role": "assistant", "content": "checking", "tool_calls": None},
                {"role": "assistant", "tool_calls": [{"type": "function",
                    "id": "old-call", "function": {"name": "fs.read", "arguments": "{}"}}]},
            ],
        }
        try:
            response = urllib.request.urlopen(urllib.request.Request(
                relay.base_url + "/chat/completions", data=json.dumps(request).encode(),
                headers={"Authorization": "Bearer " + relay.token},
            )).read()
            self.assertEqual(received[0]["tools"][0]["function"]["name"],
                             _tool_alias("fs.read"))
            self.assertEqual(received[0]["messages"][2]["tool_calls"][0]["function"]["name"],
                             _tool_alias("fs.read"))
            self.assertEqual(received[0]["messages"][0]["content"],
                             "keep fs.read in user text")
            self.assertEqual(received[0]["max_completion_tokens"], 8)
            self.assertNotIn("max_tokens", received[0])
            self.assertNotIn("stream_options", received[0])
            self.assertIn(b'"name":"fs.read"', response)
            self.assertNotIn(_tool_alias("fs.read").encode(), response)
            events = [json.loads(line[6:]) for line in response.splitlines()
                      if line.startswith(b"data: {")]
            self.assertEqual(events[0]["choices"][0]["delta"]["tool_calls"], [])
            self.assertEqual(events[1]["choices"][0]["delta"], {"tool_calls": []})
            self.assertEqual(
                events[2]["choices"][0]["delta"]["tool_calls"][0]["id"],
                "call-1",
            )
            generated = events[2]["choices"][0]["delta"]["tool_calls"][1]["id"]
            self.assertTrue(generated.startswith("call_relay_"))
            self.assertNotEqual(generated, "call-1")
            self.assertEqual(events[-1]["choices"], [])
            self.assertEqual(relay.attempts[0]["state"], "completed")
            self.assertEqual(relay.attempts[0]["repaired_tool_call_ids"], 1)
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "receipt.json"
                relay.save_receipt(path)
                saved = json.loads(path.read_text())
                self.assertEqual(saved["estimated_cost_nano_usd"], 1960)
                self.assertNotIn("synthetic-account-secret", path.read_text())
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_later_provider_id_is_preserved_without_synthesis(self):
        lines = [
            b'data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"function":{"name":"fs.read","arguments":"{}"}}]}}]}',
            b'data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"id":"call-late"}]}}]}',
            b'data: [DONE]',
        ]
        rewritten, count = _repair_missing_tool_ids(lines)
        self.assertEqual(count, 0)
        self.assertNotIn(b"call_relay_", b"\n".join(rewritten))
        self.assertIn(b"call-late", rewritten[1])
        malformed = [
            b'data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"id":""}]}}]}'
        ]
        rewritten, count = _repair_missing_tool_ids(malformed)
        self.assertEqual(count, 0)
        self.assertNotIn(b"call_relay_", rewritten[0])

    def test_tool_alias_rejects_names_that_cannot_fit_provider_limit(self):
        with self.assertRaisesRegex(ValueError, "64-character"):
            _tool_alias("x" * 100)

    def test_upstream_http_status_is_recorded_without_body_and_freezes_locally(self):
        calls = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                calls.append(1)
                self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(429)
                self.end_headers()
                self.wfile.write(b"synthetic-upstream-secret")

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}",
            credential="synthetic-account-secret", model="test-model",
        )
        body = json.dumps({"model": "test-model", "max_tokens": 8,
                           "stream": True}).encode()
        req = urllib.request.Request(
            relay.base_url + "/chat/completions", data=body,
            headers={"Authorization": "Bearer " + relay.token},
        )
        try:
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(req)
            self.assertEqual(caught.exception.code, 502)
            self.assertEqual(relay.attempts[0]["upstream_status_code"], 429)
            self.assertTrue(relay.budget_unknown)
            self.assertNotIn("synthetic-upstream-secret", json.dumps(relay.attempts))
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(req)
            self.assertEqual(caught.exception.code, 423)
            self.assertEqual(
                caught.exception.headers.get("X-Relay-Stop-Reason"), "usage_unknown"
            )
            self.assertEqual(calls, [1])
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_peak_miss_cost_gate_rejects_before_upstream_and_after_overage(self):
        received = []

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                received.append(1)
                self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(
                    b'data: {"usage":{"prompt_tokens":3400000,"completion_tokens":1}}\n\n'
                    b'data: [DONE]\n\n'
                )

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(
            upstream=f"http://127.0.0.1:{provider.server_port}",
            credential="synthetic-account-secret", model="test-model",
            max_requests=4, max_output_tokens=1_000_000,
            max_peak_miss_usd=1,
        )

        def send(cap):
            body = json.dumps({"model": "test-model", "max_tokens": cap,
                               "stream": True}).encode()
            return urllib.request.urlopen(urllib.request.Request(
                relay.base_url + "/chat/completions", data=body,
                headers={"Authorization": "Bearer " + relay.token}
            )).read()

        try:
            with self.assertRaises(urllib.error.HTTPError) as caught:
                send(1_000_000)
            self.assertEqual(caught.exception.code, 429)
            self.assertEqual(
                caught.exception.headers.get("X-Relay-Stop-Reason"), "price_limit"
            )
            self.assertEqual(relay.rejections, {"price_limit": 1})
            self.assertEqual(received, [])
            self.assertIn(b"[DONE]", send(8))
            self.assertTrue(relay.budget_exceeded)
            with self.assertRaises(urllib.error.HTTPError) as caught:
                send(8)
            self.assertEqual(caught.exception.code, 429)
            self.assertEqual(received, [1])
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_aggregate_token_budget_reserves_and_fails_closed_on_unknown_usage(self):
        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(b"data: {\"choices\":[]}\n\ndata: [DONE]\n\n")

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(upstream=f"http://127.0.0.1:{provider.server_port}",
                                credential="synthetic-account-secret", model="test-model",
                                max_requests=4, max_input_tokens_total=10_000,
                                max_output_tokens_total=100)
        try:
            body = json.dumps({"model": "test-model", "max_tokens": 8,
                               "stream": True}).encode()
            req = urllib.request.Request(relay.base_url + "/chat/completions", data=body,
                headers={"Authorization": "Bearer " + relay.token})
            self.assertIn(b"[DONE]", urllib.request.urlopen(req).read())
            self.assertTrue(relay.budget_unknown)
            self.assertIsNone(relay.attempts[0]["response_terminal_event"])
            self.assertFalse(relay.attempts[0]["response_completed"])
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(req)
            self.assertEqual(caught.exception.code, 423)
            self.assertEqual(
                caught.exception.headers.get("X-Relay-Stop-Reason"), "usage_unknown"
            )
            self.assertEqual(relay.rejections, {"usage_unknown": 1})
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_streaming_auth_bounds_and_no_upstream_redirect(self):
        received = []
        redirect = [False]

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                received.append(self.headers.get("Authorization"))
                self.rfile.read(int(self.headers["Content-Length"]))
                if redirect[0]:
                    self.send_response(307)
                    self.send_header("Location", f"http://127.0.0.1:{self.server.server_port}/redirect-target")
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(b'data: {"usage":{"prompt_tokens":7,"completion_tokens":2}}\n\ndata: [DONE]\n\n')

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        relay = CredentialRelay(upstream=f"http://127.0.0.1:{provider.server_port}",
                                credential="synthetic-account-secret", model="test-model",
                                max_requests=1, max_body_bytes=256, max_output_tokens=8)
        def request(token=None, model="test-model", cap=8, suffix="/chat/completions", **overrides):
            req = urllib.request.Request(relay.base_url + suffix,
                data=json.dumps({"model": model, "max_tokens": cap, "stream": True, **overrides}).encode(),
                headers={"Authorization": "Bearer " + (token or relay.token)})
            return urllib.request.urlopen(req).read()
        try:
            for kwargs, status in [({"token": "bad"}, 401), ({"model": "other"}, 400),
                                   ({"cap": 9}, 400), ({"suffix": "/other"}, 404),
                                   ({"n": 2}, 400), ({"best_of": 2}, 400), ({"stream": False}, 400)]:
                with self.assertRaises(urllib.error.HTTPError) as caught:
                    request(**kwargs)
                self.assertEqual(caught.exception.code, status)
            self.assertEqual(received, [])
            self.assertIn(b"[DONE]", request())
            with self.assertRaises(urllib.error.HTTPError) as caught:
                request()
            self.assertEqual(caught.exception.code, 429)
            self.assertEqual(received, ["Bearer synthetic-account-secret"])
            self.assertEqual(relay.attempts[0]["usage"]["prompt_tokens"], 7)
            self.assertNotIn("synthetic-account-secret", json.dumps(relay.attempts))
            relay.close()
            redirect[0] = True
            relay = CredentialRelay(upstream=f"http://127.0.0.1:{provider.server_port}",
                                    credential="synthetic-account-secret", model="test-model")
            with self.assertRaises(urllib.error.HTTPError) as caught:
                request()
            self.assertEqual(caught.exception.code, 502)
            self.assertEqual(len(received), 2)  # exactly one attempt, no redirected credential
            self.assertEqual(relay.attempts[0]["error_type"], "HTTPError")
            self.assertEqual(relay.attempts[0]["upstream_status_code"], 307)
        finally:
            relay.close()
            provider.shutdown()
            provider.server_close()
            thread.join()

    def test_adapter_only_forwards_trial_nonce(self):
        agent = ContextAgentTerminalBench(grant_file="/tmp/grants.json")
        captured = {}
        async def capture(environment, **kwargs):
            captured.update(kwargs)
        agent.exec_as_agent = capture
        with patch.dict(os.environ, {"OPENAI_API_KEY": "synthetic-account-secret",
                "OPENAI_BASE_URL": "https://account.invalid",
                "OPENAI_BUFFER_STREAM_FOR_RETRY": "1",
                "OPENAI_REQUEST_TIMEOUT_SECS": "600",
                "AGENT_DISABLE_SHELL_EXEC": "1",
                "TB_RELAY_BASE_URL": "http://127.0.0.1:1234/v1", "TB_RELAY_TOKEN": "trial-test"}):
            asyncio.run(agent.run("Implement the task", object()))
        self.assertEqual(captured["env"]["OPENAI_API_KEY"], "trial-test")
        self.assertEqual(captured["env"]["OPENAI_BUFFER_STREAM_FOR_RETRY"], "1")
        self.assertEqual(captured["env"]["OPENAI_REQUEST_TIMEOUT_SECS"], "600")
        self.assertEqual(captured["env"]["AGENT_DISABLE_SHELL_EXEC"], "1")
        self.assertNotIn("synthetic-account-secret", repr(captured))
        with self.assertRaises(AdapterConfigurationError):
            ContextAgentTerminalBench(grant_file="/tmp/g", extra_env={"OPENAI_API_KEY": "secret"})


if __name__ == "__main__":
    unittest.main()
