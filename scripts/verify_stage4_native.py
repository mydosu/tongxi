"""Inline controls using real Windows input; CDP only observes. No model calls."""
import ctypes as C
from ctypes import wintypes as W
import json
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_native_input as native
from verify_stage3_native import choose_option

checks=[]

def check(name, condition=True):
    if not condition: raise AssertionError(name)
    checks.append(name); print('PASS '+name,flush=True)

def run():
    proc=None; success=False; cursor=W.POINT()
    native.USER32.GetCursorPos(C.byref(cursor))
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch()
            browser=playwright.chromium.connect_over_cdp(endpoint); page=desktop.page_for(browser)
            page.on('pageerror',lambda error:native.errors.append(str(error)))
            control=native.NativeInput(proc,page)
            native.USER32.SetWindowPos(control.hwnd,None,20,120,min(1100,native.USER32.GetSystemMetrics(0)-60),min(1100,native.USER32.GetSystemMetrics(1)-160),0x0004)
            page.wait_for_timeout(250)
            choose_option(control,'#live-agent','dsh-win')
            draft='保留我的中文草稿'
            control.click('#message-input'); control.type(draft)
            control.click('#live-model'); control.hotkey(0x11,0x41); control.type('DSH-系统输入验收')
            choose_option(control,'#live-effort','high')
            page.wait_for_timeout(350)
            id=page.evaluate("localStorage.getItem('hub.selected')")
            saved=desktop.ipc(page,'get_conversation',id=id)
            session=next(s for s in saved['sessions'] if s['agent_id']=='dsh-win')
            check('real mouse and keyboard save inline model and effort',session['model']=='DSH-系统输入验收' and session['reasoning_effort']=='high')
            check('inline settings keep other members independent',all(s['model'] is None and s['reasoning_effort'] is None for s in saved['sessions'] if s['agent_id']!='dsh-win'))
            check('inline edits retain the Chinese message draft',page.locator('#message-input').input_value()==draft)
            check('model switching needs no settings dialog',not page.locator('#modal').is_visible())
            control.screenshot('stage4-native-inline.png')
            control.click('#live-reset'); page.wait_for_timeout(350)
            saved=desktop.ipc(page,'get_conversation',id=id)
            session=next(s for s in saved['sessions'] if s['agent_id']=='dsh-win')
            check('real mouse resets inline settings and keeps the draft',session['model'] is None and session['reasoning_effort'] is None and page.locator('#message-input').input_value()==draft)
            # Reject a displaced cursor before emitting any mouse-down.
            native.USER32.SetCursorPos(0,0)
            rejected=False
            try: control.pointer_guard(20,150)
            except RuntimeError: rejected=True
            check('pointer guard rejects an unexpected cursor position',rejected)
            events=page.evaluate('window.physicalInputs')
            check('observed mouse and keyboard events are trusted',bool(events) and all(e['trusted'] for e in events))
            check('no JavaScript errors during physical inline checks',not native.errors)
            browser.close(); success=True
    finally:
        if proc and proc.poll() is None: proc.terminate(); proc.wait(timeout=10)
        native.USER32.SetCursorPos(cursor.x,cursor.y)
        (desktop.ARTIFACTS/'stage4-native-verification.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'system_input_action_count':len(native.actions),'actions':native.actions,'javascript_errors':native.errors,'test_data_directory':str(desktop.DATA),'input_method':'Win32 mouse and SendInput keyboard; CDP observation only'},ensure_ascii=False,indent=2),encoding='utf-8')
    print(f'Physical stage 4: {len(checks)} checks, {len(native.actions)} system inputs',flush=True)

if __name__=='__main__': run()
