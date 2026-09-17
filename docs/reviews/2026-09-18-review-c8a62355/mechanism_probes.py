#!/usr/bin/env python3
"""Local mechanisms only, NOT execution of the repository's Rust implementation.
Pinned source: c8a623554f762505061cdfdd7466ccaa995a3de0.
Ports the relevant artifact capture/broker branches and demonstrates Linux flock
semantics using disposable files. No network, repository mutation, or paid API.
"""
from __future__ import annotations
import errno
import hashlib
import json
import os
from pathlib import Path
import tempfile

CAP = 2 * 1024 * 1024 - 400 * 8
MODEL_CAP = 16_000
SCAN_CAP = 8 * 1024 * 1024
REFERENCE = 'artifact://v1/00000000-0000-4000-8000-000000000001/process/' + 'a' * 64

def boundary(raw: bytes, n: int) -> bool:
    return n == 0 or n == len(raw) or raw[n] & 0xC0 != 0x80

def artifact_capture(raw: bytes, start: int = 1, offset: int = 0, end: int | None = None) -> dict:
    """Faithful control-flow port for the valid UTF-8 test fixtures below.
    Bounded file opening/digest checks are out of scope; source bytes are supplied.
    Footer punctuation is synthesized, while cursor/coverage decisions are ported.
    """
    end = start + 199 if end is None else end
    assert start > 0 and end >= start and end - start + 1 <= 400
    parts = raw[:SCAN_CAP].split(b'\n')
    raw_lines = [p + b'\n' for p in parts[:-1]]
    if parts[-1]:
        raw_lines.append(parts[-1])
    captured: list[str] = []
    captured_bytes = 0
    truncated = False
    mid: tuple[int, int] | None = None
    last = 0
    actually_captured_lines: list[int] = []
    for line_no, row in enumerate(raw_lines, 1):
        if line_no < start or line_no > end or mid is not None:
            continue
        eol = 2 if row.endswith(b'\r\n') else 1 if row.endswith(b'\n') else 0
        content_len = len(row) - eol
        skip = offset if line_no == start else 0
        if skip:
            assert skip < content_len and boundary(row, skip)
        rest = row[skip:]
        rendered = rest.decode('utf-8', errors='replace')
        room = max(0, CAP - captured_bytes)
        if room == 0:
            continue
        if len(rendered.encode()) <= room:
            captured.append(rendered)
            captured_bytes += len(rendered.encode())
            last = line_no
            actually_captured_lines.append(line_no)
            continue
        take = min(room, len(rest))
        while take > 0 and not boundary(rest, take):
            take -= 1
        chunk = rest[:take]
        if len(chunk.decode('utf-8', errors='replace').encode()) > room:
            fallback = room // 3
            while fallback > 0 and not boundary(rest, fallback):
                fallback -= 1
            chunk = rest[:fallback]
        if not chunk:
            # This continue reproduces the source's skip-without-cursor defect.
            continue
        text = chunk.decode('utf-8', errors='replace')
        captured.append(text)
        if not ''.join(captured).endswith('\n'):
            captured.append('\n')
        captured_bytes += len(text.encode())
        last = line_no
        actually_captured_lines.append(line_no)
        shown = skip + len(chunk)
        if shown < content_len:
            truncated = True
            mid = (line_no, shown)
    scan_complete = len(raw) < SCAN_CAP
    total = len(raw_lines)
    text = ''.join(captured)
    lines = text.splitlines()
    first_unshown = mid[0] if mid else max(last + 1, start)
    in_window_unshown = first_unshown <= min(end, total)
    more = not scan_complete or in_window_unshown or (scan_complete and end < total)
    nxt = (mid or (first_unshown, 0)) if in_window_unshown else ((end + 1, 0) if more else (end, 0))
    whole = start == 1 and end >= total and not truncated and offset == 0
    body = '\n'.join(f'{start + i:6} | {line}' for i, line in enumerate(lines))
    if more or not scan_complete or not whole:
        if more:
            cursor = f'reference={REFERENCE} start_line={nxt[0]}'
            if nxt[1]:
                cursor += f' line_byte_offset={nxt[1]}'
            body += '\n[coverage] continue with artifact.read ' + cursor
        else:
            body += f'\n[coverage] end of artifact ({total} lines)'
    return {'body': body, 'has_more': more, 'window_truncated': truncated,
            'next': nxt, 'source_lines': actually_captured_lines,
            'display_numbers': list(range(start, start + len(lines))),
            'scan_complete': scan_complete, 'total_lines': total}

def broker_clip(body: str) -> str:
    if len(body) <= MODEL_CAP:
        return body
    spill = 'artifact://v1/00000000-0000-4000-8000-000000000001/tool-output/' + hashlib.sha256(body.encode()).hexdigest()
    marker = f'\n...[output broker truncated model_content from {len(body)} chars to the {MODEL_CAP}-char cap. Full output: {spill}.]...\n'
    n = MODEL_CAP - len(marker)
    head = n // 2
    return body[:head] + marker + body[-(n-head):]

def probe_cursor_vs_visible() -> dict:
    raw = bytearray(b'a' * (3 * 1024 * 1024))
    marks = {1024*1024: b'PAGE1-MIDDLE-SENTINEL',
             2*1024*1024: b'EXISTING-TEST-NEAR-BOUNDARY',
             5*1024*1024//2: b'PAGE2-MIDDLE-SENTINEL'}
    for position, mark in marks.items():
        raw[position:position+len(mark)] = mark
    raw += b'\n'
    start, offset = 1, 0
    pages = []
    seen: set[str] = set()
    for _ in range(8):
        result = artifact_capture(bytes(raw), start, offset)
        visible = broker_clip(result['body'])
        seen.update(mark.decode() for mark in marks.values() if mark.decode() in visible)
        pages.append({'request': [start, offset], 'captured_chars': len(result['body']),
                      'visible_chars': len(visible), 'next': result['next'],
                      'has_more': result['has_more'], 'window_truncated': result['window_truncated'],
                      'broker_clipped': 'output broker truncated' in visible,
                      'end_marker_visible': 'end of artifact' in visible})
        if not result['has_more']:
            break
        start, offset = result['next']
    assert 'EXISTING-TEST-NEAR-BOUNDARY' in seen
    assert 'PAGE1-MIDDLE-SENTINEL' not in seen and 'PAGE2-MIDDLE-SENTINEL' not in seen
    assert pages[-1]['end_marker_visible'] and pages[-1]['broker_clipped']
    return {'scope': 'capture/broker control-flow port, not Rust E2E',
            'source_bytes': len(raw), 'pages': pages, 'seen': sorted(seen),
            'omitted_markers': sorted(set(m.decode() for m in marks.values()) - seen)}


def probe_ordinary_log_pages() -> dict:
    rows = []
    for i in range(1, 501):
        tag = f'LINE-{i:04d} '
        if i in (100, 300):
            tag += f'REQUIRED-MIDDLE-{i} '
        rows.append(tag + 'x' * (150-len(tag)) + '\n')
    raw = ''.join(rows).encode()
    start, offset = 1, 0
    pages, delivered = [], []
    for _ in range(8):
        result = artifact_capture(raw, start, offset)
        visible = broker_clip(result['body'])
        delivered.append(visible)
        pages.append({'start_line': start, 'next': result['next'],
                      'internal_window_truncated': result['window_truncated'],
                      'broker_clipped': 'output broker truncated' in visible,
                      'has_more': result['has_more']})
        if not result['has_more']:
            break
        start, offset = result['next']
    missing = [f'REQUIRED-MIDDLE-{i}' for i in (100, 300)
               if not any(f'REQUIRED-MIDDLE-{i}' in page for page in delivered)]
    assert len(pages) == 3 and len(missing) == 2
    return {'scope': 'valid 500-line log; capture/broker port, NOT Rust E2E',
            'source_bytes': len(raw), 'pages': pages, 'missing_markers': missing}

def probe_utf8_gap() -> dict:
    raw = b'a' * (CAP - 2) + b'\n' + '界\n'.encode() + b'x\n'
    result = artifact_capture(raw)
    assert result['source_lines'] == [1, 3]
    assert result['display_numbers'] == [1, 2]
    assert not result['has_more'] and not result['window_truncated']
    assert '界' not in result['body']
    return {'scope': 'artifact capture control-flow port, not Rust E2E',
            'source_line_numbers_captured': result['source_lines'],
            'rendered_line_numbers': result['display_numbers'],
            'omitted_source_line': 2, 'has_more': result['has_more'],
            'window_truncated': result['window_truncated'],
            'coverage_footer_present': '[coverage]' in result['body']}

def probe_rotating_lock() -> dict:
    """Actual POSIX file handles and flock; not repository WAL JSON/codec.
    No external files are touched. Uses separately opened file descriptions.
    """
    try:
        import fcntl
    except ImportError:
        return {'status': 'NOT_RUN', 'reason': 'requires POSIX flock'}
    with tempfile.TemporaryDirectory(prefix='wal-lock-mechanism-') as root:
        g1, g2, meta = (Path(root) / n for n in ('operations', 'operations.g2', 'meta'))
        g1.write_bytes(b'old-wal')
        meta.write_text('1')
        a1 = g1.open('r+b', buffering=0)
        b1 = g1.open('r+b', buffering=0)
        a2 = None
        other = None
        try:
            fcntl.flock(a1, fcntl.LOCK_EX | fcntl.LOCK_NB)
            cached_generation_b = meta.read_text()
            # B has observed generation 1 and opened it, but has not locked yet.
            g2.write_bytes(b'current-authority-wal')
            a2 = g2.open('r+b', buffering=0)
            fcntl.flock(a2, fcntl.LOCK_EX | fcntl.LOCK_NB)
            meta.write_text('2')
            a1.close()  # same old-handle release point as writer.file replacement
            g1.unlink()
            fcntl.flock(b1, fcntl.LOCK_EX | fcntl.LOCK_NB)
            stale_locked = True
            unlinked = os.fstat(b1.fileno()).st_nlink == 0
            before = g2.stat().st_size
            # Stale B next compacts to g2; source code truncates before try_lock.
            other = g2.open('w+b', buffering=0)
            size_before_lock_attempt = g2.stat().st_size
            blocked = False
            try:
                fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                blocked = True
            assert stale_locked and unlinked and blocked and size_before_lock_attempt == 0
            return {'scope': 'actual Linux flock mechanism; NOT FileOperationJournal execution',
                    'b_cached_generation': int(cached_generation_b),
                    'published_generation': int(meta.read_text()),
                    'stale_generation_lock_acquired': stale_locked,
                    'stale_wal_unlinked': unlinked,
                    'current_wal_size_before': before,
                    'current_wal_size_after_open_before_lock': size_before_lock_attempt,
                    'later_lock_correctly_refused': blocked,
                    'product_guard_note': 'Normal distinct Workspace::open calls retain an additional effect-journal lock; this experiment does not bypass it.'}
        finally:
            for handle in (other, a2, b1, a1):
                if handle is not None and not handle.closed:
                    handle.close()


def probe_pending_reconcile() -> dict:
    """Decision-table port of reconcile snapshot, classification, and commit.
    Assumes all three on-disk blobs already passed existing parse/id checks.
    """
    hot = {'a': {'checksum': 'sha-a', 'semantic': 'live'}}
    pending = [('b', 'card-b'), ('c', 'card-c')]
    blobs = {'a': 'sha-a', 'b': 'sha-b', 'c': 'sha-c'}
    resident = set()
    map_checksums = {key: row['checksum'] for key, row in hot.items()}
    hydration_complete = False
    rebuilt = []
    for item_id, checksum in blobs.items():
        if item_id not in map_checksums and item_id not in resident:
            # The source's metadata_complete check is in the deletion arm,
            # not this unconditional ownerless->rebuilt arm.
            rebuilt.append((item_id, checksum))
    for item_id, checksum in rebuilt:
        if item_id not in hot:
            hot[item_id] = {'checksum': checksum, 'semantic': 'blob-captured'}
    duplicate = sorted(set(hot) & {row[0] for row in pending})
    assert duplicate == ['b', 'c'] and len(hot) == 3 and len(pending) == 2
    return {'scope': 'reconcile decision-table port; NOT Rust execution',
            'hydration_complete': hydration_complete, 'initial_hot': 1,
            'initial_pending': 2, 'final_hot': len(hot), 'final_pending': len(pending),
            'rebuilt_existing_cold_owners': [row[0] for row in rebuilt],
            'duplicate_hot_and_pending_ids': duplicate,
            'normal_fixture_condition': 'positive hot cap already full; valid pending cards/blobs'}

if __name__ == '__main__':
    data = {'baseline': 'c8a623554f762505061cdfdd7466ccaa995a3de0',
            'rust_repository_tests': 'NOT_RUN',
            'cold_reconcile_owner_duplication': probe_pending_reconcile(),
            'artifact_cursor_vs_model_visibility': probe_cursor_vs_visible(),
            'ordinary_log_paging': probe_ordinary_log_pages(),
            'artifact_utf8_gap': probe_utf8_gap(),
            'wal_lock_rotation': probe_rotating_lock()}
    output = Path(__file__).with_name('MECHANISM_RESULTS.json')
    output.write_text(json.dumps(data, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(data, ensure_ascii=False, indent=2))
