#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Deterministic protocol faults; this stub is never a live desktop service."""
import argparse, base64, collections, json, os, pathlib, subprocess, tempfile, time
from gi.repository import Gio, GLib

parser=argparse.ArgumentParser()
parser.add_argument('--probe',type=pathlib.Path,required=True)
parser.add_argument('--output',type=pathlib.Path,required=True)
args=parser.parse_args()
assert os.environ.get('TEMPLAR_WP14_PRIVATE_BUS')=='1'
out=args.output.resolve();out.mkdir(parents=True,exist_ok=True)
xml='''<node><interface name="org.templaros.Files.Index1">
<method name="Observe"><arg type="b" direction="in"/><arg type="t" direction="in"/></method>
<method name="Roots"><arg type="s" direction="out"/></method>
<method name="Search"><arg type="s" direction="in"/><arg type="s" direction="out"/><arg type="t" direction="out"/><arg type="s" direction="out"/></method>
<method name="FetchPage"><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="out"/></method>
<method name="CloseCursor"><arg type="s" direction="in"/></method>
</interface></node>'''
counts=collections.Counter(); calls=[]; cursors={}; generation=0; faults=set()
def rows(offset,count):
    return [{'row':{'entry_id':i+1,'path':{'bytes_b64':base64.b64encode(f'/fixture/f{i:04d}.txt'.encode()).decode(),'display':f'/fixture/f{i:04d}.txt'},'kind':0,'size_bytes':1,'mtime_ns':0},'present':True} for i in range(offset,min(offset+count,201))]
def method(conn,sender,path,interface,name,params,invocation):
    global generation
    values=params.unpack(); key=name
    if name=='Search': key=json.loads(values[0])['text']
    elif name=='FetchPage': key=cursors[values[0]][1]+':page'
    counts[key]+=1; calls.append({'method':name,'key':key,'time':time.monotonic()})
    def fail(kind,reason): invocation.return_dbus_error('org.templaros.Files.Index1.Error.'+kind,reason)
    if key=='fault_busy' and counts[key]==1: fail('Busy','Busy fixture');return
    if key=='fault_busy_exhaust': fail('Busy','Busy fixture exhausted');return
    if key=='fault_snapshot_search' and counts[key]==1: fail('SnapshotRequired','fixture cursor expired');return
    if key=='fault_unavailable': fail('Unavailable','fixture-offline');return
    if key=='fault_cancelled': fail('Cancelled','newer caller query');return
    if name=='Observe' or name=='CloseCursor': invocation.return_value(GLib.Variant('()',()));return
    if name=='Roots': invocation.return_value(GLib.Variant('(s)',(json.dumps({'cache_identity':'fault-fixture','roots':[]}),)));return
    if name=='Search':
        generation+=1; cursor=f'cursor-{generation}';cursors[cursor]=(generation,key)
        result={'cursor':cursor,'generation':generation,'total':201,'retained':201,'total_files':201,'total_folders':0,'roots':[],'truncated':False,'hits':rows(0,100)}
        invocation.return_value(GLib.Variant('(sts)',(cursor,generation,json.dumps(result))));return
    if name=='FetchPage':
        cursor,offset,count=values;gen,query=cursors[cursor]
        if query in ('fault_page','fault_selection') and query not in faults:
            faults.add(query);fail('SnapshotRequired','fixture pinned rows changed');return
        result={'cursor':cursor,'generation':gen,'offset':offset,'total':201,'dropped':0,'hits':rows(offset,count)}
        invocation.return_value(GLib.Variant('(s)',(json.dumps(result),)));return
    fail('Unavailable','unexpected fixture method')

with tempfile.TemporaryDirectory(prefix='templar-wp14-faults-') as temp:
    env=os.environ.copy();env['PATH']='/usr/bin:/bin';env['HOME']=temp
    for key in ('XDG_CONFIG_HOME','XDG_DATA_HOME','XDG_STATE_HOME','XDG_CACHE_HOME','XDG_RUNTIME_DIR'):
        path=pathlib.Path(temp)/key;path.mkdir(mode=0o700);env[key]=str(path)
    conn=Gio.DBusConnection.new_for_address_sync(env['DBUS_SESSION_BUS_ADDRESS'],Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT|Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,None,None)
    info=Gio.DBusNodeInfo.new_for_xml(xml)
    registration=conn.register_object('/org/templaros/Files',info.interfaces[0],method,None,None)
    reply=conn.call_sync('org.freedesktop.DBus','/org/freedesktop/DBus','org.freedesktop.DBus','RequestName',GLib.Variant('(su)',('org.templaros.Files',4)),None,Gio.DBusCallFlags.NONE,5000,None).unpack()
    assert reply==(1,),reply
    loop=GLib.MainLoop();log=open(out/'probe.log','w')
    probe=subprocess.Popen([str(args.probe.resolve()),'--faults'],env=env,stdout=log,stderr=log)
    start=time.monotonic()
    def poll():
        if probe.poll() is not None: loop.quit();return GLib.SOURCE_REMOVE
        if time.monotonic()-start>45: probe.kill();loop.quit();return GLib.SOURCE_REMOVE
        return GLib.SOURCE_CONTINUE
    GLib.timeout_add(20,poll)
    try: loop.run();probe.wait(timeout=5)
    finally:
        if probe.poll() is None: probe.kill();probe.wait(timeout=5)
        log.close();conn.unregister_object(registration);conn.close_sync(None)
    report={'returncode':probe.returncode,'counts':dict(counts),'calls':calls}
    (out/'faults.json').write_text(json.dumps(report,indent=2)+'\n')
    assert probe.returncode==0,(out/'probe.log').read_text()
    assert counts['fault_busy']==2 and counts['fault_busy_exhaust']==2
    busy=[call['time'] for call in calls if call['key']=='fault_busy']
    assert busy[1]-busy[0]>=.045,busy
    assert counts['fault_snapshot_search']==2 and counts['fault_page']>=2
    assert counts['fault_selection:page']==1 and counts['fault_unavailable']==1
    assert counts['fault_cancelled']==1 and counts['fault_pre_cancelled']==0
    assert 'CRITICAL' not in (out/'probe.log').read_text()
    print(json.dumps(report,indent=2))
