"""Linux mechanism probe, NOT a Rust repository integration test.
Creates and cleans only its own processes, showing a reaped leader can have live group members.
"""
import ctypes, json, os, signal, subprocess, time
from pathlib import Path
out = Path(__file__).with_suffix('.json')
result = {'kind': 'OS_MECHANISM_PROBE_NOT_REPOSITORY_TEST', 'platform': 'Linux', 'child_cleanup_confirmed': False}
libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(36, 1, 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), 'PR_SET_CHILD_SUBREAPER failed')
p = None
member = None
try:
    p = subprocess.Popen(['/bin/sh', '-c', 'sleep 30 & echo $!; exit 0'], start_new_session=True,
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    member = int(p.stdout.readline().strip())
    p.stdout.close()
    p.wait(timeout=3)
    pgid = os.getpgid(member)
    assert pgid == p.pid and pgid != os.getpgrp()
    try:
        os.kill(p.pid, 0); leader_exists = True
    except ProcessLookupError:
        leader_exists = False
    os.killpg(pgid, 0)
    result.update({'leader_pid': p.pid, 'member_pid': member, 'group_id': pgid,
                   'leader_reaped': not leader_exists, 'group_member_alive_after_leader_reaped': True,
                   'current_watchdog_leader_alive_predicate': leader_exists,
                   'consequence': 'the current leader-exists condition would skip signaling this surviving group'})
finally:
    if p is not None:
        try:
            if p.pid != os.getpgrp(): os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        if p.poll() is None:
            p.kill(); p.wait(timeout=3)
    if member is not None:
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            try:
                got, status = os.waitpid(member, os.WNOHANG)
            except ChildProcessError:
                got = member
            if got == member:
                result['child_cleanup_confirmed'] = True
                break
            time.sleep(.01)
    out.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding='utf-8')
print(json.dumps(result, ensure_ascii=False, indent=2))
assert result['child_cleanup_confirmed']
