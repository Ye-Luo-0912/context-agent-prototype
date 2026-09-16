#!/usr/bin/env python3
"""Source-derived mechanism probes, NOT executions of the repository's Rust code.

No network calls and no repo mutation. Node.js is used only as the ECMAScript
JSON.stringify reference. The Ryu string samples are inputs to the ported
scientific_from_ryu helper; this script does not run the Ryu Rust crate.
"""
from __future__ import annotations
import hashlib
import json
import shutil
import subprocess
from pathlib import Path

BASELINE = '71f8a58614fcabbbc9bc9602fe85ef453df56765'

def trim(value: str) -> str:
    if '.' in value:
        value = value.rstrip('0').rstrip('.')
    return value

def source_scientific(mantissa: str, exponent: int) -> str:
    """Faithful port of the inspected branch structure in jcs.rs."""
    digits = mantissa.replace('.', '')
    k = len(digits)
    point = exponent + 1
    if 0 <= point <= 21 and point >= k:
        return digits + '0' * (point - k)
    if -6 <= point < 0:
        return trim('0.' + '0' * (-point) + digits)
    if 0 <= point <= 21 and point < k:
        return trim(digits[:point] + '.' + digits[point:])
    return trim(digits[0] + ('.' + digits[1:] if len(digits) > 1 else '')) + 'e' + ('+' if point - 1 >= 0 else '') + str(point - 1)

def source_number_from_ryu(sample: str) -> str:
    negative = sample.startswith('-')
    sample = sample.lstrip('-')
    if 'e' in sample:
        mantissa, exponent = sample.split('e')
        output = source_scientific(mantissa, int(exponent))
    else:
        output = sample.removesuffix('.0')
    return ('-' if negative else '') + output

def artifact_capture_ascii(body: bytes, start: int = 1, end: int = 200) -> dict:
    """Port of capture/cursor logic for ASCII fixtures; no broker simulation."""
    capture_cap = 2 * 1024 * 1024 - 400 * 8
    scan_cap = 8 * 1024 * 1024
    assert body.isascii()
    visible = body[:scan_cap]
    lines = visible.splitlines(keepends=True)
    captured = bytearray()
    captured_bytes = 0
    clipped = False
    last_captured = 0
    for count, line in enumerate(lines, 1):
        if not start <= count <= end:
            continue
        room = max(0, capture_cap - captured_bytes)
        if room == 0:
            continue
        if len(line) <= room:
            captured.extend(line)
            captured_bytes += len(line)
            last_captured = count
        else:
            captured.extend(line[:room])
            if not captured.endswith(b'\n'):
                captured.extend(b'\n')
            captured_bytes += room
            clipped = True
            last_captured = count
    count = len(lines)
    complete = len(visible) < scan_cap
    first_unshown = max(last_captured + 1, start)
    in_window = first_unshown <= min(end, count)
    beyond_window = complete and end < count
    has_more = (not complete) or in_window or beyond_window
    next_start = first_unshown if in_window else (end + 1 if has_more else end)
    return {
        'counted_lines': count,
        'captured_bytes_before_numbering': len(captured),
        'window_truncated': clipped,
        'scan_complete': complete,
        'has_more': has_more,
        'next_start_line': next_start,
        'suffix_sentinel_visible': b'UNSEEN-SUFFIX-SENTINEL' in captured,
        'reports_end_of_artifact': not has_more,
    }

def main() -> None:
    node = shutil.which('node')
    if not node:
        raise SystemExit('Node.js required for the ECMAScript reference')
    samples = ['1e-8', '1e-7', '1.2e-7', '-1e-7', '1e-6', '1e20', '1e21', '1e23']
    js = 'const a=JSON.parse(process.argv[1]); console.log(JSON.stringify(a.map(x=>JSON.stringify(Number(x)))));'
    reference = json.loads(subprocess.check_output([node, '-e', js, json.dumps(samples)], text=True, timeout=10))
    numeric = []
    for sample, expected in zip(samples, reference):
        got = source_number_from_ryu(sample)
        numeric.append({
            'ryu_rendering_input': sample,
            'ported_repository_helper': got,
            'node_json_stringify': expected,
            'same_bytes': got == expected,
            'ported_object_sha256': hashlib.sha256(('{"n":' + got + '}').encode()).hexdigest(),
            'reference_object_sha256': hashlib.sha256(('{"n":' + expected + '}').encode()).hexdigest(),
        })
    assert [r['ryu_rendering_input'] for r in numeric if not r['same_bytes']] == ['1e-7', '1.2e-7', '-1e-7']
    initial = artifact_capture_ascii(b'line\n' * 450)
    continuation_start = initial['next_start_line']
    defaults = {'start_line': continuation_start, 'end_line': 200}
    invalid_default = defaults['end_line'] < defaults['start_line']
    assert continuation_start == 201 and invalid_default
    long_body = b'a' * (3 * 1024 * 1024) + b'UNSEEN-SUFFIX-SENTINEL\n'
    long_read = artifact_capture_ascii(long_body)
    assert long_read['window_truncated'] and not long_read['has_more'] and not long_read['suffix_sentinel_visible']
    result = {
        'baseline': BASELINE,
        'scope': 'Mechanism ports + actual Node reference only; no Rust/.NET repository tests executed.',
        'node_version': subprocess.check_output([node, '--version'], text=True, timeout=5).strip(),
        'jcs_numeric_boundary': numeric,
        'suggested_artifact_continuation': {'first_read': initial, 'next_call_effective_range': defaults, 'rejected_as_invalid_range': invalid_default},
        'single_long_line': long_read,
        'not_executed': ['cargo test', 'dotnet test', 'real process cancellation drill', 'paid vendor API', 'full Runtime assembly sequence'],
    }
    out = Path(__file__).with_name('MECHANISM_CHECKS.json')
    out.write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({'jcs_mismatches': sum(not r['same_bytes'] for r in numeric), 'invalid_suggested_continuation': invalid_default, 'unseen_long_line_suffix_reported_as_end': long_read['reports_end_of_artifact'], 'output': str(out)}, indent=2))

if __name__ == '__main__':
    main()
