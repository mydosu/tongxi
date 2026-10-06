"""Physical mouse/keyboard group discussion. CDP observes but does not act."""
import ctypes as C
from ctypes import wintypes as W
import json
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_native_input as native
from verify_stage3_native import choose_option
import verify_stage3 as prior
import verify_stage6 as integration

checks=[]
def check(name,condition=True):
    if not condition: raise AssertionError(name)
    checks.append(name);print('PASS '+name,flush=True)

def run():
    proc=None;success=False;cursor=W.POINT();native.USER32.GetCursorPos(C.byref(cursor))
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch();browser=playwright.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
            page.on('pageerror',lambda error:native.errors.append(str(error)))
            control=native.NativeInput(proc,page)
            native.USER32.SetWindowPos(control.hwnd,None,20,120,min(1100,native.USER32.GetSystemMetrics(0)-60),min(1100,native.USER32.GetSystemMetrics(1)-160),0x0004)
            page.wait_for_timeout(200)
            rooms=desktop.ipc(page,'list_conversations',search='',archived=False)
            group=next(room for room in rooms if room['kind']=='group')
            direct=next(room for room in rooms if room['members']==['dsh-win'])
            control.click('[data-conversation="'+group['id']+'"]')
            control.click('[data-discussion-member="codex-win"]')
            check('real mouse chooses just Hermes and DSH',page.locator('[data-discussion-member="hermes-win"]').is_checked() and page.locator('[data-discussion-member="dsh-win"]').is_checked() and not page.locator('[data-discussion-member="codex-win"]').is_checked())
            control.click('#connect-discussion');hermes=prior.wait_connection(page,'hermes');dsh=prior.wait_connection(page,'dsh')
            check('real mouse connects the selected group members',hermes['connection']=='connected' and dsh['connection']=='connected')
            choose_option(control,'#live-agent','hermes-win');choose_option(control,'#live-effort','none')
            choose_option(control,'#live-agent','dsh-win');choose_option(control,'#live-effort','off')
            choose_option(control,'#discussion-rounds','2');control.click('#message-input')
            message='请一起给这个本地待办应用提两条验收建议，后面的成员回应前一位，每次不超过50字。'
            control.type(message);control.hotkey(0x11,0x10,0x0D)
            expect(page.locator('#cancel-discussion')).to_be_visible(timeout=15000)
            finished=integration.wait_job(page,group['id'],lambda j:j['status'] in ['completed','interrupted','failed'])
            detail=desktop.ipc(page,'get_conversation',id=group['id']);texts={m['id']:m for m in detail['messages']}
            check('real Ctrl Shift Enter sends exact Chinese input to the group',texts[finished['user_message_id']]['content']==message and texts[finished['user_message_id']]['status']=='delivered')
            check('two selected members actually finish two discussion rounds',finished['status']=='completed' and len(finished['turns'])==4 and [(r['agent_id'],r['round']) for r in finished['turns']]==[('hermes-win',1),('dsh-win',1),('hermes-win',2),('dsh-win',2)] and all(texts[r['assistant_message_id']]['content'].strip() for r in finished['turns']))
            check('real keyboard sets separate group thinking strengths',finished['turns'][0]['reasoning_effort']=='none' and finished['turns'][1]['reasoning_effort']=='off')
            control.screenshot('stage6-native-group.png')
            control.click('#message-input');control.type('停止验收，请逐行列出1000条测试用例。不要调用工具。');control.hotkey(0x11,0x10,0x0D)
            running=integration.wait_job(page,group['id'],lambda j:j['status']=='running' and bool(j['turns']) and bool(j['turns'][0]['native_turn_id']))
            control.click('#cancel-discussion')
            stopped=integration.wait_job(page,group['id'],lambda j:j['status'] in ['completed','interrupted','failed'])
            check('real mouse stops the group before another member starts',stopped['id']==running['id'] and stopped['status']=='interrupted' and len(stopped['turns'])==1)
            control.click('#message-input');draft='中文群聊草稿保留，醒来再继续。';control.type(draft)
            control.click('[data-conversation="'+direct['id']+'"]');control.click('[data-conversation="'+group['id']+'"]')
            check('real room switching preserves the group draft and round selection',page.locator('#message-input').input_value()==draft and page.locator('#discussion-rounds').input_value()=='2')
            observed=page.evaluate('window.physicalInputs')
            check('all observed group input events are trusted',bool(observed) and all(event['trusted'] for event in observed))
            check('no JavaScript errors during physical group flows',not native.errors)
            control.click('#window-close',wait=False);proc.wait(timeout=10);proc=None
            check('real mouse closes the frameless group window')
            browser.close();success=True
    finally:
        if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=10)
        native.USER32.SetCursorPos(cursor.x,cursor.y)
        (desktop.ARTIFACTS/'stage6-native-verification.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'system_input_action_count':len(native.actions),'actions':native.actions,'javascript_errors':native.errors,'test_data_directory':str(desktop.DATA),'input_method':'Win32 mouse and SendInput keyboard; CDP observation only'},ensure_ascii=False,indent=2),encoding='utf-8')
    print(f'Physical stage 6: {len(checks)} checks, {len(native.actions)} system inputs',flush=True)

if __name__=='__main__':run()
