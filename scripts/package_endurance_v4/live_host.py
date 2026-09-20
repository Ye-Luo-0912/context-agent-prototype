"""Opt-in real-provider host cancellation; the parent runner owns spend and relay.

Importing this module never reads provider configuration or starts a process.
The driver inherits its provider environment and uses only public host RPCs.
"""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import queue
import struct
import sys
import threading
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
from runner_process_tree import WindowsLaunchCleanupError, spawn_windows_owned, stop_windows_owned

MAX_FRAME = 4 * 1024 * 1024
MAX_TRACE = 16 * 1024 * 1024


def encoded(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode('utf-8')


def digest(path):
    path = Path(path)
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None


def save_json(path, value):
    path = Path(path)
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_bytes(encoded(value) + b'\n')
    temporary.replace(path)


def read_frame(stream):
    def read_exact(size):
        parts = []
        while size:
            chunk = stream.read(size)
            if not chunk:
                raise EOFError('public host stream closed mid-frame')
            parts.append(chunk)
            size -= len(chunk)
        return b''.join(parts)
    size = struct.unpack('<I', read_exact(4))[0]
    if not 0 < size <= MAX_FRAME:
        raise ValueError('host frame exceeds controller bound')
    return json.loads(read_exact(size))


def request(operation, payload=None, namespace='work'):
    mid, rid = str(uuid.uuid4()), str(uuid.uuid4())
    return dict(protocol=dict(name='focus-agent.platform', version=dict(major=1, minor=0),
                              active_features=[], schema_digest=hashlib.sha256(
                                  b'focus-agent.platform.work.v1|run-scoped').hexdigest()),
                message_id=mid, request_id=rid, kind='request',
                route=dict(namespace=namespace, operation=operation),
                causality=dict(correlation_id=mid), payload=payload or {})


def exchange(stream, envelope):
    body = encoded(envelope)
    stream.write(struct.pack('<I', len(body)) + body)
    response = read_frame(stream)
    if response.get('request_id') != envelope['request_id'] or response.get('kind') != 'response':
        raise ValueError('RPC response identity mismatch')
    payload = response['payload']
    if payload.get('status') != 'success':
        raise RuntimeError(f'public RPC refused: {payload}')
    return response, payload['value']


class EventLog:
    """Publish complete real envelopes atomically so budget counts never see half-lines."""
    def __init__(self, path):
        self.path = Path(path)
        self.lock = threading.Lock()
        self.rows = []
        self.identities = {}
        self.watermarks = {}
        self.error = None
        self.data = bytearray()
        self.path.write_bytes(b'')

    def set_baseline(self, run_id, watermark):
        # Public work.subscribe starts AFTER its snapshot; it does not replay.
        # Keep this bound separate from the observed event rows.
        with self.lock:
            if run_id in self.watermarks or not isinstance(watermark, int) or watermark < 0:
                raise ValueError('invalid or repeated public subscription baseline')
            self.watermarks[run_id] = watermark

    def append(self, row):
        event = row.get('event', {})
        if event.get('type') in ('model_delta', 'model_retrying'):
            return
        run, seq = row['run_id'], row['seq']
        key = (run, seq)
        wire = encoded(row)
        with self.lock:
            if key in self.identities:
                if self.identities[key] != wire:
                    raise ValueError('conflicting public event identity')
                return
            if seq != self.watermarks.get(run, 0) + 1:
                raise ValueError('public event sequence gap')
            if len(self.data) + len(wire) + 1 > MAX_TRACE:
                raise ValueError('public event capture exceeded bound')
            self.rows.append(row)
            self.identities[key] = wire
            self.watermarks[run] = seq
            self.data.extend(wire + b'\n')
            temporary = self.path.with_suffix('.jsonl.tmp')
            temporary.write_bytes(self.data)
            for attempt in range(6):
                try:
                    temporary.replace(self.path)
                    break
                except PermissionError:
                    if attempt == 5:
                        raise
                    time.sleep(.01)

    def events(self, run_id=None):
        with self.lock:
            if self.error:
                raise RuntimeError(self.error)
            return [r['event'] for r in self.rows if run_id is None or r['run_id'] == run_id]


class HostStartupError(RuntimeError):
    """Retain cleanup facts when construction fails before the caller owns us."""
    def __init__(self, cause, cleanup, log_path):
        super().__init__(f'{type(cause).__name__}: {cause}; host log: {log_path}')
        self.cleanup = cleanup


def record_driver_failure(receipt, error):
    receipt.update(status='FAILED', error=f'{type(error).__name__}: {error}')
    if isinstance(error, HostStartupError):
        receipt['cleanup'].extend(error.cleanup)
        if any(not item.get('tree_confirmed') for item in error.cleanup):
            receipt['status'] = 'CLEANUP_UNCONFIRMED'


class LiveHost:
    def __init__(self, manifest, out, rounds, events):
        if os.name != 'nt':
            raise RuntimeError('this controller supports native Windows hosts only')
        self.out = Path(out)
        self.out.mkdir()
        self.pipe = 'v4-live-' + uuid.uuid4().hex
        self.process = self.stream = self.subscriber = None
        self.thread = None
        self.stopping = False
        self.events = events
        self.cleanup = None
        self.stopped = False
        self.log = (self.out / 'host.log').open('wb')
        argv = [manifest['executables']['host']['path'], '--workdir', manifest['work'],
                '--pipe', self.pipe, '--context-policy', 'dynamic', '--max-rounds', str(rounds),
                '--restore-latest']
        try:
            self.process = spawn_windows_owned(argv, env=dict(os.environ), stdout=self.log, stderr=self.log)
            self.stream = self.connect()
            self.snapshot = self.rpc('snapshot')
            validate_snapshot(self.snapshot, manifest)
            self.run_id = self.snapshot['run_id']
            self.subscriber = self.connect()
            _, subscribed = exchange(self.subscriber, request('subscribe', {
                'replay_after_seq': self.snapshot['watermark']}))
            if subscribed.get('resync_required') or subscribed.get('watermark') != self.snapshot['watermark']:
                raise RuntimeError('public subscription moved past the observed startup snapshot')
            self.startup_audit = read_startup_audit(manifest['work'], self.run_id, subscribed['watermark'])
            save_json(self.out / 'startup-audit.json', self.startup_audit)
            self.events.set_baseline(self.run_id, subscribed['watermark'])
            save_json(self.out / 'subscription.json', dict(
                source='public_snapshot_then_live_subscription', snapshot=self.snapshot,
                subscribe=subscribed, historical_events_replayed=False))
            self.thread = threading.Thread(target=self.receive, daemon=True, name='v4-live-events')
            self.thread.start()
        except BaseException as error:
            cleanup = []
            if isinstance(error, WindowsLaunchCleanupError):
                self.process = error.process
                self.cleanup = error.cleanup
                cleanup.append(error.cleanup)
            try:
                cleanup.append(self.stop())
            except BaseException as stop_error:
                cleanup.append(dict(self.cleanup or {}, tree_confirmed=False,
                                    cleanup_error=str(stop_error)))
            raise HostStartupError(error, cleanup, self.out/'host.log') from error

    def connect(self):
        end = time.monotonic() + 20
        while time.monotonic() < end:
            if self.process.poll() is not None:
                raise RuntimeError('real host exited before pipe connection')
            try:
                return open('\\\\.\\pipe\\' + self.pipe, 'r+b', buffering=0)
            except OSError:
                time.sleep(.05)
        raise TimeoutError('real host pipe did not become available')

    def rpc(self, operation, payload=None, namespace='work', timeout=10):
        envelope = request(operation, payload, namespace)
        result = queue.Queue()
        def invoke():
            try:
                result.put((True, exchange(self.stream, envelope)))
            except BaseException as error:
                result.put((False, error))
        thread = threading.Thread(target=invoke, daemon=True)
        thread.start()
        try:
            ok, value = result.get(timeout=timeout)
        except queue.Empty:
            self.stop()
            raise TimeoutError(f'{namespace}.{operation} exceeded its bound')
        if not ok:
            raise value
        response, value = value
        with (self.out / 'rpc.jsonl').open('ab') as log:
            log.write(encoded(dict(request=envelope, response=response)) + b'\n')
        return value

    def receive(self):
        try:
            while not self.stopping:
                notification = read_frame(self.subscriber)
                if notification.get('kind') != 'notification' or notification.get('route') != dict(namespace='work', operation='event'):
                    raise ValueError('unexpected frame on public event connection')
                row = notification['payload']['envelope']
                if row['run_id'] != self.run_id:
                    raise ValueError('public event belongs to another host run')
                self.events.append(row)
        except BaseException as error:
            if not self.stopping:
                self.events.error = str(error)

    def stop(self):
        if self.stopped:
            if not self.cleanup or not self.cleanup.get('tree_confirmed'):
                raise RuntimeError('prior host cleanup remains unconfirmed')
            return self.cleanup
        self.stopped = True
        self.stopping = True
        try:
            if self.process is not None:
                self.cleanup = stop_windows_owned(self.process, 2, 10)
            elif self.cleanup is None:
                self.cleanup = dict(tree_confirmed=True, scope='no_process_started')
        finally:
            for stream in (self.stream, self.subscriber):
                if stream is not None:
                    try:
                        stream.close()
                    except OSError:
                        pass
            if self.thread:
                self.thread.join(2)
            self.log.close()
            save_json(self.out / 'cleanup.json', self.cleanup or dict(tree_confirmed=False))
        if not self.cleanup.get('tree_confirmed') or (self.thread and self.thread.is_alive()):
            raise RuntimeError('host process/event cleanup was not confirmed')
        return self.cleanup


def validate_snapshot(snapshot, manifest):
    if (snapshot.get('focus') or {}).get('task_id') != manifest['task_id']:
        raise ValueError('host did not restore the expected existing task')
    if Path(snapshot['workspace_root']).resolve() != Path(manifest['work']).resolve():
        raise ValueError('host workspace identity mismatch')
    if snapshot.get('resync_required') or snapshot.get('restore_evidence_degraded'):
        raise ValueError('host recovery/event evidence is incomplete')
    readiness = snapshot.get('continue_readiness') or {}
    reason = readiness.get('reason')
    if reason not in ('ready', 'no_active_task', 'turn_running', 'recovery_required',
                       'cleanup_in_flight', 'no_retained_directive', 'directive_may_be_truncated'):
        raise ValueError('missing or unknown typed continue-readiness reason')
    if readiness.get('can_continue') != (reason == 'ready'):
        raise ValueError('continue-readiness boolean contradicts its typed reason')
    if reason == 'recovery_required':
        raise ValueError('host is recovery-fenced')


def read_startup_audit(work, run_id, watermark):
    """Read the actual durable startup prefix; never invent public replay rows."""
    run_id = str(uuid.UUID(run_id))
    path = Path(work) / '.focus-agent/traces' / (run_id + '.jsonl')
    with path.open('rb') as stream:
        raw = stream.read(MAX_FRAME + 1)
    if len(raw) > MAX_FRAME:
        raise ValueError('startup journal exceeds the bounded inspection window')
    rows = []
    for line in raw.splitlines():
        if len(rows) == watermark:
            break
        row = json.loads(line)
        if row.get('run_id') != run_id or row.get('seq') != len(rows) + 1:
            raise ValueError('startup journal prefix identity/sequence mismatch')
        if row.get('event', {}).get('type') in ('model_started', 'model_used', 'tool_started', 'context_compacted'):
            raise ValueError('unobserved model/tool activity preceded the public subscription')
        rows.append(row)
    if len(rows) != watermark:
        raise ValueError('startup journal does not reach the public snapshot watermark')
    return dict(source='read_only_runtime_journal_startup_prefix', run_id=run_id,
                watermark=watermark, journal_path=str(path), rows=rows)


def allowed_call(call, manifest, phase):
    """Only these exact effect arguments may receive an operator approval."""
    arguments = call.get('arguments', {})
    if phase == 'cancel' and call.get('name') == 'fs.write':
        return arguments == dict(path=manifest['prefix'], content=manifest['prefix_content'])
    if phase == 'cancel' and call.get('name') == 'process.run':
        return arguments == dict(argv=manifest['argv'], timeout_ms=90000)
    return False


def settled_ledger(ledger, segment):
    rows = [a for a in ledger.get('attempts', []) if a.get('segment') == segment]
    return (ledger.get('api_protocol') == 'chat' and not ledger.get('cap_stopped')
            and ledger.get('reserved_usd') == 0 and ledger.get('unknown_usd') == 0
            and any(a.get('status') == 'committed' for a in rows)
            and all(a.get('status') in ('committed', 'upstream_rate_limited') for a in rows))


def read_ledger(manifest):
    raw = Path(manifest['ledger']).read_bytes()
    if len(raw) > 8 * 1024 * 1024:
        raise ValueError('campaign ledger exceeded controller read bound')
    return json.loads(raw)


def await_ledger(manifest):
    end = min(time.time() + 5, manifest['driver_deadline_epoch'])
    while time.time() < end:
        value = read_ledger(manifest)
        if settled_ledger(value, manifest['segment']):
            return value
        if value.get('unknown_usd', 0) > 0 or value.get('cap_stopped'):
            break
        time.sleep(.02)
    raise RuntimeError('provider usage did not reach a complete settled ledger')


def current_operation(events):
    accepted = [e['snapshot']['identity'] for e in events if e.get('type') == 'operation_accepted']
    return accepted[-1] if accepted else None


def observed_turn_running(snapshot, manifest):
    return ((snapshot.get('focus') or {}).get('task_id') == manifest['task_id']
            and snapshot.get('continue_readiness') == dict(reason='turn_running', can_continue=False)
            and not snapshot.get('resync_required'))


def cancel_gate(events, ledger, manifest, marker, process_live):
    operation = current_operation(events)
    started = [i for i, event in enumerate(events) if event.get('type') == 'tool_started']
    if not operation or not started or not process_live:
        return False
    index = started[-1]
    call = events[index]['call']
    if not (call.get('name') == 'process.run' and allowed_call(call, manifest, 'cancel')
            and operation.get('task_id') == manifest['task_id']
            and operation.get('tool_name') == 'process.run'
            and operation.get('call_id') == call.get('id')):
        return False
    if any(e.get('type') in ('tool_finished', 'model_started', 'turn_completed', 'turn_failed',
                              'turn_cancelled', 'recovery_required') for e in events[index + 1:]):
        return False
    prefix_calls = {e['call'].get('id') for e in events[:index] if e.get('type') == 'tool_started'
                    and e['call'].get('name') == 'fs.write' and allowed_call(e['call'], manifest, 'cancel')}
    if not any(e.get('type') == 'tool_finished' and e.get('output', {}).get('call_id') in prefix_calls
               and e['output'].get('tool_name') == 'fs.write' and e['output'].get('ok') is True
               for e in events[:index]):
        return False
    model_starts = [i for i, e in enumerate(events[:index]) if e.get('type') == 'model_started']
    if not model_starts:
        return False
    used = [e for e in events[model_starts[-1]:index] if e.get('type') == 'model_used']
    known = any(e.get('usage_identity') == 'observed' and e.get('role', 'main') == 'main'
                and isinstance(e.get('usage'), dict)
                and e['usage'].get('input_tokens') is not None
                and e['usage'].get('output_tokens') is not None for e in used)
    return (known and marker.get('nonce') == manifest['nonce']
            and isinstance(marker.get('pid'), int) and marker['pid'] > 0
            and settled_ledger(ledger, manifest['segment']))


def verify_frozen(manifest):
    work = Path(manifest['work'])
    for name, expected in manifest['frozen'].items():
        if digest(work / name) != expected:
            raise ValueError(f'frozen source changed: {name}')


def prepare_probe_dir(manifest):
    """Prepare infrastructure only; the model must still write every probe file."""
    work = Path(manifest['work']).resolve()
    relative = Path(manifest['probe_dir'])
    target = (work / relative).resolve()
    if relative.is_absolute() or not target.is_relative_to(work / 'app' / 'tests'):
        raise ValueError('probe directory escapes its authorized app/tests scope')
    if not target.name.startswith('.runtime-probe-'):
        raise ValueError('probe directory lacks the dedicated diagnostic prefix')
    target.mkdir(parents=True, exist_ok=False)
    if any(target.iterdir()):
        raise ValueError('newly prepared probe directory is not empty')
    return dict(source='controller_prepared_empty_directory', path=manifest['probe_dir'],
                exclusive_create=True, initial_entries=[], probe_files_written_by_controller=[])


def round_allocation(first_rounds, restored_rounds, remaining=None):
    if any(isinstance(n, bool) or not isinstance(n, int) or n < 2 for n in (first_rounds, restored_rounds)):
        raise ValueError('each host needs an integer budget of at least two rounds (tool plus final)')
    total = first_rounds + restored_rounds
    if remaining is not None and total > remaining:
        raise ValueError('requested host rounds exceed the actual remaining campaign budget')
    return total


def verify_manifest(path, expected_digest):
    if digest(path) != expected_digest:
        raise ValueError('driver manifest digest mismatch')
    manifest = json.loads(Path(path).read_bytes())
    if manifest.get('schema') != 'v4-live-host-manifest-v1':
        raise ValueError('unsupported driver manifest')
    if manifest.get('mode') == 'recovery_only':
        if manifest['first_host_rounds'] != 0 or manifest['restored_host_rounds'] != 2:
            raise ValueError('recovery-only must allocate exactly two rounds to one host')
    else:
        round_allocation(manifest['first_host_rounds'], manifest['restored_host_rounds'])
    for item in manifest['executables'].values():
        if digest(item['path']) != item['sha256'] or item['sha256'] is None:
            raise ValueError('actual executable differs from pinned manifest')
    if Path(manifest['executables']['driver']['path']).resolve() != Path(__file__).resolve():
        raise ValueError('manifest names a different driver')
    if Path(manifest['executables']['python']['path']).resolve() != Path(sys.executable).resolve():
        raise ValueError('driver interpreter differs from pinned manifest')
    verify_frozen(manifest)
    if manifest.get('mode') == 'recovery_only':
        for source in (manifest['source_manifest'], manifest['source_receipt']):
            if digest(source['path']) != source['sha256']:
                raise ValueError('the prior failed experiment evidence changed')
        verify_existing_prefix(manifest)
    return manifest


def verify_checkpoint(work, receipt, task_id, prefix=None):
    artifact = receipt['artifact']
    if Path(artifact).name != artifact or '/' in artifact or '\\' in artifact:
        raise ValueError('checkpoint artifact must be one store filename')
    path = Path(work) / '.focus-agent/checkpoints' / artifact
    if path.stat().st_size > 32 * 1024 * 1024:
        raise ValueError('checkpoint exceeds bounded probe inspection')
    header, separator, payload = path.read_bytes().partition(b'\n')
    envelope = json.loads(header)
    if (not separator or envelope.get('format') != 'runtime-checkpoint-envelope-v1'
            or len(payload) != envelope.get('payload_bytes')
            or hashlib.sha256(payload).hexdigest() != envelope.get('checksum')):
        raise ValueError('checkpoint integrity verification failed')
    checkpoint = json.loads(payload)
    if checkpoint.get('current_task_id') != task_id:
        raise ValueError('checkpoint changed the current task')
    tasks = checkpoint.get('tasks', {}).get('tasks', [])
    task = next((t for t in tasks if t.get('id') == task_id), None)
    if not task or not task.get('current_directive'):
        raise ValueError('checkpoint lost the retained task directive')
    if prefix is not None and prefix not in json.dumps(checkpoint.get('context'), ensure_ascii=False):
        raise ValueError('checkpoint lost the accepted prefix observation')
    return dict(artifact=artifact, checksum=envelope['checksum'], bytes=len(payload),
                task_id=task_id, directive=task['current_directive'],
                run_id=checkpoint.get('run_metadata', {}).get('run_id'))


class ObservedProcess:
    def __init__(self, pid):
        self.kernel = ctypes.WinDLL('kernel32', use_last_error=True)
        self.kernel.OpenProcess.argtypes = [ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32]
        self.kernel.OpenProcess.restype = ctypes.c_void_p
        self.kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
        self.kernel.WaitForSingleObject.restype = ctypes.c_uint32
        self.kernel.CloseHandle.argtypes = [ctypes.c_void_p]
        self.handle = self.kernel.OpenProcess(0x00100000 | 0x1000, False, pid)
        if not self.handle:
            raise OSError('cannot retain the observed probe process identity')
        self.pid = pid

    def live(self):
        result = self.kernel.WaitForSingleObject(self.handle, 0)
        if result not in (0, 258):
            raise OSError('probe process exit observation failed')
        return result == 258

    def wait_exit(self):
        return self.kernel.WaitForSingleObject(self.handle, 10000) == 0

    def close(self):
        if self.handle:
            self.kernel.CloseHandle(self.handle)
            self.handle = None


def synchronized_events(host):
    snapshot = host.rpc('snapshot')
    end = time.monotonic() + 5
    while host.events.watermarks.get(host.run_id, 0) < snapshot['watermark']:
        host.events.events()
        if time.monotonic() >= end:
            raise TimeoutError('public event capture did not reach snapshot watermark')
        time.sleep(.02)
    return snapshot, host.events.events(host.run_id)


def handle_approvals(host, snapshot, events, manifest, phase):
    pending = snapshot.get('pending_approvals', [])
    calls = [e['call'] for e in events if e.get('type') == 'tool_started']
    operation = current_operation(events)
    for approval in pending:
        call = calls[-1] if calls else {}
        if (len(pending) == 1 and operation and operation.get('call_id') == call.get('id')
                and approval['call_name'] == call.get('name') and allowed_call(call, manifest, phase)):
            answer = host.rpc('respond', dict(request_id=approval['request_id'], decision='Allow'), 'approval')
            if answer.get('outcome') != 'delivered':
                raise RuntimeError('exact probe approval was not delivered')
        else:
            # Keep the gate parked until cancellation owns the turn, so denial
            # cannot race an unwanted next paid model request.
            host.rpc('cancel', dict(expected_task_id=manifest['task_id'],
                                   **({'expected_turn_id': operation['turn_id']} if operation else {})))
            host.rpc('respond', dict(request_id=approval['request_id'], decision='Deny'), 'approval')
            raise RuntimeError('refused an unrecognized effect approval')


def phase_check(host, manifest, phase):
    if time.time() >= manifest['driver_deadline_epoch']:
        raise TimeoutError('original bounded driver deadline expired')
    snapshot, events = synchronized_events(host)
    validate_snapshot(snapshot, manifest)
    if any(e.get('type') in ('turn_failed', 'recovery_required', 'turn_commit_failed') for e in events):
        raise RuntimeError('real runtime reported a failure/recovery terminal')
    if any(e.get('type') == 'model_used' and e.get('usage_identity') == 'unknown' for e in events):
        raise RuntimeError('real runtime reported unknown model usage')
    handle_approvals(host, snapshot, events, manifest, phase)
    verify_frozen(manifest)
    return snapshot, events


def run_driver(manifest_path, expected_digest):
    from urllib.parse import urlparse
    manifest = verify_manifest(manifest_path, expected_digest)
    if (os.environ.get('OPENAI_API_PROTOCOL') != 'chat'
            or os.environ.get('OPENAI_CHAT_THINKING') != 'disabled'
            or os.environ.get('MAINTENANCE_MAX_CALLS_PER_MAINTAIN') != '0'
            or urlparse(os.environ.get('OPENAI_BASE_URL', '')).hostname not in ('127.0.0.1', 'localhost')):
        raise ValueError('driver requires the parent-owned Chat relay environment')
    prompt = sys.stdin.read(65537)
    if not prompt or len(prompt.encode('utf-8')) > 65536 or prompt != manifest['prompt']:
        raise ValueError('driver input differs from the pinned operator directive')
    work, out = Path(manifest['work']), Path(manifest['out'])
    if manifest.get('mode') == 'recovery_only':
        return run_recovery_driver(manifest, expected_digest)
    events = EventLog(out / 'events.jsonl')
    receipt = dict(schema='v4-live-host-receipt-v1', source='real_provider_real_host_real_tools',
                   status='RUNNING', manifest_sha256=expected_digest, task_id=manifest['task_id'],
                   executables=manifest['executables'], cleanup=[])
    host = held = None
    try:
        receipt['probe_preparation'] = prepare_probe_dir(manifest)
        host = LiveHost(manifest, out / 'host-1', manifest['first_host_rounds'], events)
        first_run = host.run_id
        receipt['first_snapshot'] = host.snapshot
        receipt['first_startup_audit'] = host.startup_audit
        host.rpc('steer', dict(instruction=prompt, expected_task_id=manifest['task_id']))
        while True:
            snapshot, trace = phase_check(host, manifest, 'cancel')
            live = work / manifest['live']
            if live.exists():
                if live.stat().st_size > 4096:
                    raise ValueError('probe marker exceeded bound')
                marker = json.loads(live.read_bytes())
                if marker.get('nonce') != manifest['nonce']:
                    raise ValueError('probe nonce mismatch')
                if held is None:
                    held = ObservedProcess(marker['pid'])
                ledger = read_ledger(manifest)
                if cancel_gate(trace, ledger, manifest, marker, held.live()) and observed_turn_running(snapshot, manifest):
                    if (work / manifest['prefix']).read_text(encoding='utf-8') != manifest['prefix_content']:
                        raise ValueError('the accepted prefix bytes differ')
                    operation = current_operation(trace)
                    if not held.live() or not settled_ledger(read_ledger(manifest), manifest['segment']):
                        raise RuntimeError('the cancellation boundary moved before admission')
                    receipt['cancel_gate'] = dict(marker=marker, operation_admission=operation,
                                                 public_snapshot=snapshot,
                                                 evidence_scope='published_admission_and_observed_running_tool',
                                                 ledger_attempts=[a for a in ledger['attempts'] if a['segment'] == manifest['segment']])
                    break
            if any(e.get('type') == 'turn_completed' for e in trace):
                raise RuntimeError('BOUNDARY_NOT_REACHED: real model ended before starting the probe')
            time.sleep(.05)
        started = time.monotonic()
        cancelled = host.rpc('cancel', dict(expected_task_id=manifest['task_id'], expected_turn_id=operation['turn_id']))
        ack = cancelled.get('ack', {})
        if ack.get('status') != 'cancelled' or ack.get('turn_id') != operation['turn_id']:
            raise ValueError('public cancellation did not acknowledge the observed turn')
        receipt['cancel'] = dict(response=cancelled, latency_s=time.monotonic() - started,
                                 pid=held.pid, exited_at_ack=not held.live())
        if not held.wait_exit():
            raise RuntimeError('probe process exit was not confirmed after cancellation')
        held.close()
        held = None
        if (work / manifest['late']).exists():
            raise ValueError('cancelled probe wrote its late marker')
        while True:
            snapshot, trace = phase_check(host, manifest, 'settle')
            if (snapshot.get('continue_readiness') or {}).get('can_continue'):
                break
            time.sleep(.05)
        terminals = [e for e in trace if e.get('type') == 'turn_cancelled' and e.get('turn_id') == operation['turn_id']]
        if len(terminals) != 1 or not settled_ledger(read_ledger(manifest), manifest['segment']):
            raise ValueError('cancellation terminal or provider settlement is incomplete')
        captured = host.rpc('checkpoint')
        receipt['checkpoint'] = verify_checkpoint(work, captured, manifest['task_id'], manifest['prefix'])
        if receipt['checkpoint']['run_id'] != first_run:
            raise ValueError('checkpoint run identity differs from the cancelled host')
        synchronized_events(host)
        receipt['cleanup'].append(host.stop())
        host = None
        host = LiveHost(manifest, out / 'host-2', manifest['restored_host_rounds'], events)
        receipt['restored_snapshot'] = host.snapshot
        receipt['restored_startup_audit'] = host.startup_audit
        if host.run_id == first_run:
            raise ValueError('cold host must have a new run identity')
        synchronized_events(host)
        restored = [row['event'] for row in host.startup_audit['rows']]
        if not any(e.get('type') == 'runtime_restored' and e.get('restored_run_id') == first_run
                   and e.get('current_run_id') == host.run_id for e in restored):
            raise ValueError('cold restore did not link the saved host run')
        correction = (f'The operator cancelled the previous probe and independently confirmed its process exited. '
                      f'Preserve all earlier product requirements. This diagnostic step is now read-only: use fs.read '
                      f'to read {manifest["prefix"]}, then report its exact marker. Do not rewrite any file, '
                      f'relaunch any process, run tests or mark the persistent task complete.')
        host.rpc('steer', dict(instruction=correction, expected_task_id=manifest['task_id']))
        while True:
            snapshot, restored = phase_check(host, manifest, 'recover')
            if any(e.get('type') == 'turn_completed' for e in restored):
                break
            time.sleep(.05)
        if not any(e.get('type') == 'tool_started' and e['call'].get('name') == 'fs.read'
                   and e['call'].get('arguments', {}).get('path') == manifest['prefix'] for e in restored):
            raise ValueError('real model did not read the restored prefix')
        if not any(e.get('type') == 'assistant_message' and manifest['nonce'] in e.get('content', '') for e in restored):
            raise ValueError('real model did not report the restored marker')
        all_calls = [e['call'] for e in events.events() if e.get('type') == 'tool_started']
        if sum(c.get('name') == 'process.run' for c in all_calls) != 1 or sum(c.get('name') == 'fs.write' for c in all_calls) != 1:
            raise ValueError('the probe effects were repeated or unexpected')
        await_ledger(manifest)
        verify_frozen(manifest)
        receipt['final_checkpoint'] = host.rpc('checkpoint')
        synchronized_events(host)
        receipt['cleanup'].append(host.stop())
        host = None
        receipt['status'] = 'PASS'
        receipt['product_acceptance'] = False
    except BaseException as error:
        record_driver_failure(receipt, error)
    finally:
        if host is not None:
            try:
                receipt['cleanup'].append(host.stop())
            except BaseException as error:
                receipt.update(status='CLEANUP_UNCONFIRMED', cleanup_error=str(error))
        if held is not None:
            try:
                if held.live():
                    receipt.update(status='CLEANUP_UNCONFIRMED', probe_still_live=True)
            finally:
                held.close()
        save_json(out / 'live-host-receipt.json', receipt)
    return 0 if receipt['status'] == 'PASS' else 1


def verify_existing_prefix(manifest):
    path = (Path(manifest['work']) / manifest['prefix']).resolve()
    scope = (Path(manifest['work']) / 'app/tests').resolve()
    if not path.is_relative_to(scope) or digest(path) != manifest['existing_prefix_sha256']:
        raise ValueError('existing probe prefix is missing, changed, or outside its authorized scope')
    if path.read_text(encoding='utf-8') != manifest['prefix_content']:
        raise ValueError('existing probe prefix bytes differ from the prior model write')


def successful_prefix_read(events, manifest):
    calls = {e['call']['id'] for e in events if e.get('type') == 'tool_started'
             and e['call'].get('name') == 'fs.read'
             and e['call'].get('arguments', {}).get('path') == manifest['prefix']}
    return any(e.get('type') == 'tool_finished' and e.get('output', {}).get('call_id') in calls
               and e['output'].get('ok') is True
               and manifest['nonce'] in e['output'].get('model_content', '') for e in events)


def run_recovery_driver(manifest, expected_digest):
    """Cold read after a forced exit; this never claims a successful cancel."""
    out = Path(manifest['out'])
    events = EventLog(out / 'events.jsonl')
    receipt = dict(schema='v4-live-host-receipt-v1', mode='recovery_only',
                   source='real_cold_read_after_forced_host_exit', status='RUNNING',
                   typed_cancel_verified=False, cancel_restore_pass=False, product_acceptance=False,
                   task_id=manifest['task_id'], manifest_sha256=expected_digest,
                   source_manifest=manifest['source_manifest'], source_receipt=manifest['source_receipt'],
                   forced_host_run_id=manifest['forced_host_run_id'],
                   expected_checkpoint_run_id=manifest['expected_checkpoint_run_id'],
                   restore_basis='existing task checkpoint, not a cancelled-turn checkpoint',
                   executables=manifest['executables'], cleanup=[])
    host = None
    try:
        verify_existing_prefix(manifest)
        host = LiveHost(manifest, out / 'host-recovery', 2, events)
        receipt.update(snapshot=host.snapshot, startup_audit=host.startup_audit)
        if host.run_id in (manifest['forced_host_run_id'], manifest['expected_checkpoint_run_id']):
            raise ValueError('recovery must use a new host run')
        restored = [row['event'] for row in host.startup_audit['rows']]
        if not any(e.get('type') == 'runtime_restored'
                   and e.get('restored_run_id') == manifest['expected_checkpoint_run_id']
                   and e.get('current_run_id') == host.run_id for e in restored):
            raise ValueError('recovery host did not restore the recorded existing task checkpoint run')
        host.rpc('steer', dict(instruction=manifest['prompt'], expected_task_id=manifest['task_id']))
        while True:
            _, trace = phase_check(host, manifest, 'recover')
            verify_existing_prefix(manifest)
            if any(e.get('type') == 'turn_completed' for e in trace):
                break
            time.sleep(.05)
        calls = [e['call'] for e in trace if e.get('type') == 'tool_started']
        if any(c.get('name') in ('fs.write', 'fs.mkdir', 'edit.patch', 'process.run', 'shell.exec') for c in calls):
            raise ValueError('recovery-only model attempted a write or process instead of a cold read')
        if not successful_prefix_read(trace, manifest):
            raise ValueError('the real model did not successfully read the existing prefix')
        if not any(e.get('type') == 'assistant_message' and manifest['nonce'] in e.get('content', '') for e in trace):
            raise ValueError('the real model did not report the existing prefix marker')
        await_ledger(manifest)
        verify_frozen(manifest)
        verify_existing_prefix(manifest)
        receipt['checkpoint'] = verify_checkpoint(manifest['work'], host.rpc('checkpoint'), manifest['task_id'])
        synchronized_events(host)
        receipt['cleanup'].append(host.stop())
        host = None
        receipt.update(status='PASS', verified='cold_read_after_forced_host_exit_only',
                       existing_prefix_sha256=manifest['existing_prefix_sha256'])
    except BaseException as error:
        record_driver_failure(receipt, error)
    finally:
        if host is not None:
            try:
                receipt['cleanup'].append(host.stop())
            except BaseException as error:
                receipt.update(status='CLEANUP_UNCONFIRMED', cleanup_error=str(error))
        save_json(out / 'live-host-receipt.json', receipt)
    return 0 if receipt['status'] == 'PASS' else 1


def campaign_module():
    spec = importlib.util.spec_from_file_location('v4_live_campaign', Path(__file__).with_name('run.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def parent_protected_files(stage):
    # The runner checks against the original manifest, so newly generated
    # candidate files cannot be added here. Their independent frozen identity
    # is enforced by verify_frozen, including an originally absent file.
    return tuple(sorted(json.loads((Path(stage) / 'baseline-lock.json').read_bytes())['files']))


def make_manifest(stage, segment, task_id, caps, first_rounds=6, restored_rounds=4):
    round_allocation(first_rounds, restored_rounds)
    stage = Path(stage).resolve()
    work = stage / 'workspace'
    if uuid.UUID(task_id).int == 0:
        raise ValueError('expected task identity must be non-nil')
    nonce = uuid.uuid4().hex
    folder = 'app/tests/.runtime-probe-' + nonce
    prefix, live, late = (folder + '/' + name for name in ('prefix.txt', 'live.json', 'late.txt'))
    content = 'confirmed-prefix-' + nonce + '\n'
    code = ("import json,os,pathlib,time;"
            f"pathlib.Path({live!r}).write_text(json.dumps({{'nonce':{nonce!r},'pid':os.getpid()}}),encoding='utf-8');"
            f"time.sleep(60);pathlib.Path({late!r}).write_text('late',encoding='utf-8')")
    argv = [str(Path(sys.executable).resolve()), '-c', code]
    first_write = dict(path=prefix, content=content)
    launch = dict(argv=argv, timeout_ms=90000)
    prompt = ("The operator authorizes one diagnostic cancellation probe in this existing task. Preserve the full "
              "product requirements. Freeze app/snapshot.py and app/tests/test_snapshot.py, including absence: "
              "do not create or edit either file. Do not run application tests, inspect Runtime private state, "
              "or access controller files or network. This diagnostic instruction additionally authorizes only "
              f"the fresh directory {folder}. The controller has exclusively prepared that directory empty; "
              "do not call fs.mkdir or create any probe file except the specified prefix write. "
              "First discover/load process.run if needed. Then call fs.write "
              f"with EXACT arguments {json.dumps(first_write, ensure_ascii=False)}. Next call process.run with "
              f"EXACT arguments {json.dumps(launch, ensure_ascii=False)}. Do not add cwd, env or other fields. "
              "The operator will cancel while this child sleeps; do not launch another process or complete the "
              "persistent task. The controller must observe the real model's tool execution; you must not "
              "substitute a written marker or a textual completion for the process call.")
    executables = {name: dict(path=str(path.resolve()), sha256=digest(path)) for name, path in {
        'driver': Path(__file__), 'host': ROOT / 'target/debug/agent-host.exe',
        'python': Path(sys.executable)}.items()}
    if any(item['sha256'] is None for item in executables.values()):
        raise ValueError('a pinned live-controller executable is missing')
    return dict(schema='v4-live-host-manifest-v1', segment=segment, work=str(work),
                out=str(stage / segment), ledger=str(stage / 'budget-ledger.json'),
                task_id=task_id, nonce=nonce, probe_dir=folder, prefix=prefix, live=live, late=late,
                prefix_content=content, argv=argv, prompt=prompt, executables=executables,
                frozen={name: digest(work / name) for name in ('app/snapshot.py', 'app/tests/test_snapshot.py')},
                first_host_rounds=first_rounds, restored_host_rounds=restored_rounds,
                driver_deadline_epoch=min(time.time() + 840, caps['deadline_epoch'] - 55),
                campaign_binary_role='existing TUI baseline; actual driver/host/interpreter pinned separately here')


def make_recovery_manifest(stage, segment, source_path, caps, expected_task_id=None):
    stage, source_path = Path(stage).resolve(), Path(source_path).resolve()
    if not source_path.is_relative_to(stage):
        raise ValueError('prior manifest must belong to this existing campaign stage')
    old = json.loads(source_path.read_bytes())
    if old.get('schema') != 'v4-live-host-manifest-v1' or old.get('mode') == 'recovery_only':
        raise ValueError('recovery-only requires a prior probe manifest')
    if Path(old['work']).resolve() != stage / 'workspace':
        raise ValueError('prior probe belongs to another workspace')
    if expected_task_id is not None and old['task_id'] != expected_task_id:
        raise ValueError('prior probe task differs from the expected task')
    source_receipt = Path(old['out']) / 'live-host-receipt.json'
    if not source_receipt.resolve().is_relative_to(stage):
        raise ValueError('prior receipt is outside this campaign stage')
    prior = json.loads(source_receipt.read_bytes())
    if (prior.get('status') != 'FAILED' or prior.get('manifest_sha256') != digest(source_path)
            or not prior.get('cleanup') or not all(row.get('tree_confirmed') for row in prior['cleanup'])):
        raise ValueError('prior failed probe lacks confirmed cleanup or manifest identity')
    forced_run = prior['first_snapshot']['run_id']
    restored = [row['event'] for row in prior['first_startup_audit']['rows']
                if row.get('event', {}).get('type') == 'runtime_restored'
                and row['event'].get('current_run_id') == forced_run]
    if len(restored) != 1:
        raise ValueError('prior probe has no exact recorded checkpoint restore basis')
    verify_frozen(old)
    work = Path(old['work'])
    manifest = {key: old[key] for key in ('task_id', 'nonce', 'prefix', 'prefix_content', 'probe_dir')}
    manifest.update(schema='v4-live-host-manifest-v1', mode='recovery_only', segment=segment,
                    work=str(work), out=str(stage / segment), ledger=str(stage / 'budget-ledger.json'),
                    first_host_rounds=0, restored_host_rounds=2,
                    source_manifest=dict(path=str(source_path), sha256=digest(source_path)),
                    source_receipt=dict(path=str(source_receipt), sha256=digest(source_receipt)),
                    forced_host_run_id=forced_run, expected_checkpoint_run_id=restored[0]['restored_run_id'],
                    existing_prefix_sha256=digest(work / old['prefix']),
                    frozen={name: digest(work / name) for name in old['frozen']},
                    driver_deadline_epoch=min(time.time() + 420, caps['deadline_epoch'] - 55),
                    executables={name: dict(path=str(path.resolve()), sha256=digest(path)) for name, path in {
                        'driver': Path(__file__), 'host': ROOT / 'target/debug/agent-host.exe',
                        'python': Path(sys.executable)}.items()})
    if manifest['existing_prefix_sha256'] is None:
        raise ValueError('prior model prefix does not exist')
    verify_existing_prefix(manifest)
    manifest['prompt'] = ("Operator diagnostic recovery only: the previous host was forcibly stopped after the "
                          "controller encountered an unsupported operation route. A successful typed cancellation "
                          "was NOT verified. Preserve all original product requirements and leave the task active. "
                          f"Use fs.read to read the existing file {manifest['prefix']}, then report its exact marker "
                          "and the observed read result. Do not modify/create files, create directories, load extra "
                          "tools, launch any process, run tests or mark the persistent task complete. "
                          "This is a cold read after a forced host exit, not a successful cancellation test.")
    return manifest


def run(stage, task_id, label='live-cancel-restore', first_rounds=6, restored_rounds=4,
        recover_from_manifest=None):
    """The only entry that reads provider configuration; root invokes this explicitly."""
    module = campaign_module()
    stage = Path(stage).resolve()
    if not label or any(c not in 'abcdefghijklmnopqrstuvwxyz0123456789-' for c in label):
        raise ValueError('segment label must be lowercase ASCII letters/digits/hyphens')
    segment = 'model-' + label
    with module.campaign_lock(stage):
        caps = module.limits(stage)
        used, tools = module.counts(stage)
        if recover_from_manifest is not None:
            total_rounds = 2
            if used + total_rounds > caps['main_decisions']:
                raise ValueError('the existing campaign cannot allocate two recovery-only rounds')
        else:
            total_rounds = round_allocation(first_rounds, restored_rounds, caps['main_decisions'] - used)
        if tools + 32 > caps['tool_attempts']:
            raise ValueError('the shared campaign has insufficient remaining tool budget')
        if time.time() >= caps['deadline_epoch'] - 60:
            raise ValueError('original campaign deadline has no safe cleanup window')
        if (stage / segment).exists():
            raise FileExistsError('never overwrite or replay an existing live segment')
        manifest = (make_recovery_manifest(stage, segment, recover_from_manifest, caps, task_id)
                    if recover_from_manifest is not None
                    else make_manifest(stage, segment, task_id, caps, first_rounds, restored_rounds))
        manifest_dir = stage / 'live-host-manifests'
        manifest_dir.mkdir(exist_ok=True)
        manifest_path = manifest_dir / (label + '.json')
        with manifest_path.open('xb') as file:
            file.write(encoded(manifest) + b'\n')
        manifest_sha = digest(manifest_path)
        env = module.build_env(ROOT)
        env.update(AGENT_PYTHON=sys.executable, MAINTENANCE_MAX_CALLS_PER_MAINTAIN='0',
                   OPENAI_API_PROTOCOL='chat', OPENAI_CHAT_THINKING='disabled')
        env.pop('OPENAI_RESPONSES_REASONING_EFFORT', None)
        protected = parent_protected_files(stage)
        config = module.RunnerConfig(
            segment=segment, campaign_dir=stage, mode='feedback', rounds=total_rounds,
            prompt_text=manifest['prompt'], env=env, api_protocol='chat',
            protected_files=protected, max_cost_usd=caps['estimated_cost_usd'],
            max_output_tokens=8192, pricing=module.ByteReservedPricing(), ledger_class=module.SnapshotLedger,
            child_command=[sys.executable, '-B', str(Path(__file__).resolve()), '--driver',
                           '--manifest', str(manifest_path), '--manifest-sha256', manifest_sha],
            child_wait_timeout_s=max(1, manifest['driver_deadline_epoch'] - time.time() + 15))
        return module.run_segment(config)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('stage', type=Path, nargs='?')
    parser.add_argument('--expected-task-id')
    parser.add_argument('--label')
    parser.add_argument('--first-rounds', type=int, default=6)
    parser.add_argument('--restored-rounds', type=int, default=4)
    parser.add_argument('--recover-from-manifest', type=Path,
                        help='read the prior forced-exit prefix with one host and at most two model rounds')
    parser.add_argument('--driver', action='store_true')
    parser.add_argument('--manifest', type=Path)
    parser.add_argument('--manifest-sha256')
    args = parser.parse_args(argv)
    if args.driver:
        if args.manifest is None or not args.manifest_sha256:
            parser.error('driver requires the pinned manifest and checksum')
        return run_driver(args.manifest, args.manifest_sha256)
    if args.stage is None or (not args.expected_task_id and args.recover_from_manifest is None):
        parser.error('explicit execution requires stage and expected task id')
    label = args.label or ('live-cold-read-only' if args.recover_from_manifest else 'live-cancel-restore')
    if args.recover_from_manifest is not None:
        return run(args.stage, args.expected_task_id, label, recover_from_manifest=args.recover_from_manifest)
    return run(args.stage, args.expected_task_id, label, args.first_rounds, args.restored_rounds)


if __name__ == '__main__':
    raise SystemExit(main())
