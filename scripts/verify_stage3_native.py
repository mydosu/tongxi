"""Actual OS input for frameless controls and session settings; no model calls."""
import ctypes as C
from ctypes import wintypes as W
import json
import time
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_native_input as native

checks=[]
def check(name, condition=True):
    if not condition: raise AssertionError(name)
    checks.append(name); print('PASS '+name,flush=True)

def bounds(hwnd):
    rect=W.RECT(); native.USER32.GetWindowRect(hwnd,C.byref(rect)); return rect

def mouse_button(flag):
    event=(native.Input*1)(); event[0].type=0; event[0].mi.dwFlags=flag
    if native.USER32.SendInput(1,event,C.sizeof(native.Input))!=1: raise RuntimeError('System mouse SendInput failed')

def drag(control, selector, dx, dy):
    control.focus_guard()
    box=control.page.locator(selector).bounding_box()
    origin=W.POINT(0,0); client=W.RECT()
    native.USER32.ClientToScreen(control.hwnd,C.byref(origin)); native.USER32.GetClientRect(control.hwnd,C.byref(client))
    viewport=control.page.evaluate('({width:innerWidth,height:innerHeight})')
    x=round(origin.x+(box['x']+box['width']/2)*client.right/viewport['width'])
    y=round(origin.y+(box['y']+box['height']/2)*client.bottom/viewport['height'])
    native.USER32.SetCursorPos(x,y)
    control.page.wait_for_timeout(60)
    control.pointer_guard(x,y)
    mouse_button(0x0002)
    try:
        control.page.wait_for_timeout(100)
        for step in range(1,9):
            control.focus_guard()
            native.USER32.SetCursorPos(x+dx*step//8,y+dy*step//8)
            control.page.wait_for_timeout(35)
    finally:
        mouse_button(0x0004)
    native.actions.append({'type':'system_mouse_drag','target':selector,'delta':[dx,dy]})
    control.page.wait_for_timeout(250)

def choose_option(control, selector, value):
    index=control.page.locator(selector).evaluate('(e,value)=>Array.from(e.options).findIndex(o=>o.value===value)',value)
    if index<0: raise AssertionError('Missing select option')
    control.click(selector); control.hotkey(0x24)
    for _ in range(index): control.hotkey(0x28)
    control.hotkey(0x0D)
    expect(control.page.locator(selector)).to_have_value(value)

def run():
    proc=None; success=False; old_cursor=W.POINT(); native.USER32.GetCursorPos(C.byref(old_cursor))
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch(); browser=playwright.chromium.connect_over_cdp(endpoint); page=desktop.page_for(browser)
            page.on('pageerror',lambda error:native.errors.append(str(error)))
            control=native.NativeInput(proc,page)
            # Place only the owned test window inside this portrait monitor.
            # Input guard must reject targets beyond the physical screen.
            width=min(1100,native.USER32.GetSystemMetrics(0)-60)
            height=min(1050,native.USER32.GetSystemMetrics(1)-120)
            native.USER32.SetWindowPos(control.hwnd,None,20,120,width,height,0x0004)
            page.wait_for_timeout(200)
            before=bounds(control.hwnd); drag(control,'.chat-heading',50,25); after=bounds(control.hwnd)
            print(json.dumps({'drag_before':[before.left,before.top,before.right,before.bottom],'drag_after':[after.left,after.top,after.right,after.bottom],'inputs':page.evaluate('window.physicalInputs.slice(-6)'),'toast':page.locator('#toast').inner_text()}),flush=True)
            check('system mouse drags the frameless window',abs(after.left-before.left)>=20 and abs(after.top-before.top)>=10)
            before=bounds(control.hwnd); drag(control,'.chat-heading',-50,-25)
            # Frameless windows retain native edge resizing.
            before=bounds(control.hwnd)
            drag(control,'[data-resize="East"]',48,0)
            after=bounds(control.hwnd)
            check('native edge resizing remains available',after.right-after.left>before.right-before.left+20)
            control.click('#window-maximize'); page.wait_for_timeout(300)
            native.USER32.IsZoomed.argtypes=[W.HWND]
            check('system mouse maximizes the native window',bool(native.USER32.IsZoomed(control.hwnd)))
            control.click('#window-maximize'); page.wait_for_timeout(300)
            check('system mouse restores the native window',not native.USER32.IsZoomed(control.hwnd))
            choose_option(control,'#live-agent','dsh-win')
            # 模型是下拉（不再能手填）：用系统键盘在下拉里选一项，强度选 high，改完即存。
            values=page.locator('#live-model').evaluate('e=>Array.from(e.options).map(o=>o.value)')
            picked=next((value for value in values if value),'')
            if picked: choose_option(control,'#live-model',picked)
            choose_option(control,'#live-effort','high')
            control.screenshot('stage3-native-settings.png')
            page.wait_for_timeout(1200)
            id=page.evaluate("localStorage.getItem('hub.selected')")
            saved=desktop.ipc(page,'get_conversation',id=id)
            settings=next(session for session in saved['sessions'] if session['agent_id']=='dsh-win')
            check('system keyboard saves per-member model and thinking strength',settings['model']==(picked or None) and settings['reasoning_effort']=='high')
            check('other group members retain their own settings',all(session['model'] is None for session in saved['sessions'] if session['agent_id']!='dsh-win'))
            choose_option(control,'#live-agent','dsh-win'); control.click('#live-reset'); page.wait_for_timeout(1200)
            saved=desktop.ipc(page,'get_conversation',id=id)
            settings=next(session for session in saved['sessions'] if session['agent_id']=='dsh-win')
            check('system mouse resets only the selected member to defaults',settings['model'] is None and settings['reasoning_effort'] is None)
            control.screenshot('stage3-native-frameless.png')
            control.click('#window-minimize'); page.wait_for_timeout(250)
            native.USER32.IsIconic.argtypes=[W.HWND]
            check('system mouse minimizes the native window',bool(native.USER32.IsIconic(control.hwnd)))
            native.USER32.ShowWindow(control.hwnd,9)
            control=native.NativeInput(proc,page)
            control.click('#window-close',wait=False); proc.wait(timeout=10); proc=None
            check('system mouse closes the native window')
            check('no JavaScript errors during physical input checks',not native.errors)
            browser.close(); success=True
    finally:
        if proc and proc.poll() is None: proc.terminate(); proc.wait(timeout=10)
        native.USER32.SetCursorPos(old_cursor.x,old_cursor.y)
        (desktop.ROOT/'artifacts/stage3-native-verification.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'system_input_action_count':len(native.actions),'actions':native.actions,'javascript_errors':native.errors,'test_data_directory':str(desktop.DATA),'input_method':'Win32 mouse and SendInput Unicode/scan-code keyboard; CDP observation only'},ensure_ascii=False,indent=2),encoding='utf-8')
    print(f'Physical stage 3: {len(checks)} checks, {len(native.actions)} system inputs',flush=True)

if __name__=='__main__': run()
