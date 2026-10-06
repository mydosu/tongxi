"""Physical project binding, live settings, execution and stop. CDP observes only."""
import argparse
import ctypes as C
from ctypes import wintypes as W
import hashlib
import json
import sys
import uuid
from pathlib import Path
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_native_input as native
from verify_stage3_native import choose_option
import verify_stage3 as prior
import verify_stage7_boundaries as integration
from stage7_evidence import begin_executable_run, finish_executable_run

PHYSICAL_REQUEST=integration.CORRECT.replace('Codex','DSH')
desktop.DATA=desktop.ARTIFACTS/('stage7-native-'+uuid.uuid4().hex[:10])
checks=[]

def check(name,condition=True):
    if not condition:raise AssertionError(name)
    checks.append(name);print('PASS '+name,flush=True)

def fill(control,selector,text):
    control.click(selector);control.hotkey(0x11,0x41);control.type(text)

def run():
    proc=None;success=False;executable_provenance=begin_executable_run(desktop.EXE);cursor=W.POINT();native.USER32.GetCursorPos(C.byref(cursor))
    root=desktop.ARTIFACTS/('stage7-native-fixture-'+uuid.uuid4().hex[:10]);root.mkdir()
    (root/'calc.py').write_text('def square(value):\n    return value\n',encoding='utf-8')
    (root/'check.py').write_text(integration.GOOD,encoding='utf-8')
    before=hashlib.md5((root/'check.py').read_bytes()).hexdigest();done=None
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch();browser=playwright.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
            page.on('pageerror',lambda error:native.errors.append(str(error)))
            control=native.NativeInput(proc,page)
            native.USER32.SetWindowPos(control.hwnd,None,20,120,min(1100,native.USER32.GetSystemMetrics(0)-60),min(1100,native.USER32.GetSystemMetrics(1)-160),0x0004)
            page.wait_for_timeout(200)
            rooms=desktop.ipc(page,'list_conversations',search='',archived=False);group=next(room for room in rooms if room['kind']=='group')
            control.click('[data-conversation="'+group['id']+'"]')
            control.click('#bind-project')
            fill(control,'#project-name','真实键鼠项目验收');fill(control,'#project-root',str(root));fill(control,'#check-program',sys.executable)
            fill(control,'#check-args','["check.py"]');control.hotkey(0x09);control.hotkey(0x09);control.hotkey(0x0D)
            expect(page.locator('#project-title')).to_contain_text('真实键鼠项目验收')
            check('physical mouse and Chinese keyboard bind actual project and fixed check',Path(desktop.ipc(page,'get_conversation',id=group['id'])['project']['root']).samefile(root))
            control.click('#connect-discussion')
            catalogs={key:prior.wait_connection(page,key) for key in ['hermes','codex','dsh']}
            check('physical mouse connects all technical group members',all(c['connection']=='connected' for c in catalogs.values()))
            choose_option(control,'#live-agent','hermes-win');choose_option(control,'#live-effort','none')
            choose_option(control,'#live-agent','codex-win');choose_option(control,'#live-effort','low')
            choose_option(control,'#live-agent','dsh-win');choose_option(control,'#live-effort','off')
            control.click('#message-input');control.type(PHYSICAL_REQUEST)
            expect(page.locator('#send-project')).to_be_enabled(timeout=10000);control.click('#send-project')
            done=integration.terminal(page,group['id'])
            check('physical execute button submits exact Chinese project request',done['request']==PHYSICAL_REQUEST and done['status']=='completed')
            check('physical workflow performs actual source write and three functional checks',any(c['path']=='calc.py' for c in done['changes']) and any(a['stage']=='verify' and a['checks'][0]['exit_code']==0 and '3 functional assertions passed' in a['checks'][0]['output'] for a in done['attempts']))
            implement=next(a for a in done['attempts'] if a['stage']=='implement')
            plans=[a for a in done['attempts'] if a['stage']=='plan']
            check('physical flow uses Codex proposal, Hermes dispatch and DSH off implementation',[(a['agent_id'],a['reasoning_effort']) for a in plans]==[('codex-win','low'),('hermes-win','none')] and implement['agent_id']=='dsh-win' and implement['reasoning_effort']=='off')
            check('actual fixed check script unchanged',hashlib.md5((root/'check.py').read_bytes()).hexdigest()==before)
            check('completed task board keeps technical JSON collapsed',all(not page.locator('[data-attempt-id="'+a['id']+'"]').evaluate('e => e.open') for a in done['attempts'] if a['stage'] in ['plan','review']))
            control.screenshot('stage7-native-project.png')
            control.click('#message-input');control.type(PHYSICAL_REQUEST);control.click('#send-project')
            running=integration.observe(page,group['id'],lambda j:any(a['stage']=='plan' and a['agent_id']=='codex-win' and a['status']=='running' and a['native_turn_id'] for a in j['attempts']))
            control.click('[data-stop-project="'+running['id']+'"]')
            stopped=integration.terminal(page,group['id'])
            check('physical mouse stops the second Codex planner before Hermes dispatch',stopped['status']=='interrupted' and len(stopped['attempts'])==1 and stopped['attempts'][0]['stage']=='plan' and stopped['attempts'][0]['agent_id']=='codex-win' and not stopped['tasks'])
            observed=page.evaluate('window.physicalInputs')
            check('all observed project input events are trusted',bool(observed) and all(event['trusted'] for event in observed))
            check('no JavaScript errors during physical project flows',not native.errors)
            owned=integration.tree(proc.pid)-{proc.pid}
            control.click('#window-close',wait=False);proc.wait(timeout=10);proc=None
            check('physical mouse closes frameless project window')
            check('physical app close cleans every observed owned Windows process',integration.wait_exit(owned));success=True
    finally:
        if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=10)
        native.USER32.SetCursorPos(cursor.x,cursor.y)
        report={'success':success,'passed':len(checks),'checks':checks,'system_input_action_count':len(native.actions),'actions':native.actions,'javascript_errors':native.errors,'test_data_directory':str(desktop.DATA),'project_root':str(root),'workflow':done,'input_method':'Win32 mouse and SendInput keyboard; CDP observation only',**finish_executable_run(desktop.EXE,executable_provenance)}
        (desktop.ARTIFACTS/'stage7-native-verification.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8')
    print('Physical stage7: '+str(len(checks))+' checks, '+str(len(native.actions))+' system inputs',flush=True)

if __name__=='__main__':
    parser=argparse.ArgumentParser(description='Run Stage 7 physical mouse and keyboard project verification.')
    parser.parse_args()
    run()
