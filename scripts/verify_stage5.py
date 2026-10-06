"""Packaged WSL Albion: real chat, isolation, resume and owned-process cleanup."""
import json
from pathlib import Path
import sqlite3
import subprocess
import time
import uuid
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_stage3 as prior
from verify_stage4 import settings
import probe_albion as probe

checks=[]; errors=[]; evidence=[]

def check(name,condition=True):
    if not condition: raise AssertionError(name)
    checks.append(name); print('PASS '+name,flush=True)

def wsl_json(code,*args):
    result=subprocess.run(['wsl','-d',probe.DISTRO,'--exec',probe.PYTHON,'-c',code,*map(str,args)],capture_output=True,text=True,encoding='utf-8',timeout=30,creationflags=subprocess.CREATE_NO_WINDOW)
    if result.returncode: raise RuntimeError('Safe WSL inspection failed')
    return json.loads(result.stdout)

def gateway_pids():
    return wsl_json("""import pathlib,json
inodes=set()
for name in ['/proc/net/tcp','/proc/net/tcp6']:
 for line in pathlib.Path(name).read_text().splitlines()[1:]:
  parts=line.split()
  if parts[1].split(':')[-1].lower()=='21ca' and parts[3]=='0A': inodes.add(parts[9])
pids=[]
for directory in pathlib.Path('/proc').iterdir():
 if not directory.name.isdigit(): continue
 try:
  if any(str(link.readlink()) in {'socket:['+inode+']' for inode in inodes} for link in (directory/'fd').iterdir()): pids.append(int(directory.name))
 except (OSError,PermissionError): pass
print(json.dumps(sorted(pids)))
""")

def native_info():
    namespace=json.loads((desktop.DATA/'albion-wsl.json').read_text(encoding='utf-8'))['namespace']
    path=Path('//wsl.localhost')/probe.DISTRO/'root/.local/share/local-agent-hub/instances'/namespace/'albion-native/runtime-info.json'
    return json.loads(path.read_text(encoding='utf-8'))

def wait_linux_exit(pid):
    deadline=time.monotonic()+12
    while time.monotonic()<deadline:
        if not wsl_json("import pathlib,json,sys; print(json.dumps(pathlib.Path('/proc/'+sys.argv[1]).exists()))",pid): return True
        time.sleep(.3)
    return False

def migration_source():
    historical=desktop.ARTIFACTS/'stage4-v0.4.0-verification.json'
    if not historical.exists(): historical=desktop.ARTIFACTS/'stage4-verification.json'
    report=json.loads(historical.read_text(encoding='utf-8'))
    source=Path(report['test_data_directory'])/'hub.db'
    desktop.DATA.mkdir(parents=True)
    with sqlite3.connect('file:'+source.as_posix()+'?mode=ro',uri=True) as old, sqlite3.connect(desktop.DATA/'hub.db') as new:
        if old.execute('PRAGMA user_version').fetchone()[0]!=4: raise AssertionError('Expected actual stage 4 database')
        baseline={name:old.execute('SELECT * FROM '+name+' ORDER BY rowid').fetchall() for name in ['messages','sessions','runs']}
        old.backup(new)
    return baseline

def run():
    proc=None; success=False; before=probe.profile_hashes(); gateway_before=gateway_pids(); old=migration_source()
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch(); browser=playwright.chromium.connect_over_cdp(endpoint); page=desktop.page_for(browser)
            page.on('pageerror',lambda error:errors.append(str(error)))
            with sqlite3.connect(desktop.DATA/'hub.db') as db:
                check('actual schema v4 upgrades to v5 without losing settings or native history',db.execute('PRAGMA user_version').fetchone()[0]==5 and all(db.execute('SELECT * FROM '+name+' ORDER BY rowid').fetchall()==rows for name,rows in old.items()) and not db.execute('PRAGMA foreign_key_check').fetchall())
            backups=list((desktop.DATA/'backups').glob('hub-schema-v4-*.db'))
            check('schema migration keeps one complete pre-upgrade SQLite backup',len(backups)==1)
            with sqlite3.connect('file:'+backups[0].as_posix()+'?mode=ro',uri=True) as db:
                check('pre-upgrade backup retains original schema, messages and session mappings',db.execute('PRAGMA user_version').fetchone()[0]==4 and all(db.execute('SELECT * FROM '+name+' ORDER BY rowid').fetchall()==rows for name,rows in old.items()))
            hermes=prior.new_private(page,'hermes-win','Windows 管家独立验收')
            page.locator('#connect-hermes').click(); prior.wait_connection(page,'hermes')
            albion=prior.new_private(page,'albion-wsl','阿尔比恩独立私聊')
            page.locator('#connect-albion').click(); catalog=prior.wait_connection(page,'albion')
            check('WSL Albion connects independently alongside Windows Hermes',bool(catalog['models']) and catalog['version']=='0.21.3' and desktop.ipc(page,'hermes_status')['connection']=='connected')
            settings(page,None,'low')
            marker='ALBION_'+uuid.uuid4().hex[:8]
            user=prior.send(page,'albion',f'不要调用工具。这是连接验收。先用一句话自我介绍，再原样输出标记 {marker}。然后逐行列出20句简短的晚安祝福。记住标记，以便稍后核对。')
            prior.wait_run(page,'albion',user,terminal=False)
            settings(page,None,'none')
            prior.select(page,hermes)
            marker_h='HERMES_'+uuid.uuid4().hex[:8]
            user_h=prior.send(page,'hermes',f'不调用工具，只回复 {marker_h}。')
            status_a=desktop.ipc(page,'albion_status')['active']; status_h=desktop.ipc(page,'hermes_status')['active']
            evidence.append({'simultaneous_states':[status_a['status'],status_h['status']]})
            check('Windows Hermes and WSL Albion can run separate turns simultaneously',status_a['status'] in ['starting','running','cancelling'] and status_h['status'] in ['starting','running','cancelling'])
            first=prior.wait_run(page,'albion',user); first_h=prior.wait_run(page,'hermes',user_h)
            check('real Albion reply uses her independent role and keeps current-turn settings',first['status']=='completed' and marker in first['text'] and '阿尔比恩' in first['text'] and first['reasoning_effort']=='low')
            info=native_info(); evidence.append(info)
            check('native system prompt includes the original Albion SOUL',info['identity_matched'] and info.get('relationship_matched') and info['soul_md5']==before[probe.PROFILE+'/SOUL.md'] and info['profile']==probe.PROFILE)
            check('Windows Hermes replies under its own native identity',first_h['status']=='completed' and marker_h in first_h['text'] and first_h['native_thread_id']!=first['native_thread_id'])
            prior.select(page,albion)
            second=prior.wait_run(page,'albion',prior.send(page,'albion','只回复刚才让我记住的验证标记。'))
            check('Albion applies live thinking changes to the next turn without losing context',second['native_thread_id']==first['native_thread_id'] and second['reasoning_effort']=='none' and marker in second['text'])
            other=prior.new_private(page,'albion-wsl','阿尔比恩隔离私聊')
            settings(page,None,'none')
            separate=prior.wait_run(page,'albion',prior.send(page,'albion','这个会话此前是否给过你验证标记？没有则只回复 ISOLATED。'))
            check('Albion private conversations have separate native context',separate['native_thread_id']!=first['native_thread_id'] and 'ISOLATED' in separate['text'] and marker not in separate['text'])
            prior.select(page,albion); page.locator('#message-input').fill('醒来后继续聊的中文草稿')
            pid=native_info()['pid']; proc.terminate(); proc.wait(timeout=10); proc=None
            check('closing the Windows app also terminates its owned WSL chat child',wait_linux_exit(pid))
            proc,endpoint=desktop.launch(); browser=playwright.chromium.connect_over_cdp(endpoint); page=desktop.page_for(browser)
            page.on('pageerror',lambda error:errors.append(str(error)))
            expect(page.locator('#message-input')).to_have_value('醒来后继续聊的中文草稿')
            expect(page.locator('#live-effort')).to_have_value('none')
            page.locator('#connect-albion').click(); prior.wait_connection(page,'albion')
            resumed=prior.wait_run(page,'albion',prior.send(page,'albion','只回复本会话最初让我记住的标记。'))
            check('Albion resumes the same native session after app restart',resumed['native_thread_id']==first['native_thread_id'] and marker in resumed['text'] and resumed['reasoning_effort']=='none')
            user=prior.send(page,'albion','这是中断验收，请从1开始逐行列出整数直到100000，不要停止。')
            prior.wait_run(page,'albion',user,terminal=False); page.locator('#cancel-albion').click(); cancelled=prior.wait_run(page,'albion',user)
            check('Albion cancellation is confirmed by the native harness',cancelled['status']=='interrupted')
            recovered=prior.wait_run(page,'albion',prior.send(page,'albion','只回复 ALBION_RECOVERED。'))
            check('Albion can reply after cancellation',recovered['status']=='completed' and 'ALBION_RECOVERED' in recovered['text'])
            page.screenshot(path=str(desktop.ARTIFACTS/'stage5-albion-chat.png'))
            # A process crash mid-turn must not leave a Linux worker or replay.
            user=prior.send(page,'albion','请从1开始逐行列出整数直到100000，这是进程恢复验收。')
            prior.wait_run(page,'albion',user,terminal=False); pid=native_info()['pid']
            proc.terminate(); proc.wait(timeout=10); proc=None
            check('app crash during a turn leaves no owned Linux worker',wait_linux_exit(pid))
            proc,endpoint=desktop.launch(); browser=playwright.chromium.connect_over_cdp(endpoint); page=desktop.page_for(browser)
            page.on('pageerror',lambda error:errors.append(str(error)))
            with sqlite3.connect(desktop.DATA/'hub.db') as db:
                check('unfinished Albion turn restores as interrupted without replay',db.execute('SELECT status FROM runs WHERE user_message_id=?',(user,)).fetchone()[0]=='interrupted')
            page.locator('#connect-albion').click(); prior.wait_connection(page,'albion')
            final=prior.wait_run(page,'albion',prior.send(page,'albion','只回复 ALBION_RESTARTED。'))
            check('Albion sends a fresh turn after crash recovery',final['status']=='completed' and 'ALBION_RESTARTED' in final['text'] and final['native_thread_id']==first['native_thread_id'])
            pid=native_info()['pid']; page.locator('#service-link').click(); page.locator('#service-albion-control').click(); prior.wait_connection(page,'albion','disconnected')
            check('service-page disconnect terminates only the owned WSL instance',wait_linux_exit(pid))
            check('original Albion gateway listener remains unchanged',gateway_pids()==gateway_before)
            check('original profile configuration, credentials and SOUL stay unchanged',probe.profile_hashes()==before)
            check('no JavaScript errors in real Albion flows',not errors)
            success=True
    finally:
        if proc and proc.poll() is None: proc.terminate(); proc.wait(timeout=10)
        after=probe.profile_hashes()
        report={'success':success,'passed':len(checks),'checks':checks,'javascript_errors':errors,'native_evidence':evidence,'profile_files_checked':len(before),'profile_files_changed':sum(after.get(p)!=h for p,h in before.items()),'gateway_pids_before':gateway_before,'gateway_pids_after':gateway_pids(),'test_data_directory':str(desktop.DATA),'input_method':'WebView2 CDP UI automation'}
        with sqlite3.connect(desktop.DATA/'hub.db') as db:
            report['new_real_run_counts']={agent:db.execute('SELECT COUNT(*) FROM runs WHERE agent_id=?',(agent,)).fetchone()[0]-sum(row[2]==agent for row in old['runs']) for agent in ['albion-wsl','hermes-win']}
        (desktop.ARTIFACTS/'stage5-verification.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8')
    print('Stage 5: '+str(len(checks))+' passed',flush=True)

if __name__=='__main__': run()
