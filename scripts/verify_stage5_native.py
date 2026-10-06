"""Real OS input: connect WSL Albion, change settings and send a real turn."""
import ctypes as C
from ctypes import wintypes as W
import json
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_native_input as native
from verify_stage3_native import choose_option
import verify_stage3 as prior
import verify_stage5 as integration
import probe_albion as probe

checks=[]
def check(name,condition=True):
    if not condition: raise AssertionError(name)
    checks.append(name); print('PASS '+name,flush=True)

def scroll_services(control):
    # Place the real pointer over the scrollable service panel before a wheel.
    control.click('.services-intro h2')
    cursor=W.POINT(); native.USER32.GetCursorPos(C.byref(cursor))
    control.pointer_guard(cursor.x,cursor.y)
    events=(native.Input*1)(); events[0].type=0
    events[0].mi.dwFlags=0x0800; events[0].mi.mouseData=(-600)&0xffffffff
    if native.USER32.SendInput(1,events,C.sizeof(native.Input))!=1: raise RuntimeError('Native wheel input failed')
    native.actions.append({'type':'mouse_wheel','target':'.services-content','delta':-600})
    control.page.wait_for_timeout(250)

def run():
    proc=None; success=False; cursor=W.POINT(); native.USER32.GetCursorPos(C.byref(cursor))
    before=probe.profile_hashes(); gateway=integration.gateway_pids()
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch(); browser=playwright.chromium.connect_over_cdp(endpoint); page=desktop.page_for(browser)
            page.on('pageerror',lambda error:native.errors.append(str(error)))
            control=native.NativeInput(proc,page)
            native.USER32.SetWindowPos(control.hwnd,None,20,120,min(1100,native.USER32.GetSystemMetrics(0)-60),min(1100,native.USER32.GetSystemMetrics(1)-160),0x0004)
            page.wait_for_timeout(200)
            room=next(r for r in desktop.ipc(page,'list_conversations',search='',archived=False) if r['members']==['albion-wsl'])
            control.click('[data-conversation="'+room['id']+'"]')
            expect(page.locator('#connect-albion')).to_be_visible()
            control.click('#connect-albion'); catalog=prior.wait_connection(page,'albion')
            check('real mouse connects the WSL Albion adapter',bool(catalog['models']))
            choose_option(control,'#live-model',catalog['default_model'])
            choose_option(control,'#live-effort','none')
            control.click('#message-input')
            saved=desktop.ipc(page,'get_conversation',id=room['id'])['sessions'][0]
            check('real keyboard selects the current private-session model and strength',saved['model']==catalog['default_model'] and saved['reasoning_effort']=='none')
            message='我准备睡觉了，请简单对我说一句晚安。'
            control.type(message)
            control.hotkey(0x11,0x10,0x0D)
            deadline=page.locator('#cancel-albion')
            expect(deadline).to_be_visible(timeout=10000)
            active=desktop.ipc(page,'albion_status')['active']
            reply=prior.wait_run(page,'albion',active['user_message_id'])
            delivered=desktop.ipc(page,'get_conversation',id=room['id'])['messages']
            check('Ctrl Shift Enter delivers exact Chinese input and receives a real reply',reply['status']=='completed' and bool(reply['text'].strip()) and any(m['id']==reply['user_message_id'] and m['content']==message and m['status']=='delivered' for m in delivered))
            check('voice-control metadata stays out of visible chat text','<|ACT:' not in reply['text'] and '<|ACT:' not in page.locator('#messages').inner_text())
            info=integration.native_info()
            check('the real reply uses the original Albion identity',info['identity_matched'] and info['soul_md5']==before[probe.PROFILE+'/SOUL.md'])
            control.click('#message-input'); control.type('保留中文草稿，醒来再聊。')
            control.click('#live-reset'); page.wait_for_timeout(250)
            saved=desktop.ipc(page,'get_conversation',id=room['id'])['sessions'][0]
            check('real mouse resets settings while retaining the Chinese draft',saved['model'] is None and saved['reasoning_effort'] is None and page.locator('#message-input').input_value()=='保留中文草稿，醒来再聊。')
            control.screenshot('stage5-native-albion.png')
            pid=info['pid']; control.click('#service-link'); scroll_services(control); control.click('#service-albion-control'); prior.wait_connection(page,'albion','disconnected')
            check('real mouse disconnects and cleans up the owned Linux child',integration.wait_linux_exit(pid))
            check('original profile and gateway remain unchanged',probe.profile_hashes()==before and integration.gateway_pids()==gateway)
            observed=page.evaluate('window.physicalInputs')
            check('all observed system input events are trusted',bool(observed) and all(e['trusted'] for e in observed))
            check('no JavaScript errors during physical Albion flows',not native.errors)
            control.click('#window-close',wait=False); proc.wait(timeout=10); proc=None
            check('real mouse closes the frameless app window')
            browser.close(); success=True
    finally:
        if proc and proc.poll() is None: proc.terminate(); proc.wait(timeout=10)
        native.USER32.SetCursorPos(cursor.x,cursor.y)
        (desktop.ARTIFACTS/'stage5-native-verification.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'system_input_action_count':len(native.actions),'actions':native.actions,'javascript_errors':native.errors,'test_data_directory':str(desktop.DATA),'model_requests':1,'input_method':'Win32 mouse and SendInput keyboard; CDP observation only'},ensure_ascii=False,indent=2),encoding='utf-8')
    print(f'Physical stage 5: {len(checks)} checks, {len(native.actions)} system inputs',flush=True)

if __name__=='__main__': run()
