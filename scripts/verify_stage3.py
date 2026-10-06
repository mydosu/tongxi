"""Packaged EXE integration: per-session settings, frameless window and real agents.

Uses CDP UI automation and read-only Win32 window inspection. Never reads keys
or prints raw model/provider errors. Data is isolated under artifacts.
"""
import ctypes as C
from ctypes import wintypes as W
import json
import os
from pathlib import Path
import sqlite3
import time
import uuid
from playwright.sync_api import sync_playwright, expect
from PIL import ImageGrab
import smoke_desktop as desktop

checks = []
errors = []
events = []
native_contexts = []

def check(name, condition=True):
    if not condition: raise AssertionError(name)
    checks.append(name)
    print('PASS ' + name, flush=True)

def ipc(page, command, **args): return desktop.ipc(page, command, **args)

def verify_codex_context(run):
    home=Path(os.environ.get('CODEX_HOME',Path.home()/'.codex'))
    # Read ONLY the just-created test thread, projecting model/effort fields.
    paths=list((home/'sessions').rglob('*'+run['native_thread_id']+'*.jsonl'))
    if len(paths)!=1: raise AssertionError('Native test thread evidence unavailable')
    contexts=[]
    for line in paths[0].read_text(encoding='utf-8').splitlines():
        item=json.loads(line)
        if item.get('type')=='turn_context':
            payload=item['payload']
            if payload.get('turn_id')==run['native_turn_id']:
                contexts.append({key:payload.get(key) for key in ['turn_id','model','effort']})
    if not contexts or contexts[-1]['model']!=run['model'] or contexts[-1]['effort']!=run['reasoning_effort']:
        raise AssertionError('Native turn settings differ from selected settings')
    native_contexts.append(contexts[-1])

def wait_connection(page, key, state='connected'):
    deadline=time.monotonic()+90
    while time.monotonic()<deadline:
        snapshot=ipc(page,key+'_status')
        if snapshot['connection']==state: return snapshot
        if snapshot.get('error'): raise AssertionError(key+' native connection failed')
        page.wait_for_timeout(150)
    raise AssertionError(key+' connection timeout')

def wait_run(page, key, user_id, terminal=True):
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        run = ipc(page, key + '_status').get('active')
        if run and run['user_message_id'] == user_id:
            events.append({'agent':key,'status':run['status'],'characters':len(run['text'])})
            if terminal and run['status'] in ('completed','interrupted','failed'):
                if run['status'] == 'failed': raise AssertionError(key + ' native run failed')
                return run
            if not terminal and run.get('native_turn_id'): return run
        page.wait_for_timeout(150)
    raise AssertionError(key + ' native run timeout')

def new_private(page, agent_id, title):
    page.locator('#new-conversation').click()
    page.locator('[data-kind="direct"]').click()
    page.locator('#create-form [name="title"]').fill(title)
    page.locator(f'#create-form input[value="{agent_id}"]').check()
    page.locator('#create-form [type="submit"]').click()
    expect(page.locator('.chat-heading h1')).to_have_text(title)
    return page.evaluate("localStorage.getItem('hub.selected')")

def select(page, id):
    desktop.choose(page, id)
    page.wait_for_function("id => document.querySelector('[data-conversation].active')?.dataset.conversation === id", arg=id)

def configure(page, model, effort, agent=None):
    # 模型/强度现在只在成员那一行的内联控件里改，改完即存（防抖），没有弹窗与提交按钮。
    if agent and page.locator('#live-agent').count(): page.locator('#live-agent').select_option(agent)
    page.locator('#live-model').select_option(model or '')
    page.locator('#live-effort').select_option(effort or '')
    page.wait_for_timeout(900)

def send(page, key, content):
    page.locator('#message-input').fill(content)
    expect(page.locator('#send-' + key)).to_be_enabled(timeout=15000)
    page.locator('#send-' + key).click()
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        run = ipc(page, key + '_status').get('active')
        if run and run['conversation_id'] == page.evaluate("localStorage.getItem('hub.selected')") and run['status'] in ('starting','running','cancelling'):
            return run['user_message_id']
        page.wait_for_timeout(80)
    raise AssertionError(key + ' send did not start')

def native_window(pid):
    user = C.WinDLL('user32', use_last_error=True)
    user.GetWindowThreadProcessId.argtypes = [W.HWND,C.POINTER(W.DWORD)]
    user.IsWindowVisible.argtypes = [W.HWND]
    callback_type = C.WINFUNCTYPE(W.BOOL,W.HWND,W.LPARAM)
    found = []
    @callback_type
    def visit(hwnd, _):
        actual = W.DWORD(); user.GetWindowThreadProcessId(hwnd,C.byref(actual))
        if actual.value == pid and user.IsWindowVisible(hwnd): found.append(hwnd); return False
        return True
    user.EnumWindows.argtypes = [callback_type,W.LPARAM]
    user.EnumWindows(visit,0)
    if not found: raise AssertionError('No native app window')
    user.GetWindowLongPtrW.argtypes = [W.HWND,C.c_int]
    user.GetWindowLongPtrW.restype = C.c_ssize_t
    user.GetWindowRect.argtypes = [W.HWND,C.POINTER(W.RECT)]
    user.GetClientRect.argtypes = [W.HWND,C.POINTER(W.RECT)]
    user.ClientToScreen.argtypes = [W.HWND,C.POINTER(W.POINT)]
    return user,found[0]

def run():
    proc = None; success = False
    try:
        with sync_playwright() as playwright:
            proc,endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser); page.on('pageerror', lambda error: errors.append(str(error)))
            user,hwnd = native_window(proc.pid)
            outer=W.RECT(); client=W.RECT(); origin=W.POINT(0,0)
            user.GetWindowRect(hwnd,C.byref(outer)); user.GetClientRect(hwnd,C.byref(client)); user.ClientToScreen(hwnd,C.byref(origin))
            # Tauri keeps WS_CAPTION for native resize/shadow while suppressing
            # the non-client caption. Inspect actual client geometry instead.
            check('native Windows title bar is absent',origin.y-outer.top<12 and client.bottom>=outer.bottom-outer.top-20)
            ImageGrab.grab(bbox=(outer.left,outer.top,outer.right,outer.bottom)).save(desktop.ROOT/'artifacts/stage3-frameless-window.png')
            expect(page.locator('#window-minimize')).to_be_visible()
            expect(page.locator('#window-maximize')).to_be_visible()
            expect(page.locator('#window-close')).to_be_visible()
            check('integrated window controls are present')
            group = next(room for room in ipc(page,'list_conversations',search='',archived=False) if room['kind']=='group')
            select(page,group['id'])
            configure(page,'future-dsh-model','low','dsh-win')
            detail = ipc(page,'get_conversation',id=group['id'])
            check('group member settings do not change other members', next(s for s in detail['sessions'] if s['agent_id']=='dsh-win')['model']=='future-dsh-model' and all(s['model'] is None for s in detail['sessions'] if s['agent_id']!='dsh-win'))
            albion = new_private(page,'albion-wsl','阿尔比恩待接入设置')
            configure(page,'future-albion-model','medium')
            check('unconnected member stores pending settings without pretending to send', ipc(page,'get_conversation',id=albion)['sessions'][0]['reasoning_effort']=='medium' and page.locator('#send-hermes, #send-codex').count()==0)
            codex = new_private(page,'codex-win','模型切换真实验收')
            page.locator('#connect-codex').click()
            wait_connection(page,'codex')
            catalog = ipc(page,'codex_status')
            print('Codex capabilities: '+json.dumps({'models':catalog['models'],'default_model':catalog['default_model'],'default_effort':catalog['default_effort']},ensure_ascii=False),flush=True)
            available = [model for model in catalog['models'] if 'low' in model['efforts']]
            chosen = next((model for model in available if model['id'] != catalog['default_model'] and 'luna' in model['id']), available[0])
            effort = 'low'
            configure(page,chosen['id'],effort)
            check('Codex model and effort controls use native model capabilities', len(catalog['models'])>0)
            nonce = 'PRIVATE_' + uuid.uuid4().hex[:8]
            message = send(page,'codex',f'不调用工具。记住标记 {nonce}，仅回复这个标记。')
            saved = ipc(page,'set_session_settings',id=codex,agentId='codex-win',model=None,reasoningEffort=None)
            check('backend accepts next-turn settings while preserving the admitted turn',saved['sessions'][0]['model'] is None)
            first = wait_run(page,'codex',message)
            check('real Codex request uses selected model and effort', first['status']=='completed' and nonce in first['text'] and first['model']==chosen['id'] and first['reasoning_effort']==effort)
            verify_codex_context(first)
            expect(page.locator('#send-codex')).to_be_enabled(timeout=15000)
            alternate = next((m for m in catalog['models'] if m['id']==catalog['default_model']), chosen)
            alternate_effort = 'medium' if 'medium' in alternate['efforts'] else alternate['efforts'][0]
            configure(page,alternate['id'],alternate_effort)
            second = wait_run(page,'codex',send(page,'codex','不调用工具。刚才让我记住的标记是什么？仅回复标记。'))
            check('changing models retains the original native thread and context', second['native_thread_id']==first['native_thread_id'] and nonce in second['text'] and second['model']==alternate['id'] and second['reasoning_effort']==alternate_effort)
            verify_codex_context(second)
            configure(page,None,None)
            third = wait_run(page,'codex',send(page,'codex','不调用工具，仅回复 DEFAULT_RESET_OK。'))
            check('reset restores effective local model and effort defaults',third['status']=='completed' and third['model']==catalog['default_model'] and third['reasoning_effort']==catalog['default_effort'] and 'DEFAULT_RESET_OK' in third['text'])
            verify_codex_context(third)
            check('native Codex turn-context evidence confirms all model changes',len(native_contexts)==3)
            hermes = new_private(page,'hermes-win','Hermes 真实私聊')
            page.locator('#connect-hermes').click()
            wait_connection(page,'hermes')
            state = ipc(page,'hermes_status')
            check('Hermes native ACP connection exposes real version and model inventory',state['version'] and len(state['models'])>0)
            configure(page,state['default_model'],'low')
            hermes_nonce = 'HERMES_REAL_' + uuid.uuid4().hex[:8]
            first_h = wait_run(page,'hermes',send(page,'hermes',f'请不调用工具，记住标记 {hermes_nonce}，仅回复这个标记。'))
            check('Hermes gives a real reply under session reasoning settings',first_h['status']=='completed' and hermes_nonce in first_h['text'] and first_h['reasoning_effort']=='low')
            expect(page.locator('#send-hermes')).to_be_enabled(timeout=15000)
            page.screenshot(path=str(desktop.ROOT/'artifacts/stage3-hermes-chat.png'))
            other_h = new_private(page,'hermes-win','Hermes 隔离会话')
            isolated = wait_run(page,'hermes',send(page,'hermes','请不调用工具。这是新的私聊。你是否知道我另一个私聊要求记住的随机标记？如果不知道，仅回复 ISOLATED。'))
            check('Hermes private rooms have distinct native sessions',isolated['native_thread_id']!=first_h['native_thread_id'] and hermes_nonce not in isolated['text'] and 'ISOLATED' in isolated['text'])
            proc.terminate(); proc.wait(timeout=10); proc=None
            time.sleep(0.7)
            proc,endpoint = desktop.launch(); browser = playwright.chromium.connect_over_cdp(endpoint); page=desktop.page_for(browser)
            page.on('pageerror',lambda error:errors.append(str(error)))
            select(page,hermes)
            check('settings survive EXE restart',ipc(page,'get_conversation',id=hermes)['sessions'][0]['reasoning_effort']=='low')
            page.locator('#connect-hermes').click()
            wait_connection(page,'hermes')
            resumed=wait_run(page,'hermes',send(page,'hermes','不调用工具。刚才让我记住的标记是什么？仅回复标记。'))
            check('Hermes reload restores its native session and context',resumed['native_thread_id']==first_h['native_thread_id'] and hermes_nonce in resumed['text'])
            cancel_message=send(page,'hermes','请从1到100000逐行写出数字，每行一个，持续输出。不要调用工具。')
            wait_run(page,'hermes',cancel_message,terminal=False)
            page.locator('#cancel-hermes').click()
            cancelled=wait_run(page,'hermes',cancel_message)
            check('Hermes cancel receives native cancelled result',cancelled['status']=='interrupted')
            expect(page.locator('#send-hermes')).to_be_enabled(timeout=20000)
            recovered=wait_run(page,'hermes',send(page,'hermes','不调用工具，仅回复 HERMES_RECOVERED。'))
            check('Hermes accepts a real next request after cancellation',recovered['status']=='completed' and 'HERMES_RECOVERED' in recovered['text'])
            expect(page.locator('#send-hermes')).to_be_enabled(timeout=15000)
            page.locator('#live-settings').screenshot(path=str(desktop.ROOT/'artifacts/stage3-model-settings.png'))
            page.locator('#service-link').click(); page.locator('#service-hermes-control').click()
            wait_connection(page,'hermes','disconnected')
            check('service controls disconnect Hermes without changing other agent configuration')
            check('no JavaScript errors in settings and native agent flows',not errors)
            # Only sessions/model metadata are queried; no credential tables or files.
            with sqlite3.connect(desktop.DATA/'hub.db') as db:
                check('schema stores native model evidence and both agents',db.execute('PRAGMA user_version').fetchone()[0]>=3 and set(r[0] for r in db.execute('SELECT DISTINCT agent_id FROM runs'))=={'codex-win','hermes-win'})
            page.locator('#window-close').click(); proc.wait(timeout=10); proc=None
            check('frameless close button exits the native app')
            browser.close(); success=True
    finally:
        if proc and proc.poll() is None: proc.terminate(); proc.wait(timeout=10)
        (desktop.ROOT/'artifacts/stage3-verification.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'events':events,'native_codex_contexts':native_contexts,'javascript_errors':errors,'test_data_directory':str(desktop.DATA),'input_method':'CDP UI automation plus read-only Win32 client geometry inspection'},ensure_ascii=False,indent=2),encoding='utf-8')
    print(f'Stage 3: {len(checks)} checks passed',flush=True)

if __name__=='__main__': run()
