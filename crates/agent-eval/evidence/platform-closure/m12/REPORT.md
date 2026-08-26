# M12 closure evidence — brokered production effect path

Schema `platform-closure.m12.v1`. Generated mechanically by `agent-eval --platform-closure-m12`; every observed row was executed inside this run.

| metric | value |
| --- | --- |
| rows | 28 |
| brokerable | 13 |
| non_transactional exceptions | 5 |
| read-only / no-effect | 10 |
| unresolved | 0 |

## Coverage

| row | family | class | seams | fencing | resolved |
| --- | --- | --- | --- | --- | --- |
| classification/fs.list | read_only_observation | no_effect | - | - | yes |
| classification/fs.read | read_only_observation | no_effect | - | - | yes |
| classification/search.grep | read_only_observation | no_effect | - | - | yes |
| classification/artifact.read | read_only_observation | no_effect | - | - | yes |
| classification/git.status | read_only_observation | no_effect | - | - | yes |
| classification/git.diff | read_only_observation | no_effect | - | - | yes |
| classification/capability.manage | read_only_observation | no_effect | - | - | yes |
| classification/context.manage | read_only_observation | no_effect | - | - | yes |
| classification/task.complete | read_only_observation | no_effect | - | - | yes |
| classification/fs.write | workspace_write_single_file | brokerable | - | - | yes |
| classification/edit.replace | workspace_write_single_file | brokerable | - | - | yes |
| classification/edit.patch | workspace_write_multi_file_composite | brokerable | - | - | yes |
| classification/process.run | generic_process_spawn | non_transactional_exception | - | - | yes |
| classification/shell.exec | generic_process_spawn | non_transactional_exception | - | - | yes |
| classification/process.session | generic_process_spawn | non_transactional_exception | - | - | yes |
| observed/fs.write/applied | workspace_write_single_file | brokerable | post-commit reopen of the reservation journal -> Applied | - | yes |
| observed/edit.replace/applied | workspace_write_single_file | brokerable | post-commit reopen of the reservation journal -> Applied | - | yes |
| observed/edit.patch/applied | workspace_write_multi_file_composite | brokerable | post-commit reopen of the reservation journal -> Applied | - | yes |
| observed/fs.write/pre_reserve_refusal | workspace_write_single_file | brokerable | authority epoch advanced between prepare and commit -> None | - | yes |
| observed/fs.write/broker_unavailable_fence | workspace_write_single_file | brokerable | - | reserve failure rejected BrokerUnavailable; staged effect settled NotApplied; no journal entry exists | yes |
| observed/plugin.notes.write/binding_revocation_fence | workspace_write_single_file | brokerable | - | replacement moved the epoch 1 -> 2; the stamped lease was fenced per binding while the following honest lease committed; reopened journal: fenced=None, applied=Applied | yes |
| crash-window/reserve-only | workspace_write_single_file | brokerable | reserved, never dispatched (writer crashed pre-dispatch) -> NotApplied | - | yes |
| crash-window/dispatch-without-ack | workspace_write_single_file | brokerable | dispatched, acknowledgement lost (writer crashed post-apply) -> Ambiguous | - | yes |
| crash-window/identity-drift | workspace_write_single_file | brokerable | reserved record probed with mismatched identity -> Ambiguous | - | yes |
| transport/process-coordinator | workspace_write_any | brokerable | - | - | yes |
| observed/fs.read/no-effect-spot-check | read_only_observation | no_effect | - | - | yes |
| observed/shell.exec/exception-execution | generic_process_spawn | non_transactional_exception | - | - | yes |
| observed/process.run/exception-execution | generic_process_spawn | non_transactional_exception | - | - | yes |

## Gates

- every brokerable row resolves on the journaled reserve/dispatch/ack path: true
- crash windows reconcile as NotApplied/Applied/Ambiguous: true
- exceptions stay inside the documented generic shell/process scope: true
- zero unresolved rows: true

**Verdict: PASS**
