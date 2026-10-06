"""Real Windows mouse/keyboard verification of the packaged app and Codex.

Playwright/CDP is used ONLY for observation, element coordinates and read-only
state checks. All UI actions use Win32 cursor/mouse events and SendInput.
Sends a few small real model requests using the existing native Codex login.
"""
from __future__ import annotations

import ctypes as C
from ctypes import wintypes as W
import json
from pathlib import Path
import time
import uuid

from PIL import ImageGrab
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop

USER32 = C.WinDLL('user32', use_last_error=True)
KERNEL32 = C.WinDLL('kernel32', use_last_error=True)
USER32.SetProcessDpiAwarenessContext.argtypes = [C.c_void_p]
USER32.SetProcessDpiAwarenessContext(C.c_void_p(-4))
USER32.GetForegroundWindow.restype = W.HWND
USER32.SetForegroundWindow.argtypes = [W.HWND]
USER32.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
USER32.GetWindowThreadProcessId.restype = W.DWORD
USER32.IsWindowVisible.argtypes = [W.HWND]
USER32.ShowWindow.argtypes = [W.HWND, C.c_int]
USER32.GetClientRect.argtypes = [W.HWND, C.POINTER(W.RECT)]
USER32.GetWindowRect.argtypes = [W.HWND, C.POINTER(W.RECT)]
USER32.GetWindowTextW.argtypes = [W.HWND, W.LPWSTR, C.c_int]
USER32.SetWindowPos.argtypes = [W.HWND, W.HWND, C.c_int, C.c_int, C.c_int, C.c_int, W.UINT]
USER32.WindowFromPoint.argtypes = [W.POINT]
USER32.WindowFromPoint.restype = W.HWND
USER32.GetAncestor.argtypes = [W.HWND, W.UINT]
USER32.GetAncestor.restype = W.HWND
USER32.ClientToScreen.argtypes = [W.HWND, C.POINTER(W.POINT)]
USER32.AttachThreadInput.argtypes = [W.DWORD, W.DWORD, W.BOOL]
USER32.SetCursorPos.argtypes = [C.c_int, C.c_int]
USER32.mouse_event.argtypes = [W.DWORD, W.DWORD, W.DWORD, W.DWORD, C.c_size_t]
USER32.keybd_event.argtypes = [W.BYTE, W.BYTE, W.DWORD, C.c_size_t]


class MouseInput(C.Structure):
    _fields_ = [('dx', W.LONG), ('dy', W.LONG), ('mouseData', W.DWORD), ('dwFlags', W.DWORD), ('time', W.DWORD), ('dwExtraInfo', C.c_size_t)]


class KeyInput(C.Structure):
    _fields_ = [('wVk', W.WORD), ('wScan', W.WORD), ('dwFlags', W.DWORD), ('time', W.DWORD), ('dwExtraInfo', C.c_size_t)]


class HardwareInput(C.Structure):
    _fields_ = [('uMsg', W.DWORD), ('wParamL', W.WORD), ('wParamH', W.WORD)]


class InputUnion(C.Union):
    _fields_ = [('mi', MouseInput), ('ki', KeyInput), ('hi', HardwareInput)]


class Input(C.Structure):
    _anonymous_ = ('u',)
    _fields_ = [('type', W.DWORD), ('u', InputUnion)]


USER32.SendInput.argtypes = [W.UINT, C.POINTER(Input), C.c_int]
USER32.SendInput.restype = W.UINT

class ProcessEntry(C.Structure):
    _fields_ = [('dwSize',W.DWORD),('cntUsage',W.DWORD),('th32ProcessID',W.DWORD),('th32DefaultHeapID',C.c_size_t),('th32ModuleID',W.DWORD),('cntThreads',W.DWORD),('th32ParentProcessID',W.DWORD),('pcPriClassBase',W.LONG),('dwFlags',W.DWORD),('szExeFile',W.WCHAR * 260)]

KERNEL32.CreateToolhelp32Snapshot.argtypes = [W.DWORD, W.DWORD]
KERNEL32.CreateToolhelp32Snapshot.restype = W.HANDLE
KERNEL32.Process32FirstW.argtypes = [W.HANDLE, C.POINTER(ProcessEntry)]
KERNEL32.Process32NextW.argtypes = [W.HANDLE, C.POINTER(ProcessEntry)]
KERNEL32.CloseHandle.argtypes = [W.HANDLE]

def process_name(pid):
    handle = KERNEL32.CreateToolhelp32Snapshot(2, 0)
    if handle == C.c_void_p(-1).value:
        return ''
    try:
        entry = ProcessEntry()
        entry.dwSize = C.sizeof(entry)
        more = KERNEL32.Process32FirstW(handle, C.byref(entry))
        while more:
            if entry.th32ProcessID == pid:
                return entry.szExeFile.lower()
            more = KERNEL32.Process32NextW(handle, C.byref(entry))
        return ''
    finally:
        KERNEL32.CloseHandle(handle)

def system_hotkey(keys):
    USER32.MapVirtualKeyW.argtypes = [W.UINT, W.UINT]
    inputs = (Input * (len(keys) * 2))()
    for index, key in enumerate(keys):
        inputs[index].type = 1
        inputs[index].ki = KeyInput(0, USER32.MapVirtualKeyW(key, 0), 0x0008, 0, 0)
    for index, key in enumerate(reversed(keys), len(keys)):
        inputs[index].type = 1
        inputs[index].ki = KeyInput(0, USER32.MapVirtualKeyW(key, 0), 0x0008 | 0x0002, 0, 0)
    if USER32.SendInput(len(inputs), inputs, C.sizeof(Input)) != len(inputs):
        raise RuntimeError('Keyboard scan-code SendInput failed')

def mouse_click():
    inputs = (Input * 2)()
    inputs[0].type = inputs[1].type = 0
    inputs[0].mi.dwFlags = 0x0002
    inputs[1].mi.dwFlags = 0x0004
    if USER32.SendInput(2, inputs, C.sizeof(Input)) != 2:
        raise RuntimeError('Native mouse SendInput failed')
checks = []
actions = []
errors = []
events = []


def check(name, condition=True):
    if not condition:
        raise AssertionError(name)
    checks.append(name)
    print('PASS ' + name, flush=True)


class NativeInput:
    def __init__(self, process, page):
        self.pid = process.pid
        self.page = page
        self.hwnd = None
        page.evaluate("""() => { window.physicalInputs = []; for (const kind of ['mousedown','mouseup','keydown','input']) document.addEventListener(kind, e => window.physicalInputs.push({kind, trusted:e.isTrusted, target:e.target.id, x:e.clientX,y:e.clientY,key:e.key,ctrl:e.ctrlKey,composing:e.isComposing}), true); }""")
        callback_type = C.WINFUNCTYPE(W.BOOL, W.HWND, W.LPARAM)

        @callback_type
        def visit(hwnd, _):
            pid = W.DWORD()
            USER32.GetWindowThreadProcessId(hwnd, C.byref(pid))
            title = C.create_unicode_buffer(256)
            if pid.value == self.pid:
                USER32.GetWindowTextW(hwnd, title, 256)
            if pid.value == self.pid and USER32.IsWindowVisible(hwnd) and 'Agent Hub' in title.value:
                self.hwnd = hwnd
                return False
            return True

        USER32.EnumWindows.argtypes = [callback_type, W.LPARAM]
        USER32.EnumWindows(visit, 0)
        if not self.hwnd:
            raise RuntimeError('No visible app window; physical input cannot be verified')
        USER32.ShowWindow(self.hwnd, 9)  # SW_RESTORE
        foreground = USER32.GetForegroundWindow()
        target_thread = USER32.GetWindowThreadProcessId(foreground, None)
        current_thread = KERNEL32.GetCurrentThreadId()
        attached = USER32.AttachThreadInput(current_thread, target_thread, True) if target_thread != current_thread else False
        try:
            USER32.SetForegroundWindow(self.hwnd)
        finally:
            if attached:
                USER32.AttachThreadInput(current_thread, target_thread, False)
        self.page.wait_for_timeout(200)
        # Windows may reject programmatic foreground activation. A guarded title
        # bar click establishes foreground through actual user input instead.
        foreground_pid = W.DWORD()
        USER32.GetWindowThreadProcessId(USER32.GetForegroundWindow(), C.byref(foreground_pid))
        if process_name(foreground_pid.value) == 'listary.exe':
            system_hotkey([0x1B])
            actions.append({'type':'keyboard_hotkey','keys':[0x1B], 'target':'dismiss Listary popup'})
            self.page.wait_for_timeout(150)
            USER32.SetForegroundWindow(self.hwnd)
            self.page.wait_for_timeout(150)
            USER32.GetWindowThreadProcessId(USER32.GetForegroundWindow(), C.byref(foreground_pid))
        print(json.dumps({'app_pid':self.pid,'app_hwnd':self.hwnd,'foreground_pid':foreground_pid.value,'foreground_hwnd':USER32.GetForegroundWindow()}), flush=True)
        if foreground_pid.value != self.pid:
            USER32.SetWindowPos(self.hwnd, W.HWND(-1), 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0040)
            try:
                self.page.wait_for_timeout(100)
                rect = W.RECT()
                USER32.GetWindowRect(self.hwnd, C.byref(rect))
                point = W.POINT((rect.left + rect.right) // 2, rect.top + 18)
                USER32.SetCursorPos(point.x, point.y)
                self.page.wait_for_timeout(60)
                self.pointer_guard(point.x, point.y, require_foreground=False)
                mouse_click()
                actions.append({'type':'mouse_click', 'target':'native app title bar'})
                self.page.wait_for_timeout(100)
            finally:
                USER32.SetWindowPos(self.hwnd, W.HWND(-2), 0, 0, 0, 0, 0x0001 | 0x0002)
            USER32.GetWindowThreadProcessId(USER32.GetForegroundWindow(), C.byref(foreground_pid))
            print(json.dumps({'title_click': [point.x, point.y], 'foreground_pid_after_click':foreground_pid.value,'foreground_hwnd':USER32.GetForegroundWindow(),'inputs':page.evaluate('window.physicalInputs')}),flush=True)
        self.focus_guard()

    def focus_guard(self):
        pid = W.DWORD()
        USER32.GetWindowThreadProcessId(USER32.GetForegroundWindow(), C.byref(pid))
        if pid.value != self.pid:
            raise RuntimeError(f'App lost foreground (wanted={self.pid},actual={pid.value}); stopped physical input to protect other windows')

    def pointer_guard(self, x, y, require_foreground=True):
        if require_foreground:
            self.focus_guard()
        actual = W.POINT()
        if not USER32.GetCursorPos(C.byref(actual)) or abs(actual.x-x)>1 or abs(actual.y-y)>1:
            raise RuntimeError('Cursor did not reach the app target; stopped physical input')
        root = USER32.GetAncestor(USER32.WindowFromPoint(actual), 2)
        root_value = root.value if hasattr(root, "value") else int(root or 0)
        target_value = self.hwnd.value if hasattr(self.hwnd, "value") else int(self.hwnd or 0)
        if root_value != target_value:
            raise RuntimeError('App target is covered by another window; stopped physical input before mouse down')

    def click(self, selector, wait=True):
        self.focus_guard()
        element = self.page.locator(selector)
        expect(element).to_be_visible()
        box = element.bounding_box()
        origin = W.POINT(0, 0)
        rect = W.RECT()
        USER32.ClientToScreen(self.hwnd, C.byref(origin))
        USER32.GetClientRect(self.hwnd, C.byref(rect))
        viewport = self.page.evaluate('({width: innerWidth, height: innerHeight})')
        x = round(origin.x + (box['x'] + box['width'] / 2) * rect.right / viewport['width'])
        y = round(origin.y + (box['y'] + box['height'] / 2) * rect.bottom / viewport['height'])
        if not (origin.x <= x < origin.x + rect.right and origin.y <= y < origin.y + rect.bottom):
            raise RuntimeError('Target is outside the app client area')
        USER32.SetCursorPos(x, y)
        self.page.wait_for_timeout(60)
        hovered = self.page.evaluate("[...document.querySelectorAll(':hover')].map(e => e.id || e.tagName).slice(-4)")
        print(json.dumps({'click': selector, 'xy': [x,y], 'client': [origin.x,origin.y,rect.right,rect.bottom], 'viewport': viewport, 'box': box, 'hover': hovered}), flush=True)
        self.pointer_guard(x, y)
        mouse_click()
        actions.append({'type': 'mouse_click', 'target': selector})
        if wait:
            self.page.wait_for_timeout(100)

    def type(self, text):
        self.focus_guard()
        encoded = text.encode('utf-16-le')
        for index in range(0, len(encoded), 2):
            self.focus_guard()
            scan = int.from_bytes(encoded[index:index + 2], 'little')
            inputs = (Input * 2)()
            inputs[0].type = inputs[1].type = 1
            inputs[0].ki = KeyInput(0, scan, 0x0004, 0, 0)
            inputs[1].ki = KeyInput(0, scan, 0x0004 | 0x0002, 0, 0)
            if USER32.SendInput(2, inputs, C.sizeof(Input)) != 2:
                raise RuntimeError('SendInput failed')
            self.page.wait_for_timeout(2)
        actions.append({'type': 'unicode_keyboard_input', 'characters': len(text)})

    def hotkey(self, *keys):
        self.focus_guard()
        system_hotkey(keys)
        actions.append({'type': 'keyboard_hotkey', 'keys': list(keys)})
        self.page.wait_for_timeout(120)
        print('Keyboard observations: ' + json.dumps(self.page.evaluate('window.physicalInputs.slice(-8)'),ensure_ascii=False),flush=True)

    def screenshot(self, name):
        self.focus_guard()
        rect = W.RECT()
        USER32.GetWindowRect(self.hwnd, C.byref(rect))
        ImageGrab.grab(bbox=(rect.left, rect.top, rect.right, rect.bottom), all_screens=True).save(desktop.ARTIFACTS / name)


def observe_stream(page):
    page.evaluate("""() => {
      window.nativeObservations = [];
      new MutationObserver(() => {
        const text = document.querySelector('.agent-message:last-child .bubble')?.textContent || '';
        if (text && text !== window.nativeObservations.at(-1)) window.nativeObservations.push(text);
      }).observe(document.querySelector('#main'), {childList:true, subtree:true, characterData:true});
    }""")


def wait_run(page, room, terminal=True, timeout=150):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        state = desktop.ipc(page, 'codex_status')
        run = state.get('active')
        if run and run['conversation_id'] == room:
            events.append({'status': run['status'], 'characters': len(run['text'])})
            if terminal and run['status'] in ('completed', 'interrupted', 'failed'):
                if run['status'] == 'failed':
                    raise RuntimeError(run.get('error') or 'Native model request failed')
                return run
            if not terminal and run.get('native_turn_id'):
                return run
        page.wait_for_timeout(120)
    raise RuntimeError('Native run timeout')


def new_private(control, title):
    page = control.page
    control.click('#new-conversation')
    control.click('[data-kind="direct"]')
    control.click('#create-form [name="title"]')
    control.type(title)
    expect(page.locator('#create-form [name="title"]')).to_have_value(title)
    control.click('#create-form [type="submit"]')
    expect(page.locator('.chat-heading h1')).to_have_text(title)
    return page.evaluate("localStorage.getItem('hub.selected')")


def run(control_factory=NativeInput, report_name='native-input-verification.json', native=True):
    proc = None
    success = False
    old_cursor = W.POINT()
    USER32.GetCursorPos(C.byref(old_cursor))
    try:
        with sync_playwright() as playwright:
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on('pageerror', lambda error: errors.append(str(error)))
            control = control_factory(proc, page)
            control.screenshot('native-initial-window.png')
            room = new_private(control, '真实鼠标键盘验收')
            check('UI creates a private room and accepts Chinese input')
            control.click('#connect-codex')
            expect(page.locator('.connection-badge')).to_contain_text('接口已连接', timeout=45000)
            state = desktop.ipc(page, 'codex_status')
            check('native app-server handshake from the desktop connect control', state['connection'] == 'connected')
            check('native Codex version detected', state['version'] == '0.160.0')
            nonce = 'NATIVE_OK_' + uuid.uuid4().hex[:10]
            prompt = f'仅作文本回复，不调用工具。先输出 {nonce}，然后换行输出20个编号句子，每句只说“桌面验收正在进行”。'
            observe_stream(page)
            control.click('#message-input')
            control.type(prompt)
            expect(page.locator('#message-input')).to_have_value(prompt)
            control.hotkey(0x11, 0x10, 0x0D)  # Ctrl+Shift+Enter; Ctrl+Enter belongs to Listary
            expect(page.locator('#cancel-codex')).to_be_visible(timeout=15000)
            check('Ctrl+Shift+Enter submits to Codex without invoking Listary')
            expect(page.locator('#send-codex')).to_be_disabled()
            check('second send is disabled while a native turn is active')
            first = wait_run(page, room)
            check('real native reply completes and includes test nonce', first['status'] == 'completed' and nonce in first['text'])
            observations = page.evaluate('window.nativeObservations')
            check('desktop displays multiple incremental reply updates', len(set(observations)) >= 3)
            detail = desktop.ipc(page, 'get_conversation', id=room)
            check('real reply and delivered user message persist in SQLite', len(detail['messages']) == 2 and detail['messages'][0]['status'] == 'delivered' and detail['messages'][1]['status'] == 'completed')
            check('native thread and turn IDs are recorded', bool(first['native_thread_id']) and bool(first['native_turn_id']) and detail['sessions'][0]['native_session_id'] == first['native_thread_id'])
            expect(page.locator('.agent-message .message-delivery')).to_contain_text('回复完成')
            control.screenshot('native-keyboard-chat.png')

            other = new_private(control, '会话隔离验收')
            control.click('#message-input')
            control.type('仅作文本回复，不调用工具。本会话之前是否给过你验证标记？如果没有，只回复 ISOLATED。')
            control.click('#send-codex')
            second = wait_run(page, other)
            check('another private room has a distinct native thread', second['native_thread_id'] != first['native_thread_id'])
            check('another native room cannot recall the private nonce', 'ISOLATED' in second['text'] and nonce not in second['text'])

            control.click(f'[data-conversation="{room}"]')
            expect(page.locator('.chat-heading h1')).to_have_text('真实鼠标键盘验收')
            control.click('#message-input')
            control.type('重启后保留的中文输入')
            proc.terminate()
            proc.wait(timeout=10)
            proc = None
            time.sleep(0.8)
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on('pageerror', lambda error: errors.append(str(error)))
            control = control_factory(proc, page)
            expect(page.locator('#message-input')).to_have_value('重启后保留的中文输入')
            check('EXE restart restores Chinese draft and saved native reply', nonce in page.locator('.agent-message .bubble').inner_text())
            check('native session mapping survives real process restart', desktop.ipc(page, 'get_conversation', id=room)['sessions'][0]['native_session_id'] == first['native_thread_id'])
            control.click('#connect-codex')
            expect(page.locator('#send-codex')).to_be_enabled(timeout=45000)
            control.click('#message-input')
            control.hotkey(0x11, 0x41)  # Ctrl+A
            control.type('不调用任何工具。只回复我们这个会话第一条消息要求你首先输出的验证标记，不解释。')
            control.click('#send-codex')
            resumed = wait_run(page, room)
            check('thread/resume continues the same native thread', resumed['native_thread_id'] == first['native_thread_id'])
            check('resumed real model remembers prior private context', nonce in resumed['text'])

            control.click('#message-input')
            control.type('不调用工具。从1开始依次列出每个整数，一行一个，直到100000，不要解释。')
            control.click('#send-codex')
            wait_run(page, room, terminal=False)
            control.click('#conversation-menu')
            control.click('#archive-action')
            expect(page.locator('#toast')).to_contain_text('会话正在运行')
            check('archive is rejected while the turn is running')
            control.click('#modal-close')
            control.click('#cancel-codex')
            cancelled = wait_run(page, room, timeout=40)
            check('stop control gets native interrupted confirmation', cancelled['status'] == 'interrupted')
            expect(page.locator('#send-codex')).to_be_enabled()
            check('composer re-enables after confirmed cancellation')
            control.click('#message-input')
            control.type('不调用工具，仅回复 CANCEL_RECOVERED。')
            control.click('#send-codex')
            recovered = wait_run(page, room)
            check('new real request succeeds after cancellation', recovered['status'] == 'completed' and 'CANCEL_RECOVERED' in recovered['text'])
            control.screenshot('native-cancel-recover.png')
            control.click('#service-link')
            expect(page.locator('#service-codex-control')).to_have_text('断开 Codex')
            control.click('#service-codex-control')
            expect(page.locator('#service-codex-control')).to_have_text('连接 Codex', timeout=15000)
            check('service control disconnects owned native harness', desktop.ipc(page, 'codex_status')['connection'] == 'disconnected')
            check('no desktop JavaScript errors during native input verification', not errors)
            success = True
    finally:
        if proc is not None and proc.poll() is None:
            try:
                control.screenshot('native-last-window.png')
            except Exception:
                pass
        if proc is not None and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)
        if native:
            USER32.SetCursorPos(old_cursor.x, old_cursor.y)
        report = {'success': success, 'passed': len(checks), 'checks': checks, 'actions': actions, 'native_input_action_count': len(actions) if native else 0, 'ui_action_count': len(actions), 'observed_runtime_events': events, 'javascript_errors': errors, 'test_data_directory': str(desktop.DATA), 'input_method': 'Win32 system mouse events and SendInput Unicode / scan-code hotkeys; CDP observation only' if native else 'WebView2 CDP UI automation; no system mouse or keyboard control'}
        (desktop.ARTIFACTS / report_name).write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding='utf-8')
    print(f'{"Native input" if native else "CDP desktop"}: {len(checks)} checks, {len(actions)} UI actions passed', flush=True)


if __name__ == '__main__':
    run()
