"""Deterministic repository snapshots and atomic restore (SPEC.md V4).

Python standard library only.  Never executes package payloads, never touches
the network, never mutates the source repository.
"""
import hashlib
import io
import json
import os
import re
import shutil
import sqlite3
import stat
import struct
import sys
import zipfile

FORMAT = 'package-snapshot-v1'
SCHEMA_VERSION = 2
NAME_RE = re.compile(r'\A[a-z][a-z0-9-]{0,63}\Z')
DIGEST_RE = re.compile(r'\A[0-9a-f]{64}\Z')
HEX_RE = re.compile(r'\A[0-9a-f]+\Z')
ZIP_TIMESTAMP = (1980, 1, 1, 0, 0, 0)

MAX_MEMBERS = 256
MAX_MEMBER_BYTES = 2 * 1024 * 1024
MAX_TOTAL_MEMBER_BYTES = 8 * 1024 * 1024
MAX_ARCHIVE_BYTES = 9 * 1024 * 1024
MAX_RECEIPTS = 128
MAX_CURRENT = 64
MAX_PACKAGES = 128


class SnapshotError(ValueError):
    """Raised for any invalid, unsafe, incomplete or conflicting input."""


def _fail(message):
    raise SnapshotError(message)


def _canonical_bytes(obj):
    try:
        text = json.dumps(obj, ensure_ascii=False, sort_keys=True,
                          separators=(',', ':'), allow_nan=False)
    except (TypeError, ValueError, OverflowError) as exc:
        _fail('value is not canonical JSON: %s' % (exc,))
    return text.encode('utf-8')


def _no_duplicates(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise SnapshotError('duplicate JSON key %r' % (key,))
        out[key] = value
    return out


def _parse_canonical(data, label):
    if data.startswith(b'\xef\xbb\xbf'):
        _fail('%s must not have a BOM' % (label,))
    try:
        text = data.decode('utf-8')
    except UnicodeDecodeError:
        _fail('%s is not valid UTF-8' % (label,))
    try:
        obj = json.loads(text, object_pairs_hook=_no_duplicates,
                         parse_constant=_reject_constant)
    except SnapshotError:
        raise
    except ValueError as exc:
        _fail('%s is not valid JSON: %s' % (label, exc))
    if _canonical_bytes(obj) != data:
        _fail('%s is not canonical JSON' % (label,))
    return obj


def _reject_constant(name):
    raise SnapshotError('non-finite JSON constant %r' % (name,))


def _sha256(data):
    return hashlib.sha256(data).hexdigest()


def _is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def _check_int(value, label, minimum):
    if not _is_int(value) or value < minimum:
        _fail('%s must be an integer >= %d' % (label, minimum))
    return value


def _check_name(value, label):
    if not isinstance(value, str) or NAME_RE.match(value) is None:
        _fail('%s is not a legal name' % (label,))
    return value


def _check_digest(value, label):
    if not isinstance(value, str) or DIGEST_RE.match(value) is None:
        _fail('%s is not a lowercase sha256 digest' % (label,))
    return value


# ---------------------------------------------------------------------------
# Safe filesystem inspection (never follows links or Windows reparse points).
# ---------------------------------------------------------------------------

_REPARSE = getattr(stat, 'FILE_ATTRIBUTE_REPARSE_POINT', 0x400)


def _fs_path(path):
    return os.fspath(path)


def _lstat(path, label):
    try:
        return os.lstat(_fs_path(path))
    except FileNotFoundError:
        _fail('%s does not exist' % (label,))
    except OSError as exc:
        _fail('%s cannot be inspected: %s' % (label, exc))


def _is_link(st):
    return stat.S_ISLNK(st.st_mode) or bool(
        getattr(st, 'st_file_attributes', 0) & _REPARSE)


def _dir_stat(path, label):
    st = _lstat(path, label)
    if _is_link(st):
        _fail('%s is a link' % (label,))
    if not stat.S_ISDIR(st.st_mode):
        _fail('%s is not a directory' % (label,))
    return st


def _file_stat(path, label):
    st = _lstat(path, label)
    if _is_link(st):
        _fail('%s is a link' % (label,))
    if not stat.S_ISREG(st.st_mode):
        _fail('%s is not a regular file' % (label,))
    return st


def _require_absent(path, label):
    try:
        os.lstat(_fs_path(path))
    except FileNotFoundError:
        return
    except OSError as exc:
        _fail('%s cannot be inspected: %s' % (label, exc))
    _fail('%s already exists' % (label,))


def _listdir(path, label):
    try:
        return sorted(os.listdir(_fs_path(path)))
    except OSError as exc:
        _fail('%s cannot be listed: %s' % (label, exc))


def _read_bytes(path, label, limit=None):
    st = _file_stat(path, label)
    if limit is not None and st.st_size > limit:
        _fail('%s exceeds %d bytes' % (label, limit))
    try:
        with open(_fs_path(path), 'rb') as handle:
            data = handle.read()
    except OSError as exc:
        _fail('%s cannot be read: %s' % (label, exc))
    if limit is not None and len(data) > limit:
        _fail('%s exceeds %d bytes' % (label, limit))
    return data


def read_file(path, label, limit=None):
    return _read_bytes(path, label, limit)


def read_regular_file(path, label, limit=None):
    return _read_bytes(path, label, limit)


def _walk_root(root):
    """Return {relative posix path: 'dir'|'file'} for every real entry."""
    entries = {}

    def visit(abs_dir, rel_dir):
        for name in _listdir(abs_dir, rel_dir or root):
            abs_path = os.path.join(abs_dir, name)
            rel = name if not rel_dir else rel_dir + '/' + name
            st = _lstat(abs_path, rel)
            if _is_link(st):
                _fail('%s is a link' % (rel,))
            if stat.S_ISDIR(st.st_mode):
                entries[rel] = 'dir'
                visit(abs_path, rel)
            elif stat.S_ISREG(st.st_mode):
                entries[rel] = 'file'
            else:
                _fail('%s is not a regular file or directory' % (rel,))

    visit(_fs_path(root), '')
    return entries


def _uri_quote(text):
    out = []
    for ch in text:
        if ch == '%':
            out.append('%25')
        elif ch == '#':
            out.append('%23')
        elif ch == '?':
            out.append('%3F')
        elif ch in ('/', ':'):
            out.append(ch)
        elif ch <= ' ' or ch == '\\':
            out.append('%' + format(ord(ch), '02X'))
        else:
            out.append(ch)
    return ''.join(out)


def _sqlite_uri(path):
    """Build a read-only immutable SQLite URI for a literal filesystem path."""
    raw = os.fspath(path)
    if not isinstance(raw, str):
        raw = os.fsdecode(raw)
    text = raw.replace('\\', '/')
    encoded = '//' + _uri_quote(text[2:]) if text.startswith('//') else _uri_quote(text)
    return 'file:' + encoded + '?mode=ro&immutable=1'


# ---------------------------------------------------------------------------
# Shape validation helpers.
# ---------------------------------------------------------------------------

_SIDECARS = ('repo.sqlite-wal', 'repo.sqlite-shm', 'repo.sqlite-journal')
_ROOT_FILES = {'repo.sqlite'}
_ROOT_DIRS = {'objects', 'manifests', 'deployments'}


def _exact_keys(obj, keys, label):
    if not isinstance(obj, dict):
        _fail('%s must be an object' % (label,))
    if set(obj) != set(keys):
        _fail('%s must have exactly keys %s' % (label, sorted(keys)))
    return obj


def _check_request_json(text, label):
    obj = _parse_canonical(text.encode('utf-8'), label)
    _exact_keys(obj, ('requirements', 'expected_generation'), label)
    reqs = obj['requirements']
    if not isinstance(reqs, dict):
        _fail('%s.requirements must be an object' % (label,))
    if len(reqs) > MAX_PACKAGES:
        _fail('%s.requirements exceeds %d packages' % (label, MAX_PACKAGES))
    for pkg, bounds in reqs.items():
        _check_name(pkg, '%s.requirements name' % (label,))
        _exact_keys(bounds, ('min', 'max'), '%s.requirements.%s' % (label, pkg))
        low = _check_int(bounds['min'], '%s.requirements.%s.min' % (label, pkg), 1)
        high = _check_int(bounds['max'], '%s.requirements.%s.max' % (label, pkg), 1)
        if low >= high:
            _fail('%s.requirements.%s requires min < max' % (label, pkg))
    expected = obj['expected_generation']
    if expected is not None:
        _check_int(expected, '%s.expected_generation' % (label,), 0)
    return obj


def _sort_rows(rows, keys, label):
    seen = set()
    for row in rows:
        ident = tuple(row[k] for k in keys)
        if ident in seen:
            _fail('duplicate %s identity %r' % (label, ident))
        seen.add(ident)
    return sorted(rows, key=lambda row: tuple(row[k] for k in keys))


def _check_six(row, label):
    _exact_keys(row, ('tenant', 'environment', 'key', 'generation',
                      'manifest_sha256', 'request_json'), label)
    _check_name(row['tenant'], label + '.tenant')
    _check_name(row['environment'], label + '.environment')
    _check_name(row['key'], label + '.key')
    _check_int(row['generation'], label + '.generation', 1)
    _check_digest(row['manifest_sha256'], label + '.manifest_sha256')
    if not isinstance(row['request_json'], str):
        _fail('%s.request_json must be a string' % (label,))
    _check_request_json(row['request_json'], label + '.request_json')
    return row


def _check_five(row, label):
    _exact_keys(row, ('tenant', 'environment', 'key', 'generation', 'manifest_sha256'), label)
    _check_name(row['tenant'], label + '.tenant')
    _check_name(row['environment'], label + '.environment')
    _check_name(row['key'], label + '.key')
    _check_int(row['generation'], label + '.generation', 1)
    _check_digest(row['manifest_sha256'], label + '.manifest_sha256')
    return row


def _five_of(six):
    return {k: six[k] for k in ('tenant', 'environment', 'key', 'generation', 'manifest_sha256')}


def _check_manifest(obj, scope, label):
    _exact_keys(obj, ('tenant', 'environment', 'packages'), label)
    tenant = _check_name(obj['tenant'], label + '.tenant')
    environment = _check_name(obj['environment'], label + '.environment')
    if (tenant, environment) != scope:
        _fail('%s scope does not match the receipts referring to it' % (label,))
    packages = obj['packages']
    if not isinstance(packages, list):
        _fail('%s.packages must be an array' % (label,))
    if len(packages) > MAX_PACKAGES:
        _fail('%s.packages exceeds %d entries' % (label, MAX_PACKAGES))
    names = []
    for index, entry in enumerate(packages):
        item = '%s.packages[%d]' % (label, index)
        _exact_keys(entry, ('name', 'version', 'sha256'), item)
        names.append(_check_name(entry['name'], item + '.name'))
        _check_int(entry['version'], item + '.version', 1)
        _check_digest(entry['sha256'], item + '.sha256')
    if names != sorted(names) or len(set(names)) != len(names):
        _fail('%s.packages names must be unique and ascending' % (label,))
    return obj


def _split_digest_dir(entries, prefix, label):
    """Validate objects/ or manifests/ entries; return set of legal digests."""
    digests = set()
    prefix_dir = prefix + '/'
    for rel in entries:
        if rel == prefix:
            if entries[rel] != 'dir':
                _fail('%s must be a directory' % (label,))
            continue
        if not rel.startswith(prefix_dir):
            continue
        tail = rel[len(prefix_dir):]
        if '/' in tail:
            _fail('%s has an unrecognized nested path %r' % (label, rel))
        if entries[rel] != 'file':
            _fail('%s contains a non-file entry' % (rel,))
        digest = tail[:-5] if (prefix == 'manifests' and tail.endswith('.json')) else tail
        _check_digest(digest, '%s entry %r' % (label, rel))
        digests.add(digest)
    return digests


def _collect_receipts(conn, entries):
    """Validate receipts table and return sorted six-field dict rows."""
    names = {row[0] for row in conn.execute(
        'SELECT name FROM sqlite_master WHERE type=\'table\'')}
    for table in ('receipts', 'current', 'journal', 'outbox'):
        if table not in names:
            _fail('source repository is missing table %r' % (table,))
    version = conn.execute('PRAGMA user_version').fetchone()[0]
    if version != SCHEMA_VERSION:
        _fail('source repository user_version must be %d' % (SCHEMA_VERSION,))
    for table in ('journal', 'outbox'):
        count = conn.execute('SELECT COUNT(*) FROM %s' % (table,)).fetchone()[0]
        if count:
            _fail('source repository has %d row(s) in %s' % (count, table))
    rows = []
    for raw in conn.execute('SELECT tenant, environment, key, generation,'
                            ' manifest_sha256, request_json FROM receipts'):
        rows.append({
            'tenant': raw[0], 'environment': raw[1], 'key': raw[2],
            'generation': raw[3], 'manifest_sha256': raw[4],
            'request_json': raw[5],
        })
    if len(rows) > MAX_RECEIPTS:
        _fail('source repository exceeds %d receipts' % (MAX_RECEIPTS,))
    for index, row in enumerate(rows):
        _check_six(row, 'receipts[%d]' % (index,))
    return _sort_rows(rows, ('tenant', 'environment', 'key'), 'receipt')


def _scope_generation_ok(sixes):
    """Generations per scope must be unique contiguous 1..N."""
    scopes = {}
    for row in sixes:
        scopes.setdefault((row['tenant'], row['environment']), []).append(row['generation'])
    for scope, gens in scopes.items():
        if sorted(gens) != list(range(1, len(gens) + 1)):
            _fail('scope %r generations are not contiguous 1..N' % (scope,))
    return scopes


def _scope_max_generation(sixes):
    tops = {}
    for row in sixes:
        scope = (row['tenant'], row['environment'])
        tops[scope] = max(tops.get(scope, 0), row['generation'])
    return tops
def _collect_current(conn, sixes, tops):
    """Validate current table; return sorted five-field rows."""
    by_key = {(r['tenant'], r['environment'], r['key']): r for r in sixes}
    rows = []
    for raw in conn.execute('SELECT tenant, environment, receipt_json FROM current'):
        label = 'current(%r,%r)' % (raw[0], raw[1])
        scope = (raw[0], raw[1])
        _check_name(raw[0], label + '.tenant')
        _check_name(raw[1], label + '.environment')
        if not isinstance(raw[2], str):
            _fail('%s.receipt_json must be a string' % (label,))
        five = _check_five(_parse_canonical(raw[2].encode('utf-8'), label + '.receipt_json'),
                           label + '.receipt_json')
        if (five['tenant'], five['environment']) != scope:
            _fail('%s scope does not match the row identity' % (label,))
        row = by_key.get((five['tenant'], five['environment'], five['key']))
        if row is None:
            _fail('%s points at an unknown receipt' % (label,))
        if _five_of(row) != five:
            _fail('%s does not equal its historical receipt' % (label,))
        if five['generation'] != tops.get(scope, 0):
            _fail('%s does not name the maximal generation for its scope' % (label,))
        rows.append(five)
    expected = {s for s, g in tops.items() if g} if tops else set()
    scopes = {(r['tenant'], r['environment']) for r in sixes}
    if expected != scopes:
        _fail('every scope with receipts needs exactly one current row')
    return _sort_rows(rows, ('tenant', 'environment'), 'current')


def _collect_deployments(root, entries, current_rows):
    """Validate deployments/ tree against the current rows."""
    expected = {}
    for row in current_rows:
        rel = 'deployments/%s/%s/current.json' % (row['tenant'], row['environment'])
        expected[rel] = _canonical_bytes(row)
    seen = set()
    for rel in entries:
        if rel == 'deployments':
            continue
        if not rel.startswith('deployments/'):
            continue
        if rel in seen:
            _fail('duplicate deployment path %r' % (rel,))
        seen.add(rel)
    for rel in expected:
        if rel not in entries:
            _fail('missing deployment file %r' % (rel,))
        if entries[rel] != 'file':
            _fail('%r must be a regular file' % (rel,))
    for rel in seen:
        if rel not in expected:
            _fail('orphan or unrecognized deployment path %r' % (rel,))
        raw = _read_bytes(os.path.join(_fs_path(root), *rel.split('/')), rel)
        if _parse_canonical(raw, rel) != _parse_canonical(expected[rel], rel):
            _fail('%r does not equal its canonical current receipt' % (rel,))
    return expected


def _collect_manifests(root, entries, sixes):
    """Return (ordered manifest dicts, referenced objects set)."""
    present = _split_digest_dir(entries, 'manifests', 'manifests/')
    scopes = {}
    for row in sixes:
        key = (row['tenant'], row['environment'])
        digest = row['manifest_sha256']
        scopes.setdefault(digest, key)
        if scopes[digest] != key:
            _fail('manifest %s is referenced by more than one scope' % (digest,))
    manifests = []
    objects = set()
    for digest in sorted(scopes):
        rel = 'manifests/%s.json' % (digest,)
        if digest not in present:
            _fail('missing manifest %s referenced by a receipt' % (digest,))
        if entries.get(rel) != 'file':
            _fail('%r must be a regular file' % (rel,))
        raw = _read_bytes(os.path.join(_fs_path(root), 'manifests', digest + '.json'),
                          rel, 2 * 1024 * 1024)
        if hashlib.sha256(raw).hexdigest() != digest:
            _fail('manifest %s contents do not hash to its name' % (digest,))
        obj = _check_manifest(_parse_canonical(raw, rel), scopes[digest], rel)
        manifests.append((digest, raw))
        for entry in obj['packages']:
            objects.add(entry['sha256'])
    return manifests, objects


def _collect_objects(root, entries, objects):
    present = _split_digest_dir(entries, 'objects', 'objects/')
    blobs = []
    for digest in sorted(objects):
        rel = 'objects/%s' % (digest,)
        if digest not in present:
            _fail('missing object %s referenced by a manifest' % (digest,))
        if entries.get(rel) != 'file':
            _fail('%r must be a regular file' % (rel,))
        raw = _read_bytes(os.path.join(_fs_path(root), 'objects', digest), rel, 2 * 1024 * 1024)
        if hashlib.sha256(raw).hexdigest() != digest:
            _fail('object %s contents do not hash to its name' % (digest,))
        blobs.append((digest, raw))
    return blobs


# ---------------------------------------------------------------------------
# Canonical archive encoding helpers.
# ---------------------------------------------------------------------------

_ARC_FIXED = ((1980, 1, 1, 0, 0, 0),)
_EXTERNAL_ATTR = (0o100644 << 16)


def _zip_info(name, size):
    info = zipfile.ZipInfo(name, date_time=ZIP_TIMESTAMP)
    info.compress_type = zipfile.ZIP_STORED
    info.create_system = 3
    info.external_attr = _EXTERNAL_ATTR
    info.internal_attr = 0
    info.flag_bits = 0
    info.extra = b''
    info.comment = b''
    return info


def _check_member_metadata(info):
    if info.is_dir() or info.filename.endswith('/'):
        _fail('archive member %r must not be a directory' % (info.filename,))
    if info.compress_type != zipfile.ZIP_STORED:
        _fail('archive member %r must use ZIP_STORED' % (info.filename,))
    if info.date_time != ZIP_TIMESTAMP:
        _fail('archive member %r has a non-canonical timestamp' % (info.filename,))
    if info.create_system != 3:
        _fail('archive member %r must use create_system=3' % (info.filename,))
    if info.external_attr != _EXTERNAL_ATTR:
        _fail('archive member %r has non-canonical attributes' % (info.filename,))
    if info.flag_bits & 0x1:
        _fail('archive member %r must not be encrypted' % (info.filename,))
    if info.extra:
        _fail('archive member %r must not carry extra fields' % (info.filename,))
    if info.comment:
        _fail('archive member %r must not carry a comment' % (info.filename,))
    if info.file_size > MAX_MEMBER_BYTES:
        _fail('archive member %r exceeds %d bytes' % (info.filename, MAX_MEMBER_BYTES))


def _safe_member_name(name):
    if not name or '\\' in name or name.startswith('/'):
        _fail('archive member name %r is unsafe' % (name,))
    if ':' in name:
        _fail('archive member name %r is unsafe' % (name,))
    parts = name.split('/')
    for part in parts:
        if part in ('', '.', '..'):
            _fail('archive member name %r has a dot segment' % (name,))
    if name in ('snapshot.json',):
        return name
    if name.startswith('manifests/') or name.startswith('objects/'):
        return name
    _fail('archive member name %r is not permitted' % (name,))


def _write_zip(members):
    """members: ordered list of (name, bytes).  Returns canonical ZIP bytes."""
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, 'w', zipfile.ZIP_STORED) as archive:
        for name, data in members:
            info = _zip_info(name, len(data))
            archive.writestr(info, data)
    blob = buffer.getvalue()
    if len(blob) > MAX_ARCHIVE_BYTES:
        _fail('resulting archive exceeds %d bytes' % (MAX_ARCHIVE_BYTES,))
    with zipfile.ZipFile(io.BytesIO(blob), 'r') as check:
        names = check.namelist()
    if names != [name for name, _ in members]:
        _fail('internal error: archive member order is not canonical')
    return blob


def _read_zip(path, label):
    """Return ordered list of (name, bytes); rejects non-canonical archives."""
    st = _file_stat(path, label)
    if st.st_size > MAX_ARCHIVE_BYTES:
        _fail('%s exceeds %d bytes' % (label, MAX_ARCHIVE_BYTES))
    data = _read_bytes(path, label, MAX_ARCHIVE_BYTES)
    try:
        archive = zipfile.ZipFile(io.BytesIO(data), 'r')
    except (zipfile.BadZipFile, OSError) as exc:
        _fail('%s is not a readable ZIP archive: %s' % (label, exc))
    with archive:
        infos = archive.infolist()
        if len(infos) > MAX_MEMBERS:
            _fail('%s exceeds %d members' % (label, MAX_MEMBERS))
        names = []
        for info in infos:
            _check_member_metadata(info)
            _safe_member_name(info.filename)
            names.append(info.filename)
        if names != sorted(names):
            _fail('%s members are not in lexical order' % (label,))
        if len(set(names)) != len(names):
            _fail('%s has duplicate member names' % (label,))
        total = 0
        members = []
        for info in infos:
            data_bytes = archive.read(info.filename)
            if len(data_bytes) != info.file_size:
                _fail('archive member %r size mismatch' % (info.filename,))
            total += len(data_bytes)
            if total > MAX_TOTAL_MEMBER_BYTES:
                _fail('%s exceeds %d total member bytes' % (label, MAX_TOTAL_MEMBER_BYTES))
            members.append((info.filename, data_bytes))
    return members


def _overlaps(a, b):
    a = os.path.abspath(_fs_path(a))
    b = os.path.abspath(_fs_path(b))
    try:
        return os.path.commonpath([a, b]) == a
    except ValueError:
        return False


def _publish_file(target, blob):
    """Atomically publish exact bytes; existing identical bytes are accepted."""
    abs_target = os.path.abspath(_fs_path(target))
    parent = os.path.dirname(abs_target) or '.'
    _dir_stat(parent, 'archive parent directory')
    try:
        os.lstat(abs_target)
    except FileNotFoundError:
        pass
    except OSError as exc:
        _fail('archive cannot be inspected: %s' % (exc,))
    else:
        st = _lstat(abs_target, 'archive')
        if _is_link(st):
            _fail('archive path is a link')
        if not stat.S_ISREG(st.st_mode):
            _fail('archive path is not a regular file')
        if _read_bytes(abs_target, 'archive') == blob:
            return
        _fail('archive already exists with different contents')
    temp = abs_target + '.tmp-%d' % (os.getpid(),)
    _require_absent(temp, 'temporary archive')
    handle = open(temp, 'wb')
    try:
        handle.write(blob)
        handle.flush()
        os.fsync(handle.fileno())
    finally:
        handle.close()
    try:
        os.replace(temp, abs_target)
    except OSError as exc:
        try:
            os.remove(temp)
        except OSError:
            pass
        _fail('cannot publish archive: %s' % (exc,))


# ---------------------------------------------------------------------------
# Snapshot descriptor (snapshot.json) construction and parsing.
# ---------------------------------------------------------------------------

def _build_descriptor(sixes, current_rows):
    return {
        'format': FORMAT,
        'schema_version': SCHEMA_VERSION,
        'receipts': sixes,
        'current': current_rows,
    }


def _summarize(descriptor_bytes, sixes, current_rows, manifest_count, object_count):
    return {
        'snapshot_id': _sha256(descriptor_bytes),
        'receipts': len(sixes),
        'current': len(current_rows),
        'manifests': manifest_count,
        'objects': object_count,
    }


def _parse_descriptor(members):
    """Validate members[0] as snapshot.json; return (bytes, sixes, current)."""
    if not members or members[0][0] != 'snapshot.json':
        _fail('archive must start with snapshot.json')
    raw = members[0][1]
    obj = _parse_canonical(raw, 'snapshot.json')
    _exact_keys(obj, ('format', 'schema_version', 'receipts', 'current'), 'snapshot.json')
    if obj['format'] != FORMAT:
        _fail('snapshot.json format must be %r' % (FORMAT,))
    if obj['schema_version'] != SCHEMA_VERSION:
        _fail('snapshot.json schema_version must be %d' % (SCHEMA_VERSION,))
    receipts = obj['receipts']
    current = obj['current']
    if not isinstance(receipts, list) or not isinstance(current, list):
        _fail('snapshot.json receipts/current must be arrays')
    if len(receipts) > MAX_RECEIPTS:
        _fail('snapshot.json exceeds %d receipts' % (MAX_RECEIPTS,))
    if len(current) > MAX_CURRENT:
        _fail('snapshot.json exceeds %d current scopes' % (MAX_CURRENT,))
    sixes = [dict(_check_six(dict(row), 'receipts[%d]' % (i,)))
             for i, row in enumerate(receipts)]
    currents = [dict(_check_five(dict(row), 'current[%d]' % (i,)))
                for i, row in enumerate(current)]
    sixes = _sort_rows(sixes, ('tenant', 'environment', 'key'), 'receipt')
    currents = _sort_rows(currents, ('tenant', 'environment'), 'current')
    if raw != _canonical_bytes(obj):
        _fail('snapshot.json is not canonical')
    return raw, sixes, currents


# ---------------------------------------------------------------------------
# Source repository loading (read-only, never mutating).
# ---------------------------------------------------------------------------


def _load_source(root):
    """Validate a quiescent source repository; return (sixes, currents, manifests, blobs)."""
    abs_root = os.path.abspath(_fs_path(root))
    _dir_stat(abs_root, 'source root')
    entries = _walk_root(abs_root)
    for sidecar in _SIDECARS:
        if sidecar in entries:
            _fail('source repository contains forbidden SQLite sidecar %r' % (sidecar,))
    for rel, kind in entries.items():
        top = rel.split('/', 1)[0]
        if '/' not in rel:
            if kind == 'file' and rel not in _ROOT_FILES:
                _fail('unrecognized source root file %r' % (rel,))
            if kind == 'dir' and rel not in _ROOT_DIRS and rel != 'journal':
                _fail('unrecognized source root directory %r' % (rel,))
    if 'repo.sqlite' not in entries or entries['repo.sqlite'] != 'file':
        _fail('source repository is missing repo.sqlite')
    if 'journal' in entries:
        if entries['journal'] != 'dir':
            _fail('source journal must be a directory')
        for rel in entries:
            if rel.startswith('journal/'):
                _fail('source disk journal directory is not empty: %r' % (rel,))
    for name in _ROOT_DIRS:
        if name in entries and entries[name] != 'dir':
            _fail('%r must be a directory' % (name,))
    db_path = os.path.join(abs_root, 'repo.sqlite')
    conn = sqlite3.connect(_sqlite_uri(db_path), uri=True)
    try:
        sixes = _collect_receipts(conn, entries)
        tops = _scope_max_generation(sixes)
        _scope_generation_ok(sixes)
        currents = _collect_current(conn, sixes, tops)
    finally:
        conn.close()
    _collect_deployments(abs_root, entries, currents)
    manifests, objects = _collect_manifests(abs_root, entries, sixes)
    blobs = _collect_objects(abs_root, entries, objects)
    _validate_authority(sixes, currents, manifests, blobs)
    return sixes, currents, manifests, blobs


def _archive_members(sixes, currents, manifests, blobs):
    """Build the canonical ordered member list (name, bytes)."""
    descriptor = _build_descriptor(sixes, currents)
    members = [('snapshot.json', _canonical_bytes(descriptor))]
    for digest, raw in manifests:
        members.append(('manifests/%s.json' % (digest,), raw))
    for digest, raw in blobs:
        members.append(('objects/%s' % (digest,), raw))
    members.sort(key=lambda item: item[0])
    if len(members) > MAX_MEMBERS:
        _fail('snapshot exceeds %d members' % (MAX_MEMBERS,))
    total = 0
    for name, data in members:
        if len(data) > MAX_MEMBER_BYTES:
            _fail('member %r exceeds %d bytes' % (name, MAX_MEMBER_BYTES))
        total += len(data)
    if total > MAX_TOTAL_MEMBER_BYTES:
        _fail('snapshot exceeds %d total member bytes' % (MAX_TOTAL_MEMBER_BYTES,))
    return members, descriptor


def export_snapshot(root, archive):
    """Validate a source repository and publish a canonical snapshot archive."""
    abs_archive = os.path.abspath(_fs_path(archive))
    if _overlaps(root, archive):
        _fail('archive path must be outside the source tree')
    sixes, currents, manifests, blobs = _load_source(root)
    members, descriptor = _archive_members(sixes, currents, manifests, blobs)
    blob = _write_zip(members)
    if len(blob) > MAX_ARCHIVE_BYTES:
        _fail('resulting archive exceeds %d bytes' % (MAX_ARCHIVE_BYTES,))
    _publish_file(abs_archive, blob)
    return _summarize(members[0][1], sixes, currents, len(manifests), len(blobs))


def _load_archive(archive):
    """Validate an archive end to end; return (descriptor_bytes, sixes, currents, manifests, blobs)."""
    members = _read_zip(os.path.abspath(_fs_path(archive)), 'archive')
    raw, sixes, currents = _parse_descriptor(members)
    manifests = []
    blobs = []
    for name, data in members[1:]:
        if name.startswith('manifests/'):
            digest = name[len('manifests/'):-len('.json')]
            if not name.endswith('.json') or hashlib.sha256(data).hexdigest() != digest:
                _fail('manifest %s contents do not hash to its name' % (name,))
            manifests.append((digest, _parse_canonical(data, name)))
        else:
            digest = name[len('objects/'):]
            if hashlib.sha256(data).hexdigest() != digest:
                _fail('object %s contents do not hash to its name' % (name,))
            blobs.append((digest, data))
    if [d for d, _ in manifests] != sorted(d for d, _ in manifests):
        _fail('manifest members are not in lexical order')
    if [d for d, _ in blobs] != sorted(d for d, _ in blobs):
        _fail('object members are not in lexical order')
    _validate_authority(sixes, currents, manifests, blobs)
    return raw, sixes, currents, manifests, blobs


def inspect_snapshot(archive):
    """Validate an archive and return its summary without side effects."""
    raw, sixes, currents, manifests, blobs = _load_archive(archive)
    return _summarize(raw, sixes, currents, len(manifests), len(blobs))


def _validate_authority(sixes, currents, manifests, blobs):
    """Full semantic validation of a parsed descriptor plus contents."""
    tops = _scope_max_generation(sixes)
    scopes = _scope_generation_ok(sixes)
    by_key = {(r['tenant'], r['environment'], r['key']): r for r in sixes}
    if len(currents) != len(scopes):
        _fail('every scope with receipts needs exactly one current row')
    for five in currents:
        scope = (five['tenant'], five['environment'])
        row = by_key.get((five['tenant'], five['environment'], five['key']))
        if row is None:
            _fail('current row points at an unknown receipt')
        if _five_of(row) != five:
            _fail('current row does not equal its historical receipt')
        if five['generation'] != tops.get(scope, 0):
            _fail('current row does not name the maximal generation for its scope')
    manifest_scope = {}
    for row in sixes:
        key = (row['tenant'], row['environment'])
        digest = row['manifest_sha256']
        prev = manifest_scope.setdefault(digest, key)
        if prev != key:
            _fail('manifest %s is referenced by more than one scope' % (digest,))
    if {d for d, _ in manifests} != set(manifest_scope):
        _fail('archive manifest set does not match referenced manifests')
    expected_objects = set()
    for digest, obj in manifests:
        if _sha256(_canonical_bytes(obj)) != digest:
            _fail('manifest %s contents do not hash to its name' % (digest,))
        _check_manifest(obj, manifest_scope[digest], 'manifests/%s.json' % (digest,))
        for entry in obj['packages']:
            expected_objects.add(entry['sha256'])
    if {d for d, _ in blobs} != expected_objects:
        _fail('archive object set does not match referenced objects')
    for digest, data in blobs:
        if _sha256(data) != digest:
            _fail('object %s contents do not hash to its name' % (digest,))
    return tops
