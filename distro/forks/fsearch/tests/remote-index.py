#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-or-later
import argparse, json, os, pathlib, re, signal, subprocess, tempfile, time
from gi.repository import Gio, GLib
parser=argparse.ArgumentParser(description="Private FSearch/Index1 integration; never a live user bus")
parser.add_argument('--filed',type=pathlib.Path,required=True)
parser.add_argument('--fsearch',type=pathlib.Path,required=True)
parser.add_argument('--probe',type=pathlib.Path,required=True)
parser.add_argument('--xvfb',default='Xvfb')
parser.add_argument('--strace',default='strace')
parser.add_argument('--output',type=pathlib.Path,required=True)
args=parser.parse_args()
assert os.environ.get('TEMPLAR_WP14_PRIVATE_BUS')=='1', 'Use remote-index.sh to create an owned private bus'
out=args.output.resolve();out.mkdir(parents=True,exist_ok=True)
def stop(p):
    if p.poll() is None:
        p.terminate()
        try: p.wait(timeout=5)
        except subprocess.TimeoutExpired: p.kill(); p.wait(timeout=5)
def call(conn, method, signature=None, args=()):
    return conn.call_sync('org.templaros.Files','/org/templaros/Files','org.templaros.Files.Index1',method,
        GLib.Variant(signature,args) if signature else None,None,Gio.DBusCallFlags.NO_AUTO_START,5000,None).unpack()
with tempfile.TemporaryDirectory(prefix='templar-wp14-') as temp:
    root=pathlib.Path(temp); env=os.environ.copy(); processes=[]; logs=[]; app_pid=None
    env.pop('WAYLAND_DISPLAY',None); env.update(PATH='/usr/bin:/bin',GDK_BACKEND='x11',LIBGL_ALWAYS_SOFTWARE='1',G_MESSAGES_DEBUG='fsearch-database-remote fsearch-window',NO_AT_BRIDGE='1',GTK_IM_MODULE='gtk-im-context-simple',GIO_USE_VFS='local',GTK_USE_PORTAL='0')
    for key,folder in [('HOME','home'),('XDG_CONFIG_HOME','config'),('XDG_DATA_HOME','data'),('XDG_STATE_HOME','state'),('XDG_CACHE_HOME','cache'),('XDG_RUNTIME_DIR','runtime')]:
        path=root/folder; path.mkdir(mode=0o700);env[key]=str(path)
    tree=root/'tree'; tree.mkdir()
    for i in range(5000): (tree/f'f{i:04d}.txt').write_bytes(b'fixture\n')
    raw=os.fsencode(tree)+b'/f12-\xff-native.txt'; pathlib.Path(os.fsdecode(raw)).write_bytes(b'native')
    try:
        address=env['DBUS_SESSION_BUS_ADDRESS'];env['DBUS_SYSTEM_BUS_ADDRESS']=address
        log=open(out/'gtk-filed.log','w');logs.append(log)
        filed=subprocess.Popen([str(args.filed.resolve()),'--index-root',str(tree),'--index-db',str(root/'cache/index.sqlite3'),'--state-db',str(root/'state/session.db'),'--reserve-percent','0'],env=env,stdout=log,stderr=log);processes.append(filed)
        conn=Gio.DBusConnection.new_for_address_sync(address,Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT|Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,None,None)
        deadline=time.monotonic()+30
        while True:
            assert filed.poll() is None
            try:
                roots=json.loads(call(conn,'Roots')[0])
                if roots['roots'] and all(r['state']=='current' for r in roots['roots']): break
            except GLib.Error: pass
            assert time.monotonic()<deadline,'root startup deadline';time.sleep(.1)
        monitor_log=open(out/'index-calls.log','w');logs.append(monitor_log)
        monitor=subprocess.Popen(['/usr/bin/dbus-monitor',"type='method_call',interface='org.templaros.Files.Index1',member='Search'"],env=env,stdout=monitor_log,stderr=monitor_log);processes.append(monitor)
        log=open(out/'xvfb.log','w');logs.append(log)
        display_read, display_write=os.pipe()
        xvfb=subprocess.Popen([args.xvfb,'-displayfd',str(display_write),'-screen','0','1024x768x24','-nolisten','tcp','-ac','-noreset'],pass_fds=(display_write,),env=env,stdout=log,stderr=log);processes.append(xvfb);os.close(display_write)
        with os.fdopen(display_read) as stream: env['DISPLAY']=':'+stream.readline().strip()
        log=open(out/'gtk-fsearch.log','w');logs.append(log)
        trace=out/'gtk-getdents.log'
        search=subprocess.Popen([args.strace,'-f','-yy','-o',str(trace),'-e','trace=execve,getdents64,inotify_init,inotify_init1,inotify_add_watch,fanotify_init',str(args.fsearch.resolve()),'--search','f12'],env=env,stdout=log,stderr=log);processes.append(search)
        deadline=time.monotonic()+5
        while app_pid is None:
            assert search.poll() is None,(out/'gtk-fsearch.log').read_text()
            children=[int(pid) for pid in pathlib.Path(f'/proc/{search.pid}/task/{search.pid}/children').read_text().split()]
            if children:
                assert len(children)==1,children
                app_pid=children[0]
                app_identity=pathlib.Path(f'/proc/{app_pid}/stat').read_text().rsplit(')',1)[1].split()[19]
                break
            assert time.monotonic()<deadline,'traced application startup deadline'
            time.sleep(.05)
        probe=subprocess.run([str(args.probe.resolve()),str(tree)],env=env,capture_output=True,text=True,timeout=75)
        (out/'remote-probe.log').write_text(probe.stdout+probe.stderr)
        report_probe=probe.returncode
        time.sleep(1)
        assert report_probe==0,(out/'remote-probe.log').read_text()
        assert search.poll() is None,(out/'gtk-fsearch.log').read_text()
        for query in ('f13','f12'):
            activation=subprocess.run([str(args.fsearch.resolve()),'--search',query],env=env,capture_output=True,text=True,timeout=10)
            assert activation.returncode==0,(query,activation.returncode,activation.stdout+activation.stderr)
            time.sleep(.5)
        assert pathlib.Path(f'/proc/{app_pid}').exists()
        sender=None;activations=[]
        for line in (out/'index-calls.log').read_text().splitlines():
            match=re.search(r'method call .* sender=(\S+)',line)
            if match: sender=match.group(1)
            if line.strip().startswith('string "{'):
                # dbus-monitor prints the JSON's inner quotes without escaping.
                request=json.loads(line.strip()[8:-1])
                if request.get('client_tag','').startswith('view-') and request.get('text') in ('f12','f13'):
                    activations.append({'sender':sender,'query':request['text']})
        primary={row['sender'] for row in activations if row['query']=='f12'}
        secondary={row['sender'] for row in activations if row['query']=='f13'}
        assert len(primary)==1 and primary==secondary,activations
        # Keep GDK's process-global display connection out of the fixture parent:
        # Xvfb is stopped in finally, before the parent's interpreter teardown.
        subprocess.run(['/usr/bin/python3','-c','''
import gi, sys
gi.require_version('Gdk','3.0')
from gi.repository import Gdk
display=Gdk.Display.open(sys.argv[1]);assert display is not None
window=display.get_default_screen().get_root_window()
pixels=Gdk.pixbuf_get_from_window(window,0,0,window.get_width(),window.get_height())
assert pixels.savev(sys.argv[2],'png',[],[])
display.close()
''',env['DISPLAY'],str(out/'fsearch-private.png')],env=env,check=True,timeout=10)
        pid=app_pid
        fdinfo={p.name:p.read_text() for p in pathlib.Path(f'/proc/{pid}/fdinfo').iterdir()}
        watches={name:text for name,text in fdinfo.items() if 'inotify' in text or 'fanotify' in text}
        (out/'gtk-watch-fds.json').write_text(json.dumps(watches,indent=2)+'\n')
        cursor,generation,text=call(conn,'Search','(s)',(json.dumps({'text':'f12','page_size':100,'fields':255}),)); result=json.loads(text)
        call(conn,'CloseCursor','(s)',(cursor,))
        report={'probe_returncode':report_probe,'generation':generation,'query_total':result['total'],'inotify_fanotify_fd_count':len(watches),'fsearch_pid':pid,'db_files':[str(p.relative_to(root)) for p in root.rglob('fsearch.db')], 'root':str(tree),'fsearch_log':(out/'gtk-fsearch.log').read_text()}
        assert not watches, watches
        assert not report['db_files'], report['db_files']
        assert 'Generation '+str(generation)+' ' in report['fsearch_log'], report['fsearch_log']
        assert 'CRITICAL' not in report['fsearch_log'], report['fsearch_log']
        indexed_entries=sum(root['entry_count'] for root in roots['roots'])
        assert indexed_entries>=5001,roots
        assert 'Index results visible: '+str(indexed_entries)+' indexed entries' in report['fsearch_log'],report['fsearch_log']
        report['indexed_entries']=indexed_entries
        allow_prefixes=['/etc/fonts/','/usr/share/fontconfig/','/usr/share/drirc.d',
            '/usr/lib/x86_64-linux-gnu/gio/modules','/var/lib/flatpak/exports/share/icons',
            '/usr/share/icons','/var/lib/snapd/desktop/icons','/usr/share/pixmaps',
            '/usr/share/glycin-loaders/','/usr/share/themes','/run/systemd/userdb']
        reads=[]
        trace_lines=trace.read_text().splitlines()
        owned_pids={int(line.split()[0]) for line in trace_lines if line.split() and line.split()[0].isdigit()}
        for line in trace_lines:
            match=re.search(r'getdents64\(\d+<([^>]+)>',line)
            if not match: continue
            path=match.group(1)
            allowed=any(path.startswith(prefix) for prefix in allow_prefixes)
            # Glycin's owned image-loader sandbox inspects only / and its own fd table.
            proc_fd=re.fullmatch(r'/proc/(\d+)/fd',path)
            allowed=allowed or path=='/' or bool(proc_fd and int(proc_fd.group(1)) in owned_pids)
            assert allowed,(path,line)
            assert not path.startswith(str(tree)),(path,line)
            reads.append(path)
        report['directory_reads']=sorted(set(reads))
        report['allowlist']=allow_prefixes+['/ (one level, image-loader sandbox)','/proc/<owned-loader-pid>/fd']
        report['gtk_input_method']='gtk-im-context-simple (no private IBus daemon)'
        report['single_instance_search_calls']=activations
        report['screenshot']='fsearch-private.png'
        report['binaries']={name:{'path':str(path.resolve()),'sha256':__import__('hashlib').sha256(path.read_bytes()).hexdigest()}
            for name,path in [('filed',args.filed),('fsearch',args.fsearch),('probe',args.probe)]}
        (out/'gtk-smoke.json').write_text(json.dumps(report,indent=2)+'\n')
        print(json.dumps(report,indent=2))
    finally:
        if app_pid:
            try:
                identity=pathlib.Path(f'/proc/{app_pid}/stat').read_text().rsplit(')',1)[1].split()[19]
                if identity==app_identity: os.kill(app_pid, signal.SIGTERM)
            except ProcessLookupError: pass
            except FileNotFoundError: pass
        for p in reversed(processes): stop(p)
        for log in logs: log.close()
