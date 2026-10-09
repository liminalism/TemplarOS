#!/usr/bin/python3
"""Owned debugger crash boundaries; no installed service, mount or desktop access."""
import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shlex
import signal
import sqlite3
import subprocess
import tempfile
import threading
import time
from gi.repository import Gio, GLib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--filed', type=Path, required=True)
parser.add_argument('--filed-tests', type=Path, required=True)
parser.add_argument('--reopen-test', type=Path, required=True)
parser.add_argument('--gdb', type=Path, required=True)
parser.add_argument('--gdb-library-path', required=True)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
assert os.environ.get('TEMPLAR_INDEX_CRASH_PRIVATE_BUS') == '1', 'Use the private wrapper'
repo = Path(__file__).resolve().parent.parent
out = args.output.resolve()
out.mkdir(parents=True, exist_ok=True)
filed_path = args.filed.resolve()

def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)

def wait_for(predicate, seconds, label):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(.05)
    raise AssertionError(label)

def facts(cache):
    with sqlite3.connect(cache.as_uri() + '?mode=ro', uri=True, timeout=2) as db:
        head = int(db.execute("SELECT value FROM index_meta WHERE key='committed_generation'").fetchone()[0])
        rows = db.execute('SELECT entry_id,root_id,path_bytes,size_bytes,written_generation,deleted_generation FROM entries ORDER BY entry_id').fetchall()
        changes = db.execute('SELECT seq,generation FROM changes ORDER BY seq').fetchall()
        roots = db.execute('SELECT root_id,committed_generation,entry_count FROM roots ORDER BY root_id').fetchall()
        assert all(row[4] <= head for row in rows), (head, rows[:3])
        assert all(row[1] <= head for row in changes), (head, changes[-3:])
        assert all(row[1] <= head for row in roots), (head, roots)
        assert db.execute('PRAGMA quick_check').fetchone()[0] == 'ok'
        encoded = [(a, b, base64.b64encode(c).decode(), d, e, f) for a, b, c, d, e, f in rows]
        return {'head': head, 'row_count': len(rows), 'roots': roots, 'change_count': len(changes),
                'last_change': changes[-1] if changes else None,
                'row_sha256': hashlib.sha256(json.dumps(encoded).encode()).hexdigest()}

def source_line(path, anchor, start=None):
    text = (repo / path).read_text()
    begin = text.index(start) if start else 0
    return text.count('\n', 0, text.index(anchor, begin)) + 1

with tempfile.TemporaryDirectory(prefix='templar-index-crash-') as temp:
    root = Path(temp)
    (root / '.templar-index-crash-fixture').write_text('owned WP17 fixture\n')
    env = os.environ.copy()
    env.update(PATH='/usr/bin:/bin', RUST_LOG='warn', TEMPLAR_INDEX_FIXTURE_GUARD='1')
    env['DBUS_SYSTEM_BUS_ADDRESS'] = env['DBUS_SESSION_BUS_ADDRESS']
    for key, folder in [('HOME', 'home'), ('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'),
                        ('XDG_STATE_HOME', 'state'), ('XDG_CACHE_HOME', 'cache'), ('XDG_RUNTIME_DIR', 'runtime')]:
        path = root / folder
        path.mkdir(mode=0o700)
        env[key] = str(path)
    tree = root / 'tree'
    tree.mkdir()
    for n in range(5000):
        (tree / f'item-{n:04}.txt').write_bytes(b'fixture\n')
    cache = root / 'index.sqlite3'
    filed_argv = ['--index-root', str(tree), '--index-db', str(cache), '--state-db',
                  str(root / 'state/session.db'), '--reserve-percent', '0']
    processes, logs, debuggers, sealed_fds = [], [], [], []
    conn = Gio.DBusConnection.new_for_address_sync(env['DBUS_SESSION_BUS_ADDRESS'],
        Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
        None, None)

    def log(name):
        stream = open(out / name, 'w')
        logs.append(stream)
        return stream

    def call(name, signature=None, values=(), timeout=5000):
        return conn.call_sync('org.templaros.Files', '/org/templaros/Files',
            'org.templaros.Files.Index1', name, GLib.Variant(signature, values) if signature else None,
            None, Gio.DBusCallFlags.NO_AUTO_START, timeout, None).unpack()

    def ready():
        try:
            roots = json.loads(call('Roots')[0])['roots']
            return roots and all(r['state'] == 'current' for r in roots) and roots
        except GLib.Error:
            return False

    def start(name):
        stream = log(name + '.log')
        process = subprocess.Popen([str(filed_path), *filed_argv], env=env, stdout=stream, stderr=stream)
        processes.append(process)
        wait_for(ready, 30, name + ' roots current')
        return process

    def snapshot():
        request = json.dumps({'roots': [tree_id], 'fields': 255, 'detail': 'materialized', 'max_entries': 10000})
        value, descriptors = conn.call_with_unix_fd_list_sync('org.templaros.Files', '/org/templaros/Files',
            'org.templaros.Files.Index1', 'OpenSnapshot', GLib.Variant('(s)', (request,)), None,
            Gio.DBusCallFlags.NO_AUTO_START, 35000, None, None)
        handle, manifest = value.unpack()
        fd = descriptors.get(handle)
        return fd, json.loads(manifest)

    def debug(phase, executable, argv, location, wanted_root=None, row_limit=None, extra_env=None):
        marker, release = root / (phase + '.stopped'), root / (phase + '.release')
        script = out / (phase + '.gdb')
        setup = f'''
import gdb,json,pathlib,time
class Boundary(gdb.Breakpoint):
    def stop(self):
        frame=gdb.selected_frame()
        if {wanted_root!r} is not None:
            owner=frame.read_var('self').dereference()['root']
            number=int(owner[owner.type.fields()[0].name])
            if number!={wanted_root!r}:return False
        if {row_limit!r} is not None and int(frame.read_var('entry_count')) < {row_limit!r}:
            return False
        return True
Boundary({location!r})
'''
        stopped = f'''
frame=gdb.selected_frame();line=frame.find_sal()
result={{'phase':{phase!r},'pid':gdb.selected_inferior().pid,'function':frame.name(),'line':line.line}}
if {row_limit!r} is not None:
    result['entry_count']=int(frame.read_var('entry_count'))
    result['payload_bytes']=int(frame.read_var('payload').dereference()['length'])
pathlib.Path({str(marker)!r}).write_text(json.dumps(result))
deadline=time.monotonic()+20
while not pathlib.Path({str(release)!r}).exists():
    if time.monotonic()>deadline:raise gdb.GdbError('owned crash parent release deadline')
    time.sleep(.05)
'''
        script.write_text('set pagination off\nset confirm off\nset auto-load off\nset debuginfod enabled off\n'
            'set startup-with-shell off\nunset environment LD_LIBRARY_PATH\n'
            'set args ' + ' '.join(shlex.quote(str(word)) for word in argv) + '\npython\n' + setup
            + '\nend\nrun\nbt 6\npython\n' + stopped + '\nend\nkill\nquit\n')
        debug_env = env.copy()
        debug_env['LD_LIBRARY_PATH'] = args.gdb_library_path
        if extra_env:
            debug_env.update(extra_env)
        stream = log(phase + '-debugger.log')
        process = subprocess.Popen([str(args.gdb.resolve()), '--nx', '--quiet', '--batch',
            '-iex', 'set auto-load off', '-iex', 'set debuginfod enabled off',
            '-x', str(script), str(executable.resolve())], env=debug_env, cwd=repo,
            stdout=stream, stderr=stream)
        debuggers.append((process, executable.resolve()))
        return process, marker, release

    def stopped(marker, debugger):
        def check():
            assert debugger.poll() is None, 'debugger exited: ' + str(debugger.returncode)
            return json.loads(marker.read_text()) if marker.exists() and marker.stat().st_size else None
        return wait_for(check, 45, 'precise crash boundary')

    def release_and_wait(debugger, release):
        release.write_text('kill only owned inferior\n')
        debugger.wait(timeout=10)
        assert debugger.returncode == 0, debugger.returncode

    def old_cursor_refused(cursor):
        try:
            call('FetchPage', '(suu)', (cursor, 0, 100))
            raise AssertionError('restarted service accepted its old cursor')
        except GLib.Error as error:
            assert 'SnapshotRequired' in str(error), str(error)
            return str(error)

    report = {}
    try:
        warm = start('warm-filed')
        roots = ready()
        native_root = base64.b64encode(os.fsencode(tree)).decode()
        tree_id = next(r['root'] for r in roots if r['path_b64'] == native_root)
        old_cursor, generation, result = call('Search', '(s)', (json.dumps({'text': 'item-', 'page_size': 100}),))
        assert json.loads(result)['total'] == 5000
        fd, manifest = snapshot()
        sealed_fds.append(fd)
        seals = fcntl.fcntl(fd, fcntl.F_GET_SEALS)
        assert seals & 15 == 15 and manifest['sealed']
        original_bytes = os.pread(fd, os.fstat(fd).st_size, 0)
        original_sha = hashlib.sha256(original_bytes).hexdigest()
        before = facts(cache)
        stop(warm)
        (tree / 'unpublished.txt').write_bytes(b'x' * 37)
        location = 'crates/templar-file-index/src/generation.rs:' + str(source_line(
            'crates/templar-file-index/src/generation.rs', 'transaction.commit()?;', 'fn commit_transaction('))
        debugger, marker, release = debug('scan-commit', filed_path, filed_argv, location, wanted_root=tree_id)
        boundary = stopped(marker, debugger)
        paused = facts(cache)
        assert next(r[1:] for r in paused['roots'] if r[0] == tree_id) == next(r[1:] for r in before['roots'] if r[0] == tree_id)
        with sqlite3.connect(cache.as_uri() + '?mode=ro', uri=True) as db:
            assert db.execute('SELECT COUNT(*) FROM entries WHERE path_bytes=?', (os.fsencode(tree / 'unpublished.txt'),)).fetchone()[0] == 0
        release_and_wait(debugger, release)
        after = facts(cache)
        assert paused == after, (paused, after)
        assert hashlib.sha256(os.pread(fd, len(original_bytes), 0)).hexdigest() == original_sha
        recovered = start('recovered-scan')
        refusal = old_cursor_refused(old_cursor)
        fresh_fd, fresh_manifest = snapshot()
        sealed_fds.append(fresh_fd)
        assert fresh_manifest['generation'] > manifest['generation'] and fresh_manifest['entry_count'] == 5002
        assert fcntl.fcntl(fresh_fd, fcntl.F_GET_SEALS) & 15 == 15
        report['scan_commit'] = {'boundary': boundary, 'paused': paused, 'after_kill': after,
            'old_cursor_error': refusal, 'fresh_generation': fresh_manifest['generation'],
            'fresh_entries': fresh_manifest['entry_count'], 'old_sealed_frame_survived': True}
        second_cursor = call('Search', '(s)', (json.dumps({'text': 'item-', 'page_size': 100}),))[0]
        stop(recovered)

        location = 'crates/templar-file-index/src/snapshot.rs:' + str(source_line(
            'crates/templar-file-index/src/snapshot.rs', 'entry_count += 1;'))
        debugger, marker, release = debug('snapshot-write', filed_path, filed_argv, location, row_limit=2500)
        wait_for(ready, 30, 'snapshot daemon roots current')
        response = []
        def pending_snapshot():
            try:
                value = snapshot()
                sealed_fds.append(value[0])
                response.append({'unexpected_fd': value[1]})
            except GLib.Error as error:
                response.append({'error': str(error)})
        request_thread = threading.Thread(target=pending_snapshot)
        request_thread.start()
        boundary = stopped(marker, debugger)
        assert boundary['entry_count'] == 2500 and boundary['payload_bytes'] > 262144, boundary
        partial = []
        for path in Path(f"/proc/{boundary['pid']}/fd").iterdir():
            try:
                if 'memfd:templar-index-snapshot' not in os.readlink(path):
                    continue
                descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC)
                try:
                    partial.append({'size': os.fstat(descriptor).st_size, 'header_hex': os.pread(descriptor, 64, 0).hex(),
                                    'seals': fcntl.fcntl(descriptor, fcntl.F_GET_SEALS)})
                finally:
                    os.close(descriptor)
            except FileNotFoundError:
                pass
        assert len(partial) == 1 and partial[0]['size'] > 262144 and partial[0]['header_hex'] == '00' * 64 and partial[0]['seals'] == 0, partial
        paused = facts(cache)
        release_and_wait(debugger, release)
        request_thread.join(timeout=10)
        assert not request_thread.is_alive() and len(response) == 1 and 'error' in response[0], response
        after = facts(cache)
        assert paused == after
        assert hashlib.sha256(os.pread(fd, len(original_bytes), 0)).hexdigest() == original_sha
        recovered = start('recovered-snapshot')
        refusal = old_cursor_refused(second_cursor)
        descriptor, new_manifest = snapshot()
        sealed_fds.append(descriptor)
        assert new_manifest['sealed'] and fcntl.fcntl(descriptor, fcntl.F_GET_SEALS) & 15 == 15
        report['snapshot_write'] = {'boundary': boundary, 'partial_memfd': partial, 'call_response': response,
            'paused': paused, 'after_kill': after, 'old_cursor_error': refusal,
            'resnapshot_generation': new_manifest['generation'], 'old_sealed_frame_survived': True}
        stop(recovered)

        delta_tmp = root / 'delta'
        delta_tmp.mkdir()
        delta_name = 'delta_catchup::tests::delta_a_crash_after_commit_before_pointer_publication_requires_reconciliation'
        location = 'crates/templar-file-index/src/mutation.rs:' + str(source_line(
            'crates/templar-file-index/src/mutation.rs', 'transaction.commit()?;'))
        debugger, marker, release = debug('delta-apply', args.filed_tests, ['--exact', delta_name, '--nocapture'],
            location, extra_env={'TMPDIR': str(delta_tmp)})
        boundary = stopped(marker, debugger)
        caches = list(delta_tmp.rglob('index.sqlite3'))
        assert len(caches) == 1, caches
        delta_cache = caches[0]
        pointer = delta_cache.parent / 'delta-checkpoint.json'
        fence = pointer.with_suffix('.root-1.pending')
        assert pointer.is_file() and fence.is_file()
        pointer_bytes = pointer.read_bytes()
        paused = facts(delta_cache)
        release_and_wait(debugger, release)
        after = facts(delta_cache)
        assert paused == after and pointer.read_bytes() == pointer_bytes and fence.is_file()
        reopen_env = env.copy()
        reopen_env.update(TEMPLAR_INDEX_CRASH_FIXTURE=str(root), TEMPLAR_INDEX_CRASH_CACHE=str(delta_cache),
                          TEMPLAR_INDEX_CRASH_STATE=str(pointer))
        reopen = subprocess.run([str(args.reopen_test.resolve()), '--ignored', '--exact',
            'killed_delta_reopens_with_unchanged_rows_and_a_replay_fence', '--nocapture'], env=reopen_env,
            capture_output=True, text=True, timeout=20)
        (out / 'delta-reopen.log').write_text(reopen.stdout + reopen.stderr)
        assert reopen.returncode == 0, reopen.stdout + reopen.stderr
        report['delta_apply'] = {'boundary': boundary, 'paused': paused, 'after_kill': after,
            'checkpoint_pointer_sha256': hashlib.sha256(pointer_bytes).hexdigest(), 'pending_fence_retained': True,
            'production_reopen_refused_replay': True, 'checkpoint_provider': 'fake; no Btrfs mount or send ioctl'}
        report['binaries'] = {name: {'path': str(path.resolve()), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
            for name, path in [('filed', args.filed), ('filed_tests', args.filed_tests), ('reopen_test', args.reopen_test), ('gdb', args.gdb)]}
        report['installed_or_physical_qualification'] = False
        (out / 'crash-index.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))
    finally:
        for process in reversed(processes):
            stop(process)
        # Only direct inferior children of our debugger, with their actual executable checked.
        for debugger, executable in reversed(debuggers):
            if debugger.poll() is None:
                children = Path(f'/proc/{debugger.pid}/task/{debugger.pid}/children').read_text().split()
                for child in children:
                    try:
                        if Path(f'/proc/{child}/exe').resolve() == executable:
                            os.kill(int(child), signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                stop(debugger)
        for descriptor in sealed_fds:
            os.close(descriptor)
        for stream in logs:
            stream.close()
