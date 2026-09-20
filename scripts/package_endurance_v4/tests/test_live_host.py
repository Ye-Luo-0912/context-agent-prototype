"""Controller-only tests: no provider, host process, or probe execution."""
import copy
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

MODULE = Path(__file__).resolve().parents[1] / 'live_host.py'
spec = importlib.util.spec_from_file_location('v4_live_host_tests', MODULE)
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


class FragmentedPipe:
    def __init__(self, rows):
        self.input = io.BytesIO(b''.join(struct.pack('<I', len(live.encoded(r))) + live.encoded(r) for r in rows))
        self.output = bytearray()

    def read(self, count):
        return self.input.read(min(count, 3))

    def write(self, value):
        self.output.extend(value)
        return len(value)


class LiveControllerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.manifest = dict(task_id='task-1', nonce='nonce-1', segment='model-live', work=str(self.root),
                             prefix='app/tests/.probe/prefix.txt', prefix_content='exact-prefix',
                             argv=['python', '-c', 'bounded-probe'], frozen={})
        self.operation = dict(run_id='run-1', task_id='task-1', turn_id='turn-1', scope_id='scope-1',
                              operation_id='op-1', generation=2, call_id='call-1',
                              tool_name='process.run', argument_digest='a' * 64)
        self.call = dict(id='call-1', name='process.run', arguments=dict(argv=self.manifest['argv'], timeout_ms=90000))
        self.events = [dict(type='model_started'),
                       dict(type='model_used', usage_identity='observed', role='main',
                            usage=dict(input_tokens=100, output_tokens=10)),
                       dict(type='tool_started', call=dict(id='prefix-call', name='fs.write',
                            arguments=dict(path=self.manifest['prefix'], content=self.manifest['prefix_content']))),
                       dict(type='tool_finished', output=dict(call_id='prefix-call', tool_name='fs.write', ok=True)),
                       dict(type='operation_accepted', snapshot=dict(identity=self.operation, state=dict(phase='accepted'))),
                       dict(type='tool_started', call=self.call)]
        self.ledger = dict(api_protocol='chat', cap_stopped=False, reserved_usd=0, unknown_usd=0,
                           attempts=[dict(segment='model-live', status='committed')])

    def test_constructor_failure_retains_cleanup_in_driver_receipt(self):
        manifest = dict(work=str(self.root), executables={'host': {'path': 'fixture-host'}})
        for confirmed in (True, False):
            with self.subTest(confirmed=confirmed):
                output = self.root/str(confirmed)
                cleanup = dict(tree_confirmed=confirmed, outcome='fixture_exit')
                with mock.patch.object(live, 'os', SimpleNamespace(name='nt', environ={})), \
                     mock.patch.object(live, 'spawn_windows_owned', return_value=object()), \
                     mock.patch.object(live, 'stop_windows_owned', return_value=cleanup), \
                     mock.patch.object(live.LiveHost, 'connect', side_effect=RuntimeError('restore fenced')):
                    with self.assertRaises(live.HostStartupError) as raised:
                        live.LiveHost(manifest, output, 2, mock.Mock())
                receipt = {'cleanup': []}
                live.record_driver_failure(receipt, raised.exception)
                self.assertEqual(receipt['cleanup'][0]['tree_confirmed'], confirmed)
                self.assertEqual(receipt['status'], 'FAILED' if confirmed else 'CLEANUP_UNCONFIRMED')
                self.assertIn('restore fenced', receipt['error'])
                self.assertTrue((output/'cleanup.json').is_file())

    def test_cancel_requires_known_usage_settled_ledger_exact_nonce_and_live_operation(self):
        marker = dict(nonce='nonce-1', pid=123)
        self.assertTrue(live.cancel_gate(self.events, self.ledger, self.manifest, marker, True))
        snapshot = dict(focus=dict(task_id='task-1'), continue_readiness=dict(reason='turn_running', can_continue=False))
        self.assertTrue(live.observed_turn_running(snapshot, self.manifest))
        snapshot['continue_readiness'] = dict(reason='ready', can_continue=True)
        self.assertFalse(live.observed_turn_running(snapshot, self.manifest))
        for changed, field, value in [(self.ledger, 'reserved_usd', .1), (self.ledger, 'unknown_usd', .1),
                                      (self.ledger, 'cap_stopped', True), (marker, 'nonce', 'old-nonce')]:
            edited = copy.deepcopy(changed)
            edited[field] = value
            ledger, observed = (edited, marker) if changed is self.ledger else (self.ledger, edited)
            self.assertFalse(live.cancel_gate(self.events, ledger, self.manifest, observed, True))
        self.assertFalse(live.cancel_gate(self.events, self.ledger, self.manifest, marker, False))
        unknown = copy.deepcopy(self.events)
        unknown[1]['usage_identity'] = 'unknown'
        self.assertFalse(live.cancel_gate(unknown, self.ledger, self.manifest, marker, True))
        raced = self.events + [dict(type='model_started')]
        self.assertFalse(live.cancel_gate(raced, self.ledger, self.manifest, marker, True))
        wrong = copy.deepcopy(self.events)
        wrong[4]['snapshot']['identity']['task_id'] = 'other-task'
        self.assertFalse(live.cancel_gate(wrong, self.ledger, self.manifest, marker, True))
        unfinished = copy.deepcopy(self.events)
        unfinished[3]['output']['ok'] = False
        self.assertFalse(live.cancel_gate(unfinished, self.ledger, self.manifest, marker, True))

    def test_effect_approval_is_exact_and_unknown_request_is_cancelled_then_denied(self):
        self.assertTrue(live.allowed_call(self.call, self.manifest, 'cancel'))
        self.assertFalse(live.allowed_call(self.call, self.manifest, 'recover'))
        extra = copy.deepcopy(self.call)
        extra['arguments']['env'] = {'UNEXPECTED': '1'}
        self.assertFalse(live.allowed_call(extra, self.manifest, 'cancel'))
        wrong_timeout = copy.deepcopy(self.call)
        wrong_timeout['arguments']['timeout_ms'] = 120000
        self.assertFalse(live.allowed_call(wrong_timeout, self.manifest, 'cancel'))
        write = dict(name='fs.write', arguments=dict(path=self.manifest['prefix'], content='exact-prefix'))
        self.assertTrue(live.allowed_call(write, self.manifest, 'cancel'))
        write['arguments']['path'] = 'app/snapshot.py'
        self.assertFalse(live.allowed_call(write, self.manifest, 'cancel'))
        class Host:
            calls = []
            def rpc(self, *args):
                self.calls.append(args)
                return dict(outcome='delivered')
        host = Host()
        snapshot = dict(pending_approvals=[dict(request_id='approval-1', call_name='process.run')])
        with self.assertRaisesRegex(RuntimeError, 'unrecognized'):
            live.handle_approvals(host, snapshot, self.events[:-1] + [dict(type='tool_started', call=extra)],
                                  self.manifest, 'cancel')
        self.assertEqual(host.calls[0], ('cancel', dict(expected_task_id='task-1', expected_turn_id='turn-1')))
        self.assertEqual(host.calls[1][1]['decision'], 'Deny')

    def test_public_rpc_and_subscription_frames_are_distinct_and_events_are_complete(self):
        envelope = live.request('subscribe', dict(replay_after_seq=3))
        response = dict(request_id=envelope['request_id'], kind='response',
                        payload=dict(status='success', value=dict(watermark=3, resync_required=False)))
        pipe = FragmentedPipe([response])
        _, result = live.exchange(pipe, envelope)
        self.assertFalse(result['resync_required'])
        sent = json.loads(bytes(pipe.output)[4:])
        self.assertEqual(sent['route'], dict(namespace='work', operation='subscribe'))
        self.assertEqual(sent['payload']['replay_after_seq'], 3)
        self.assertEqual(sent['protocol']['schema_digest'], hashlib.sha256(b'focus-agent.platform.work.v1|run-scoped').hexdigest())
        capture = live.EventLog(self.root / 'events.jsonl')
        capture.set_baseline('run-1', 3)
        self.assertEqual(capture.path.read_bytes(), b'', 'a snapshot bound is not a fabricated event')
        host = live.LiveHost.__new__(live.LiveHost)
        host.run_id, host.stopping, host.events = 'run-1', False, capture
        row = dict(run_id='run-1', seq=4, event=dict(type='model_started'))
        host.subscriber = FragmentedPipe([dict(kind='notification', route=dict(namespace='work', operation='event'),
                                              payload=dict(envelope=row))])
        original = capture.append
        def append_and_stop(event):
            original(event)
            host.stopping = True
        capture.append = append_and_stop
        host.receive()
        self.assertIsNone(capture.error)
        self.assertEqual(json.loads(capture.path.read_text()), row)
        original(row)
        original(dict(run_id='run-1', seq=4, event=dict(type='model_delta', delta='not durable')))
        self.assertEqual(len(capture.rows), 1)
        with self.assertRaisesRegex(ValueError, 'sequence gap'):
            original(dict(run_id='run-1', seq=6, event=dict(type='tool_started')))
        run_id = '12345678-1234-4234-8234-123456789abc'
        journal = self.root / '.focus-agent/traces' / (run_id + '.jsonl')
        journal.parent.mkdir(parents=True)
        startup = [dict(run_id=run_id, seq=1, event=dict(type='run_started')),
                   dict(run_id=run_id, seq=2, event=dict(type='runtime_restored', restored_run_id='prior'))]
        journal.write_bytes(b''.join(live.encoded(r) + b'\n' for r in startup))
        audit = live.read_startup_audit(self.root, run_id, 2)
        self.assertEqual(audit['rows'], startup)
        self.assertEqual(audit['source'], 'read_only_runtime_journal_startup_prefix')
        startup[-1]['event'] = dict(type='model_started')
        journal.write_bytes(b''.join(live.encoded(r) + b'\n' for r in startup))
        with self.assertRaisesRegex(ValueError, 'unobserved model/tool'):
            live.read_startup_audit(self.root, run_id, 2)

    def test_manifest_pins_actual_executables_and_preserves_absent_candidate_file(self):
        from runtime_endurance_incremental_runner import RunnerConfig, _verify_protected_baseline
        original = self.root / 'seed.txt'
        original.write_bytes(b'original protected file')
        live.save_json(self.root / 'baseline-lock.json', {'files': {'seed.txt': live.digest(original)}})
        cfg = RunnerConfig(segment='model-probe', campaign_dir=self.root,
                           protected_files=live.parent_protected_files(self.root))
        self.assertIsNone(_verify_protected_baseline(cfg, self.root, self.root))
        cfg.protected_files += ('app/snapshot.py', 'app/tests/test_snapshot.py')
        self.assertIsNotNone(_verify_protected_baseline(cfg, self.root, self.root),
                             'unrecorded candidate names would refuse the real runner before launch')
        host_binary = self.root / 'host.exe'
        host_binary.write_bytes(b'fixture executable identity; never executed')
        manifest = dict(self.manifest, schema='v4-live-host-manifest-v1',
                        executables={name: dict(path=str(path.resolve()), sha256=live.digest(path)) for name, path in
                                     [('host', host_binary), ('driver', MODULE), ('python', Path(sys.executable))]},
                        frozen={'app/tests/test_snapshot.py': None}, first_host_rounds=6, restored_host_rounds=4)
        path = self.root / 'manifest.json'
        live.save_json(path, manifest)
        expected = live.digest(path)
        live.verify_manifest(path, expected)
        candidate = self.root / 'app/tests/test_snapshot.py'
        candidate.parent.mkdir(parents=True)
        candidate.write_text('unexpected creation')
        with self.assertRaisesRegex(ValueError, 'frozen source'):
            live.verify_manifest(path, expected)
        candidate.unlink()
        host_binary.write_bytes(b'different executable')
        with self.assertRaisesRegex(ValueError, 'executable'):
            live.verify_manifest(path, expected)

    def test_controller_prepares_only_an_empty_exclusive_dir_and_uses_remaining_rounds(self):
        manifest = dict(self.manifest, probe_dir='app/tests/.runtime-probe-fresh')
        facts = live.prepare_probe_dir(manifest)
        folder = self.root / manifest['probe_dir']
        self.assertTrue(facts['exclusive_create'])
        self.assertEqual(facts['probe_files_written_by_controller'], [])
        self.assertEqual(list(folder.iterdir()), [])
        sentinel = folder / 'do-not-overwrite'
        sentinel.write_bytes(b'existing evidence')
        with self.assertRaises(FileExistsError):
            live.prepare_probe_dir(manifest)
        self.assertEqual(sentinel.read_bytes(), b'existing evidence')
        with self.assertRaisesRegex(ValueError, 'authorized'):
            live.prepare_probe_dir(dict(manifest, probe_dir='../.runtime-probe-escape'))
        self.assertEqual(live.round_allocation(4, 2, remaining=96 - 90), 6)
        with self.assertRaisesRegex(ValueError, 'remaining'):
            live.round_allocation(6, 4, remaining=96 - 90)
        with self.assertRaisesRegex(ValueError, 'at least two'):
            live.round_allocation(4, 1, remaining=6)
        with mock.patch.object(live, 'run', return_value=0) as run:
            self.assertEqual(live.main([str(self.root), '--expected-task-id', 'task-1',
                                       '--label', 'fresh-final-six', '--first-rounds', '4',
                                       '--restored-rounds', '2']), 0)
            run.assert_called_once_with(self.root, 'task-1', 'fresh-final-six', 4, 2)

    def test_checkpoint_and_snapshot_refuse_wrong_task_corrupt_payload_and_typed_recovery(self):
        folder = self.root / '.focus-agent/checkpoints'
        folder.mkdir(parents=True)
        checkpoint = dict(current_task_id='task-1', tasks=dict(tasks=[dict(id='task-1', current_directive={'body':'retained'})]),
                          context=dict(observation=self.manifest['prefix']))
        payload = live.encoded(checkpoint)
        header = dict(format='runtime-checkpoint-envelope-v1', payload_bytes=len(payload), checksum=hashlib.sha256(payload).hexdigest())
        path = folder / 'saved.json'
        path.write_bytes(live.encoded(header) + b'\n' + payload)
        self.assertEqual(live.verify_checkpoint(self.root, {'artifact':'saved.json'}, 'task-1', self.manifest['prefix'])['task_id'], 'task-1')
        with self.assertRaisesRegex(ValueError, 'current task'):
            live.verify_checkpoint(self.root, {'artifact':'saved.json'}, 'other', self.manifest['prefix'])
        path.write_bytes(path.read_bytes() + b'corrupt')
        with self.assertRaisesRegex(ValueError, 'integrity'):
            live.verify_checkpoint(self.root, {'artifact':'saved.json'}, 'task-1', self.manifest['prefix'])
        snapshot = dict(focus=dict(task_id='task-1'), workspace_root=str(self.root),
                        continue_readiness=dict(reason='recovery_required', can_continue=False))
        with self.assertRaisesRegex(ValueError, 'recovery-fenced'):
            live.validate_snapshot(snapshot, self.manifest)
        snapshot['continue_readiness'] = dict(reason='not_a_recovery_reason', can_continue=False)
        with self.assertRaisesRegex(ValueError, 'unknown typed'):
            live.validate_snapshot(snapshot, self.manifest)

    def test_recovery_only_pins_prior_failed_evidence_and_allocates_two_read_only_rounds(self):
        stage = self.root / 'stage'
        work = stage / 'workspace'
        folder = work / 'app/tests/.runtime-probe-old'
        folder.mkdir(parents=True)
        prefix = folder / 'prefix.txt'
        prefix.write_text('prefix-old-nonce\n', encoding='utf-8')
        prior_out = stage / 'model-failed'
        prior_out.mkdir()
        source = stage / 'prior-manifest.json'
        old = dict(schema='v4-live-host-manifest-v1', task_id='task-1', nonce='old-nonce',
                   work=str(work), out=str(prior_out), prefix=prefix.relative_to(work).as_posix(),
                   prefix_content='prefix-old-nonce\n', probe_dir=folder.relative_to(work).as_posix(),
                   frozen={'app/snapshot.py': None, 'app/tests/test_snapshot.py': None})
        live.save_json(source, old)
        source_bytes = source.read_bytes()
        live.save_json(prior_out / 'live-host-receipt.json', dict(status='FAILED', manifest_sha256=live.digest(source),
            cleanup=[dict(tree_confirmed=True)], first_snapshot=dict(run_id='forced-host'),
            first_startup_audit=dict(rows=[dict(event=dict(type='runtime_restored', current_run_id='forced-host',
                                                        restored_run_id='existing-checkpoint-run'))])))
        fake_root = self.root / 'fixture-root'
        binary = fake_root / 'target/debug/agent-host.exe'
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b'local fixture; never executed')
        with mock.patch.object(live, 'ROOT', fake_root):
            manifest = live.make_recovery_manifest(stage, 'model-recovery-only', source, {'deadline_epoch': 9999999999})
        self.assertEqual(manifest['mode'], 'recovery_only')
        self.assertEqual((manifest['first_host_rounds'], manifest['restored_host_rounds']), (0, 2))
        self.assertEqual(manifest['forced_host_run_id'], 'forced-host')
        self.assertEqual(manifest['expected_checkpoint_run_id'], 'existing-checkpoint-run')
        self.assertIn('successful typed cancellation was NOT verified', manifest['prompt'])
        self.assertEqual(source.read_bytes(), source_bytes)
        pinned = stage / 'new-recovery-manifest.json'
        live.save_json(pinned, manifest)
        live.verify_manifest(pinned, live.digest(pinned))
        events = [dict(type='tool_started', call=dict(id='read-1', name='fs.read', arguments={'path': old['prefix']})),
                  dict(type='tool_finished', output=dict(call_id='read-1', ok=True, model_content='prefix-old-nonce'))]
        self.assertTrue(live.successful_prefix_read(events, manifest))
        events[-1]['output']['ok'] = False
        self.assertFalse(live.successful_prefix_read(events, manifest))
        prefix.write_text('changed', encoding='utf-8')
        with self.assertRaisesRegex(ValueError, 'existing probe prefix'):
            live.verify_manifest(pinned, live.digest(pinned))
        with mock.patch.object(live, 'run', return_value=0) as run:
            self.assertEqual(live.main([str(stage), '--recover-from-manifest', str(source)]), 0)
            run.assert_called_once_with(stage, None, 'live-cold-read-only', recover_from_manifest=source)


if __name__ == '__main__':
    unittest.main()
