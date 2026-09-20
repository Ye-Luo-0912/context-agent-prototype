"""ASSISTED implementation of the frozen V4 snapshot contract (stdlib only)."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import stat
import struct
import sys
import tempfile
import time
import zipfile

FORMAT = 'package-snapshot-v1'
NAME = re.compile(r'[a-z][a-z0-9-]{0,63}\Z')
DIGEST = re.compile(r'[0-9a-f]{64}\Z')
RECEIPT = {'tenant', 'environment', 'key', 'generation', 'manifest_sha256'}
MAX_MEMBER = 2 * 1024 * 1024
MAX_TOTAL = 8 * 1024 * 1024
MAX_ARCHIVE = 9 * 1024 * 1024
SCHEMA = '''
CREATE TABLE receipts(tenant TEXT NOT NULL,environment TEXT NOT NULL,key TEXT NOT NULL,
 generation INTEGER NOT NULL,manifest_sha256 TEXT NOT NULL,request_json TEXT NOT NULL,
 PRIMARY KEY(tenant,environment,key));
CREATE TABLE current(tenant TEXT NOT NULL,environment TEXT NOT NULL,receipt_json TEXT NOT NULL,
 PRIMARY KEY(tenant,environment));
CREATE TABLE journal(tenant TEXT NOT NULL,environment TEXT NOT NULL,key TEXT NOT NULL,
 status TEXT NOT NULL,old_generation INTEGER,old_manifest TEXT,old_receipt TEXT,
 new_generation INTEGER,new_manifest TEXT,new_receipt TEXT NOT NULL,
 PRIMARY KEY(tenant,environment,key));
CREATE TABLE outbox(tenant TEXT NOT NULL,environment TEXT NOT NULL,key TEXT NOT NULL,
 body TEXT NOT NULL,status TEXT NOT NULL,ack TEXT,url TEXT,PRIMARY KEY(tenant,environment,key));
PRAGMA user_version=2;
'''


def require(condition, message):
    if not condition:
        raise ValueError(message)


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(',', ':'), allow_nan=False).encode('utf-8')


def sha(data):
    return hashlib.sha256(data).hexdigest()


def no_duplicates(pairs):
    value = {}
    for key, item in pairs:
        require(key not in value, 'duplicate JSON key')
        value[key] = item
    return value


def parse(data):
    def invalid_constant(_):
        raise ValueError('non-finite JSON value')
    value = json.loads(data.decode('utf-8'), object_pairs_hook=no_duplicates,
                       parse_constant=invalid_constant)
    require(canonical(value) == data, 'JSON is not canonical')
    return value


def name(value):
    require(isinstance(value, str) and NAME.fullmatch(value), 'illegal name')


def digest_name(value):
    require(isinstance(value, str) and DIGEST.fullmatch(value), 'illegal digest')


def integer(value, minimum=1):
    require(type(value) is int and value >= minimum, 'invalid integer')


def shape(value, keys):
    require(isinstance(value, dict) and set(value) == keys, 'invalid object fields')


def path(value):
    try:
        return Path(os.path.abspath(os.fspath(value)))
    except (TypeError, ValueError) as error:
        raise ValueError('invalid path') from error


def check_node(value, directory=False):
    info = value.lstat()
    require(not stat.S_ISLNK(info.st_mode) and not (getattr(info, 'st_file_attributes', 0) & 0x400),
            'links and reparse points are refused')
    require((stat.S_ISDIR if directory else stat.S_ISREG)(info.st_mode), 'wrong filesystem type')


def bounded_read(value, bound=MAX_MEMBER):
    check_node(value)
    with value.open('rb') as stream:
        data = stream.read(bound+1)
    require(len(data) <= bound, 'file exceeds size bound')
    return data


def tree_nodes(root):
    check_node(root, True)
    files, directories = set(), set()
    for base, dirs, names in os.walk(root, followlinks=False):
        for item in dirs:
            node = Path(base)/item
            check_node(node, True)
            directories.add(node.relative_to(root).as_posix())
        for item in names:
            node = Path(base)/item
            check_node(node)
            files.add(node.relative_to(root).as_posix())
    return files, directories


def five(row):
    return {key: row[key] for key in RECEIPT}


def validate_descriptor(descriptor):
    shape(descriptor, {'format', 'schema_version', 'receipts', 'current'})
    require(descriptor['format'] == FORMAT and type(descriptor['schema_version']) is int
            and descriptor['schema_version'] == 2, 'unsupported format')
    receipts, current = descriptor['receipts'], descriptor['current']
    require(isinstance(receipts, list) and len(receipts) <= 128, 'receipt bound')
    require(isinstance(current, list) and len(current) <= 64, 'scope bound')
    by_key, groups = {}, {}
    for row in receipts:
        shape(row, RECEIPT | {'request_json'})
        for key in ('tenant', 'environment', 'key'):
            name(row[key])
        integer(row['generation']); digest_name(row['manifest_sha256'])
        require(isinstance(row['request_json'], str), 'request_json must be a string')
        request = parse(row['request_json'].encode('utf-8'))
        shape(request, {'requirements', 'expected_generation'})
        require(isinstance(request['requirements'], dict), 'invalid requirements')
        for package, bounds in request['requirements'].items():
            name(package); shape(bounds, {'min', 'max'})
            integer(bounds['min']); integer(bounds['max'])
            require(bounds['min'] < bounds['max'], 'invalid version interval')
        if request['expected_generation'] is not None:
            integer(request['expected_generation'], 0)
        key = (row['tenant'], row['environment'], row['key'])
        require(key not in by_key, 'duplicate receipt identity')
        by_key[key] = row
        groups.setdefault(key[:2], []).append(row['generation'])
    require(list(by_key) == sorted(by_key), 'receipts are not sorted')
    for generations in groups.values():
        require(sorted(generations) == list(range(1, len(generations)+1)), 'invalid generation history')
    scopes = []
    for row in current:
        shape(row, RECEIPT)
        for key in ('tenant', 'environment', 'key'):
            name(row[key])
        integer(row['generation']); digest_name(row['manifest_sha256'])
        key = (row['tenant'], row['environment'], row['key'])
        require(key in by_key and five(by_key[key]) == row, 'current has no matching receipt')
        require(row['generation'] == len(groups[key[:2]]), 'current is not maximal generation')
        scopes.append(key[:2])
    require(scopes == sorted(set(groups)), 'current scope set is incomplete or duplicated')
    return receipts, current


def validate_members(members):
    require(1 <= len(members) <= 256 and 'snapshot.json' in members, 'member count/descriptor invalid')
    require(all(len(data) <= MAX_MEMBER for data in members.values()), 'member size bound')
    require(sum(map(len, members.values())) <= MAX_TOTAL, 'total member bound')
    descriptor = parse(members['snapshot.json'])
    receipts, current = validate_descriptor(descriptor)
    manifests, objects = {}, set()
    for row in receipts:
        key = row['manifest_sha256']; scope = (row['tenant'], row['environment'])
        require(manifests.setdefault(key, scope) == scope, 'cross-scope manifest')
    expected = {'snapshot.json'}
    for key, scope in manifests.items():
        member = 'manifests/'+key+'.json'; expected.add(member)
        require(member in members and sha(members[member]) == key, 'missing/corrupt manifest')
        manifest = parse(members[member]); shape(manifest, {'tenant', 'environment', 'packages'})
        require((manifest['tenant'], manifest['environment']) == scope, 'manifest scope mismatch')
        packages = manifest['packages']
        require(isinstance(packages, list) and len(packages) <= 128, 'package bound')
        package_names = []
        for item in packages:
            shape(item, {'name', 'version', 'sha256'})
            name(item['name']); integer(item['version']); digest_name(item['sha256'])
            package_names.append(item['name']); objects.add(item['sha256'])
        require(package_names == sorted(set(package_names)), 'package ordering/identity invalid')
    for key in objects:
        member = 'objects/'+key; expected.add(member)
        require(member in members and sha(members[member]) == key, 'missing/corrupt object')
    require(set(members) == expected, 'extra or missing archive members')
    return dict(snapshot_id=sha(members['snapshot.json']), receipts=len(receipts), current=len(current),
                manifests=len(manifests), objects=len(objects))


def source_members(root, exact=False):
    root = path(root); files, dirs = tree_nodes(root)
    require(not files & {'repo.sqlite-wal', 'repo.sqlite-shm', 'repo.sqlite-journal'}, 'SQLite sidecar present')
    require(not any(item.startswith('journal/') for item in files | dirs), 'pending disk journal')
    for item in files:
        parts = item.split('/')
        if len(parts) == 2 and parts[0] == 'objects':
            digest_name(parts[1])
        elif len(parts) == 2 and parts[0] == 'manifests':
            require(parts[1].endswith('.json'), 'invalid manifest path'); digest_name(parts[1][:-5])
        elif item != 'repo.sqlite' and not item.startswith('deployments/'):
            raise ValueError('unrecognized source file')
    check_node(root/'repo.sqlite')
    connection = sqlite3.connect((root/'repo.sqlite').as_uri()+'?mode=ro&immutable=1', uri=True)
    try:
        require(connection.execute('PRAGMA user_version').fetchone()[0] == 2, 'unsupported source schema')
        for table in ('receipts', 'current', 'journal', 'outbox'):
            require(connection.execute('SELECT type FROM sqlite_master WHERE name=?', (table,)).fetchone() == ('table',), 'missing authority table')
        require(connection.execute('SELECT 1 FROM journal LIMIT 1').fetchone() is None, 'pending journal')
        require(connection.execute('SELECT 1 FROM outbox LIMIT 1').fetchone() is None, 'publication state is outside snapshot scope')
        columns = ('tenant','environment','key','generation','manifest_sha256','request_json')
        receipts = [dict(zip(columns,row)) for row in connection.execute(
            'SELECT tenant,environment,key,generation,manifest_sha256,request_json FROM receipts ORDER BY tenant,environment,key LIMIT 129')]
        current = []
        for tenant, environment, text in connection.execute('SELECT tenant,environment,receipt_json FROM current ORDER BY tenant,environment LIMIT 65'):
            require(isinstance(text, str), 'invalid current JSON')
            row = parse(text.encode('utf-8'))
            require(isinstance(row, dict) and (row.get('tenant'),row.get('environment')) == (tenant,environment), 'SQL current scope mismatch')
            current.append(row)
    except sqlite3.Error as error:
        raise ValueError('source database invalid: '+str(error)) from error
    finally:
        connection.close()
    descriptor = dict(format=FORMAT, schema_version=2, receipts=receipts, current=current)
    validate_descriptor(descriptor)
    expected_dirs = {'objects','manifests','deployments','journal'}
    pointers = set()
    for row in current:
        scope = 'deployments/'+row['tenant']+'/'+row['environment']
        expected_dirs.update({'deployments/'+row['tenant'],scope})
        member = scope+'/current.json'; pointers.add(member)
        require(member in files and bounded_read(root/member) == canonical(row), 'current pointer mismatch')
    require(dirs <= expected_dirs, 'unrecognized directory')
    require({item for item in files if item.startswith('deployments/')} == pointers, 'orphan deployment file')
    members = {'snapshot.json':canonical(descriptor)}
    objects = set()
    for key in sorted({row['manifest_sha256'] for row in receipts}):
        member = 'manifests/'+key+'.json'; data = bounded_read(root/member); members[member] = data
        manifest = parse(data); shape(manifest, {'tenant','environment','packages'})
        require(isinstance(manifest['packages'], list) and len(manifest['packages']) <= 128, 'package bound')
        for item in manifest['packages']:
            shape(item, {'name','version','sha256'}); digest_name(item['sha256']); objects.add(item['sha256'])
    for key in objects:
        members['objects/'+key] = bounded_read(root/'objects'/key)
    validate_members(members)
    if exact:
        require(files == {'repo.sqlite'} | pointers | (set(members)-{'snapshot.json'}), 'existing destination has extra files')
    return members


def read_archive(archive):
    data = bounded_read(path(archive), MAX_ARCHIVE)
    require(len(data) >= 22 and data[-22:-18] == b'PK\x05\x06', 'invalid ZIP end/comment')
    end = struct.unpack('<4s4H2LH', data[-22:])
    require(end[1] == end[2] == end[7] == 0 and end[3] == end[4] and end[4] <= 256,
            'split, ZIP64 or oversized ZIP directory')
    central_offset = end[6]
    require(central_offset + end[5] == len(data)-22, 'ZIP64/trailing directory data refused')
    try:
        with zipfile.ZipFile(io.BytesIO(data)) as z:
            infos = z.infolist()
            require(len(infos) == end[4] and 1 <= len(infos) <= 256 and not z.comment, 'ZIP member count')
            names = [item.filename for item in infos]
            require(names == sorted(set(names)), 'duplicate or unordered ZIP names')
            total, next_header = 0, 0
            for item in infos:
                filename = item.filename
                if filename.startswith('objects/'):
                    digest_name(filename[8:])
                elif filename.startswith('manifests/') and filename.endswith('.json'):
                    digest_name(filename[10:-5])
                else:
                    require(filename == 'snapshot.json', 'unsafe/unrecognized member path')
                require(item.compress_type == zipfile.ZIP_STORED and item.compress_size == item.file_size,
                        'only uncompressed members are supported')
                require(item.file_size <= MAX_MEMBER, 'member size bound')
                total += item.file_size
                require(total <= MAX_TOTAL, 'total member bound')
                require(item.date_time == (1980,1,1,0,0,0) and item.create_system == 3
                        and item.external_attr == 0o100644 << 16, 'invalid ZIP file metadata')
                require(not item.comment and not item.extra and not item.flag_bits & ~0x808,
                        'ZIP comments/extra/encryption/unsupported flags refused')
                require(item.header_offset == next_header and next_header + 30 <= central_offset,
                        'unrecognized local ZIP data')
                header = struct.unpack('<4s5H3L2H', data[next_header:next_header+30])
                require(header[0] == b'PK\x03\x04' and header[2] == item.flag_bits
                        and header[3:6] == (0,0,33) and header[10] == 0, 'invalid local ZIP header')
                filename_bytes = filename.encode('utf-8')
                start = next_header+30
                require(header[9] == len(filename_bytes) and data[start:start+header[9]] == filename_bytes,
                        'local ZIP path mismatch')
                next_header = start+header[9]+item.file_size
                require(next_header <= central_offset, 'local ZIP member overflow')
                if item.flag_bits & 8:
                    require(header[6] in (0,item.CRC) and header[7] in (0,item.file_size)
                            and header[8] in (0,item.file_size), 'invalid descriptor header')
                    if data[next_header:next_header+4] == b'PK\x07\x08':
                        next_header += 4
                    require(next_header+12 <= central_offset, 'missing data descriptor')
                    descriptor = struct.unpack('<3L',data[next_header:next_header+12])
                    require(descriptor == (item.CRC,item.file_size,item.file_size), 'invalid/ZIP64 data descriptor')
                    next_header += 12
                else:
                    require(header[6:9] == (item.CRC,item.file_size,item.file_size), 'local size/CRC mismatch')
            require(next_header == central_offset, 'unrecognized data before central directory')
            members = {item.filename:z.read(item) for item in infos}
    except (zipfile.BadZipFile, struct.error, NotImplementedError) as error:
        raise ValueError('invalid ZIP: '+str(error)) from error
    validate_members(members)
    return members


def archive_bytes(members):
    validate_members(members)
    output = io.BytesIO()
    with zipfile.ZipFile(output, 'w', compression=zipfile.ZIP_STORED, allowZip64=False) as z:
        for filename in sorted(members):
            info = zipfile.ZipInfo(filename, (1980,1,1,0,0,0))
            info.create_system = 3
            info.external_attr = 0o100644 << 16
            info.compress_type = zipfile.ZIP_STORED
            z.writestr(info, members[filename])
    result = output.getvalue()
    require(len(result) <= MAX_ARCHIVE, 'archive byte bound')
    return result


def publish_directory(stage, destination):
    # Windows scanners may briefly hold a just-closed SQLite/staging handle.
    # Retry only a refused rename of this same stage into an absent target;
    # never adopt a destination or repeat any repository/database writes.
    identity = stage.stat()
    for attempt in range(7):
        require(not os.path.lexists(destination), 'destination appeared before publication')
        current = stage.stat()
        require((current.st_dev, current.st_ino) == (identity.st_dev, identity.st_ino),
                'stage identity changed before publication')
        try:
            os.rename(stage, destination)
            return
        except PermissionError as error:
            if getattr(error, 'winerror', None) not in (5, 32, 33) or attempt == 6:
                raise
            time.sleep(.01 * 2**attempt)


def export_snapshot(root, archive):
    root, archive = path(root), path(archive)
    require(not archive.is_relative_to(root), 'archive must be outside source')
    check_node(archive.parent, True)
    members = source_members(root)
    data = archive_bytes(members)
    if os.path.lexists(archive):
        require(bounded_read(archive, MAX_ARCHIVE) == data, 'existing archive conflicts')
        return validate_members(members)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(prefix='.snapshot-', dir=archive.parent, delete=False) as output:
            temporary = Path(output.name)
            output.write(data); output.flush(); os.fsync(output.fileno())
        try:
            # Atomic no-overwrite publication. A concurrent unrelated file is
            # never replaced after a prior absence check.
            os.link(temporary, archive)
        except FileExistsError:
            require(bounded_read(archive, MAX_ARCHIVE) == data, 'concurrent archive conflicts')
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return validate_members(members)


def inspect_snapshot(archive):
    return validate_members(read_archive(archive))


def restore_snapshot(archive, destination, *, crash_at=None):
    require(crash_at in (None, 'before_publish'), 'unsupported crash boundary')
    members = read_archive(archive)
    destination = path(destination)
    check_node(destination.parent, True)
    summary = validate_members(members)
    if os.path.lexists(destination):
        require(source_members(destination, exact=True) == members, 'existing destination conflicts')
        return summary
    stage = Path(tempfile.mkdtemp(prefix='.snapshot-restore-', dir=destination.parent))
    require(stage.parent == destination.parent, 'stage outside destination filesystem')
    try:
        for directory in ('objects','manifests','deployments'):
            (stage/directory).mkdir()
        for filename, data in members.items():
            if filename != 'snapshot.json':
                target = stage/filename
                target.parent.mkdir(parents=True, exist_ok=True)
                with target.open('xb') as output:
                    output.write(data); output.flush(); os.fsync(output.fileno())
        descriptor = parse(members['snapshot.json'])
        db = sqlite3.connect(stage/'repo.sqlite')
        try:
            db.execute('PRAGMA synchronous=FULL')
            db.executescript(SCHEMA)
            columns = ('tenant','environment','key','generation','manifest_sha256','request_json')
            db.executemany('INSERT INTO receipts VALUES(?,?,?,?,?,?)',
                           [tuple(row[k] for k in columns) for row in descriptor['receipts']])
            db.executemany('INSERT INTO current VALUES(?,?,?)',
                           [(row['tenant'],row['environment'],canonical(row).decode('utf-8'))
                            for row in descriptor['current']])
            db.commit()
        finally:
            db.close()
        for row in descriptor['current']:
            target = stage/'deployments'/row['tenant']/row['environment']/'current.json'
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open('xb') as output:
                output.write(canonical(row)); output.flush(); os.fsync(output.fileno())
        require(source_members(stage, exact=True) == members, 'stage validation failed')
        if crash_at == 'before_publish':
            os._exit(73)
        # Parents are trusted and hostile concurrent writers are outside the
        # contract. Re-check absence immediately before directory publication.
        publish_directory(stage, destination)
        stage = None
        return summary
    finally:
        if stage is not None:
            # Only the unique directory created by this call is removable.
            require(stage.parent == destination.parent and stage.name.startswith('.snapshot-restore-'),
                    'refused cleanup outside owned staging directory')
            shutil.rmtree(stage)


def main(argv=None):
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest='operation', required=True)
    export = sub.add_parser('export'); export.add_argument('--root', required=True); export.add_argument('--archive', required=True)
    inspect = sub.add_parser('inspect'); inspect.add_argument('--archive', required=True)
    restore = sub.add_parser('restore'); restore.add_argument('--archive', required=True); restore.add_argument('--destination', required=True)
    restore.add_argument('--crash-at')
    args = parser.parse_args(argv)
    try:
        if args.operation == 'export':
            result = export_snapshot(args.root, args.archive)
        elif args.operation == 'inspect':
            result = inspect_snapshot(args.archive)
        else:
            result = restore_snapshot(args.archive, args.destination, crash_at=args.crash_at)
        print(json.dumps(dict(ok=True, **result), sort_keys=True, separators=(',', ':')))
        return 0
    except (ValueError, OSError, sqlite3.Error) as error:
        print(json.dumps(dict(ok=False, error=str(error)), sort_keys=True, separators=(',', ':')))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
