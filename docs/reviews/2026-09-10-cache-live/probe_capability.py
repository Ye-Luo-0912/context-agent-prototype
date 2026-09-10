"""Two tiny, explicitly invoked API shape checks; no prompt-cache benefit claim."""
import copy
import hashlib
import json
import os
import re
import time
import uuid
from pathlib import Path

import requests


def environment(repo):
    values = dict(os.environ)
    allowed = {"OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL", "OPENAI_API_PROTOCOL",
               "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"}
    for line in (repo / "eval.env").read_text(encoding="utf-8-sig").splitlines():
        line = line.strip()
        if line.startswith("export "):
            line = line[7:].strip()
        name, sep, value = line.partition("=")
        name, value = name.strip(), value.strip()
        if not sep or name not in allowed or values.get(name, "").strip():
            continue
        if len(value) > 1 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        values[name] = value
    return values


def redacted_error(value, credential):
    text = str(value).replace(credential, "[credential]")
    text = re.sub(r"https?://[^\s\"']+", "[url]", text)
    text = re.sub(r"(?i)bearer\s+\S+|sk-[A-Za-z0-9_-]+|[A-Za-z0-9+/=-]{24,}", "[redacted]", text)
    return text[:600]


def main():
    repo = Path(__file__).resolve().parents[3]
    values = environment(repo)
    credential = values.get("OPENAI_API_KEY", "").strip()
    if not credential or values.get("OPENAI_API_PROTOCOL") != "responses":
        raise SystemExit("An existing credential and explicit Responses configuration are required.")
    path = Path(__file__).with_name(f"capability-shape-{time.time_ns()}.json")
    nonce = str(uuid.uuid4())
    payload = {"model": values["OPENAI_MODEL"], "stream": True, "store": False,
               "max_output_tokens": 128, "input": [
                   {"role": "system", "content": f"Synthetic API shape check {nonce}. Return exactly OK."},
                   {"role": "user", "content": [{"type": "input_text", "text": "Synthetic data: STATUS=OK."}]},
                   {"role": "user", "content": "Copy STATUS."}]}
    report = {"schema": "kv-capability-shape/v1", "model": values["OPENAI_MODEL"],
              "max_requests": 2, "max_output_tokens": 128, "transport_retries": False,
              "scope": "Tiny Responses field acceptance only; below cache measurement size",
              "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "raw_requests_saved": False, "raw_errors_saved": False, "requests": []}
    path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    with requests.Session() as session:
        session.headers["Authorization"] = "Bearer " + credential
        for scheme in ("http", "https"):
            proxy = values.get(scheme.upper() + "_PROXY")
            if proxy:
                session.proxies[scheme] = proxy
        for number, mode in enumerate(("content_array_control", "explicit_boundary"), 1):
            body = copy.deepcopy(payload)
            if mode == "explicit_boundary":
                body["input"][1]["content"][0]["prompt_cache_breakpoint"] = {"mode": "explicit"}
                body["prompt_cache_options"] = {"mode": "explicit"}
            report["attempted_requests"] = number
            path.write_text(json.dumps(report, indent=2), encoding="utf-8")
            start = time.monotonic()
            row = {"mode": mode, "request_sha256": hashlib.sha256(json.dumps(body, sort_keys=True).encode()).hexdigest()}
            try:
                with session.post(values["OPENAI_BASE_URL"].rstrip("/") + "/responses",
                                  json=body, stream=True, timeout=(10, 20), allow_redirects=False) as response:
                    incoming = bytearray()
                    for chunk in response.iter_content(4096):
                        incoming.extend(chunk)
                        if len(incoming) > 128 * 1024 or time.monotonic() - start > 40:
                            raise RuntimeError("response budget exceeded")
                    row["http_status"] = response.status_code
                    row["response_bytes"] = len(incoming)
                    if response.status_code >= 400:
                        try:
                            document = json.loads(incoming)
                        except (ValueError, UnicodeDecodeError):
                            document = {"message": "non-JSON error body"}
                        error = document.get("error", document)
                        if not isinstance(error, dict):
                            error = {"message": error}
                        # A bounded redacted diagnostic may be viewed locally,
                        # but the persisted report stores only structural facts.
                        print(json.dumps({"mode": mode, "status": response.status_code,
                                          "error": {key: redacted_error(error[key], credential)
                                                    for key in ("message", "type", "code", "param", "detail") if key in error}}, ensure_ascii=False), flush=True)
                        row["error_keys"] = [key for key in ("message", "type", "code", "param", "detail") if key in error]
                    else:
                        for line in incoming.decode("utf-8").splitlines():
                            if not line.startswith("data: ") or line[6:] == "[DONE]":
                                continue
                            event = json.loads(line[6:])
                            if event.get("type") == "response.completed":
                                completed = event.get("response", {})
                                row["completed"] = True
                                row["usage"] = completed.get("usage")
                        print(json.dumps({"mode": mode, "status": response.status_code, "completed": row.get("completed", False)}), flush=True)
            except (requests.RequestException, RuntimeError, ValueError) as error:
                row["error_class"] = type(error).__name__
            row["elapsed_ms"] = round((time.monotonic() - start) * 1000)
            report["requests"].append(row)
            path.write_text(json.dumps(report, indent=2), encoding="utf-8")
            if mode == "content_array_control" and not row.get("completed"):
                break
    print(f"Report path: {path}", flush=True)


if __name__ == "__main__":
    main()
