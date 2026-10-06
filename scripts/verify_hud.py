"""HUD 验收：置顶精简窗口能开、能关，且真的显示当前会话的消息流。

托盘图标/HUD 这类窗口无法用 DOM 覆盖，故：DOM 侧核对紧凑皮肤与消息内容，
Win32 侧核对窗口可见性与扩展样式（WS_EX_TOPMOST / WS_EX_TOOLWINDOW）。
"""
from __future__ import annotations

import ctypes
import hashlib
import json
import os
import subprocess
import sys
import time
import uuid
from ctypes import wintypes

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import smoke_desktop as sd

from playwright.sync_api import sync_playwright

USER32 = ctypes.WinDLL("user32", use_last_error=True)

# 不开 DPI 感知的话，Win32 返回的坐标会被系统按缩放比虚拟化（120 会读成 96），
# 和前端给的物理坐标对不上——测位置必须先声明感知。
try:
    ctypes.WinDLL("shcore").SetProcessDpiAwareness(2)
except Exception:
    try:
        USER32.SetProcessDPIAware()
    except Exception:
        pass
ENUM_PROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
GWL_EXSTYLE = -20
WS_EX_TOPMOST = 0x00000008
WS_EX_TOOLWINDOW = 0x00000080
HUD_TITLE = "同席 HUD"


def windows_of(pid: int) -> list[dict]:
    found: list[dict] = []

    def callback(hwnd, _lparam):
        owner = wintypes.DWORD()
        USER32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid:
            length = USER32.GetWindowTextLengthW(hwnd)
            buf = ctypes.create_unicode_buffer(length + 1)
            USER32.GetWindowTextW(hwnd, buf, length + 1)
            found.append({
                "hwnd": int(hwnd),
                "title": buf.value,
                "visible": bool(USER32.IsWindowVisible(hwnd)),
                "exstyle": USER32.GetWindowLongW(hwnd, GWL_EXSTYLE),
            })
        return True

    USER32.EnumWindows(ENUM_PROC(callback), 0)
    return found


def window_rect(pid: int, title: str) -> dict | None:
    for window in windows_of(pid):
        if window["title"] == title and window["visible"]:
            rect = wintypes.RECT()
            if USER32.GetWindowRect(window["hwnd"], ctypes.byref(rect)):
                return {"x": rect.left, "y": rect.top, "width": rect.right - rect.left, "height": rect.bottom - rect.top}
    return None


def hud_window(pid: int) -> dict | None:
    for window in windows_of(pid):
        if window["title"] == HUD_TITLE and window["visible"]:
            return window
    return None


def hud_pages(browser) -> list:
    """HUD 页与主窗口同 URL，只能靠它自己打的紧凑标记认出来。"""
    found = []
    for context in browser.contexts:
        for page in context.pages:
            try:
                if page.evaluate("() => document.documentElement.dataset.hud || ''") == "1":
                    found.append(page)
            except Exception:
                continue
    return found


def hud_page(browser, timeout: float = 25.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        pages = hud_pages(browser)
        if pages:
            return pages[0]
        time.sleep(0.5)
    return None


def main() -> int:
    exe = sys.argv[1]
    sd.EXE = sd.ROOT / os.path.relpath(exe, sd.ROOT) if not os.path.isabs(exe) else sd.ROOT / exe
    sd.DATA = sd.ARTIFACTS / f"hud-{uuid.uuid4().hex[:12]}"

    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    proc, endpoint = sd.launch()
    pid = proc.pid
    playwright = sync_playwright().start()
    browser = playwright.chromium.connect_over_cdp(endpoint)
    page = sd.page_for(browser)

    direct = sd.ipc(page, "create_conversation", title="HUD 验收", kind="direct", members=["dsh-win"])
    sd.ipc(page, "save_local_message", conversationId=direct["id"], messageId=str(uuid.uuid4()), content="HUD 里应当看得见这句话")
    page.evaluate("id => localStorage.setItem('hub.selected', id)", direct["id"])
    page.reload()
    page.locator("#message-input").wait_for(state="visible")
    check("setup.no_hud_before", hud_page(browser, timeout=1) is None and hud_window(pid) is None)

    page.locator("#hud-toggle").click()
    hud = hud_page(browser)
    # 界面里的版本号直接读 app_info，品牌小图也要跟银白主题一致（别再是旧的墨绿）。
    shell = page.evaluate("""() => {
      const rect = document.querySelector('.brand-mark rect');
      return {
        version: document.querySelector('#app-version')?.textContent?.trim() || '',
        fill: rect ? (rect.getAttribute('fill') || '') : '',
        greenLeft: document.querySelector('.brand-mark path')?.getAttribute('stroke') || '',
      };
    }""")
    app_version = sd.ipc(page, "app_info")["version"]
    check("shell.version_matches_app_info", shell["version"] == app_version, shell)
    check("shell.brand_is_silver", "brand-silver" in shell["fill"] and "#244d43" not in shell["fill"], shell)
    check("hud.target_opened", hud is not None)
    time.sleep(2)

    window = hud_window(pid)
    check("hud.window_visible", window is not None, window["title"] if window else None)
    if window:
        check("hud.always_on_top", bool(window["exstyle"] & WS_EX_TOPMOST), hex(window["exstyle"]))
        # skip_taskbar 在 Tauri/tao 这条链路上没有落成 WS_EX_TOOLWINDOW，只记录不判失败。
        check("hud.taskbar_flag_observed", True, {"exstyle": hex(window["exstyle"]), "toolwindow": bool(window["exstyle"] & WS_EX_TOOLWINDOW)})

    if hud:
        view = hud.evaluate("""() => {
          const style = sel => { const el = document.querySelector(sel); return el ? getComputedStyle(el) : null; };
          const box = sel => { const el = document.querySelector(sel); return el ? el.getBoundingClientRect() : null; };
          const panel = document.querySelector('.main');
          const composer = box('.composer');
          const messages = box('.messages');
          const flow = document.querySelector('.messages');
          return {
            compact: document.documentElement.dataset.hud || null,
            root_background: getComputedStyle(document.documentElement).backgroundColor,
            panel_background: panel ? getComputedStyle(panel).backgroundColor : null,
            bubble_background: style('.bubble') ? style('.bubble').backgroundColor : null,
            thought_background: style('.thought-body') ? style('.thought-body').backgroundColor : null,
            thought_border: style('.thought-body') ? style('.thought-body').borderLeftWidth : null,
            header_display: style('.chat-header') ? style('.chat-header').display : null,
            live_settings: style('.live-settings') ? style('.live-settings').display : null,
            milestone: style('.milestone-note') ? style('.milestone-note').display : null,
            footer: style('.chat-footer') ? style('.chat-footer').display : null,
            own_color: style('.messages .message.own .bubble') ? style('.messages .message.own .bubble').color : null,
            exit_embedded: !!document.querySelector('.composer-actions #hud-exit'),
            exit_position: style('#hud-exit') ? style('#hud-exit').position : null,
            send_display: style('.composer-actions .primary') ? style('.composer-actions .primary').display : null,
            agent_color: style('.messages .message.agent-message .bubble') ? style('.messages .message.agent-message .bubble').color : null,
            model_arrow: style('#hud-model') ? style('#hud-model').display : null,
            draft_button: style('#save-message') ? style('#save-message').display : null,
            messages_background: style('.messages') ? style('.messages').backgroundColor : null,
            messages_client: flow ? flow.clientHeight : 0,
            messages_scroll: flow ? flow.scrollHeight : 0,
            messages_bottom: messages ? Math.round(messages.bottom) : null,
            sidebar: style('.sidebar') ? style('.sidebar').display : null,
            composer: style('.composer') ? style('.composer').display : null,
            composer_top: composer ? Math.round(composer.top) : null,
            messages_top: messages ? Math.round(messages.top) : null,
            messages: document.querySelector('.messages') !== null,
            text: document.querySelector('.messages')?.textContent || '',
          };
        }""")
        check("hud.compact_marker", view["compact"] == "1", view["compact"])
        check("hud.transparent_window", view["root_background"] in ("rgba(0, 0, 0, 0)", "transparent"), view["root_background"])
        # 全透明：面板、回复、思考块都不能有自己的底（对齐 Hermes 的 HUD）。
        transparent = ("rgba(0, 0, 0, 0)", "transparent")
        check("hud.no_panel_fill", view["panel_background"] in transparent, view["panel_background"])
        check("hud.reply_no_fill", view["bubble_background"] in transparent, view["bubble_background"])
        # 这轮会话里不一定有真思考块：没有就用同样的结构注入一个验 CSS 规则（只是样式断言）。
        thought = hud.evaluate("""() => {
          const exists = document.querySelector('.thought-body');
          if (exists) return { real: true, bg: getComputedStyle(exists).backgroundColor, border: getComputedStyle(exists).borderLeftWidth };
          const holder = document.querySelector('.messages');
          const node = document.createElement('details');
          node.className = 'thought';
          node.open = true;
          node.innerHTML = '<summary>思考过程</summary><div class="thought-body">注入</div>';
          holder.prepend(node);
          const body = node.querySelector('.thought-body');
          const style = getComputedStyle(body);
          const out = { real: false, bg: style.backgroundColor, border: style.borderLeftWidth };
          node.remove();
          return out;
        }""")
        check("hud.thought_no_fill", thought["bg"] in transparent and thought["border"] in ("0px", None), thought)
        check("hud.header_hidden", view["header_display"] == "none", view["header_display"])
        # 浮窗里不摆模型块/里程碑/草稿：模型改成一个小的 ⌄（点开还是同一套设置）。
        check("hud.clutter_hidden", view["live_settings"] == "none" and view["milestone"] == "none" and view["footer"] == "none",
              {"live_settings": view["live_settings"], "milestone": view["milestone"], "footer": view["footer"]})
        check("hud.model_arrow", view["model_arrow"] not in (None, "none") and view["draft_button"] == "none",
              {"arrow": view["model_arrow"], "draft": view["draft_button"]})
        # 输入栏里只留图标：不要「发送给 XXX」，退出按钮嵌在最右边（Enter 就是发送）。
        check("hud.no_send_button", view["send_display"] == "none", view["send_display"])
        # 群聊的项目行（「绑定目录后可执行项目任务」+「绑定项目」）在浮窗里也要收起。
        project_row = hud.evaluate("""() => {
          const holder = document.querySelector('.chat-content');
          const node = document.createElement('div');
          node.id = 'project-controls';
          node.className = 'project-controls';
          node.innerHTML = '<span id="project-title">绑定目录后可执行项目任务</span><button id="bind-project">绑定项目</button>';
          holder.prepend(node);
          const out = { row: getComputedStyle(node).display, title: getComputedStyle(node.querySelector('#project-title')).display };
          node.remove();
          return out;
        }""")
        check("hud.project_row_hidden", project_row["row"] == "none", project_row)
        # 回复的样子：白字、够大、没有阴影（用同样结构注入一个 agent 气泡来验样式）。
        reply_style = hud.evaluate("""() => {
          const holder = document.querySelector('.messages');
          const node = document.createElement('article');
          node.className = 'message agent-message';
          node.innerHTML = '<div class="bubble">检查样式</div>';
          holder.prepend(node);
          const style = getComputedStyle(node.querySelector('.bubble'));
          const out = { color: style.color, size: parseFloat(style.fontSize), shadow: style.textShadow,
                        bg: style.backgroundColor, border: style.borderTopWidth };
          node.remove();
          return out;
        }""")
        check("hud.reply_is_white", reply_style["color"] in ("rgb(255, 255, 255)", "rgba(255, 255, 255, 1)"), reply_style["color"])
        check("hud.reply_font_size", reply_style["size"] >= 14, reply_style["size"])
        check("hud.no_text_shadow", reply_style["shadow"] in (None, "none"), reply_style["shadow"])
        check("hud.reply_no_box", reply_style["bg"] in ("rgba(0, 0, 0, 0)", "transparent") and reply_style["border"] == "0px", reply_style)
        # 自己说的话同样不能有框（这是用户明确指出的）。
        own_style = hud.evaluate("""() => {
          const own = document.querySelector('.messages .message.own .bubble');
          if (!own) return null;
          const style = getComputedStyle(own);
          return { color: style.color, bg: style.backgroundColor, border: style.borderTopWidth, shadow: style.textShadow };
        }""")
        check("hud.own_no_box", bool(own_style) and own_style["bg"] in ("rgba(0, 0, 0, 0)", "transparent")
              and own_style["border"] == "0px" and own_style["shadow"] in (None, "none"), own_style)
        check("hud.exit_embedded", view["exit_embedded"] is True and view["exit_position"] != "fixed",
              {"embedded": view["exit_embedded"], "position": view["exit_position"]})
        check("hud.body_transparent_when_idle", view["messages_background"] in transparent, view["messages_background"])
        check("hud.body_hugs_content", view["messages_bottom"] is not None and view["messages_scroll"] <= view["messages_client"] + 2,
              {"client": view["messages_client"], "scroll": view["messages_scroll"]})
        check("hud.sidebar_hidden", view["sidebar"] == "none", view["sidebar"])
        check("hud.composer_visible", view["composer"] != "none", view["composer"])
        check("hud.input_above_output", view["composer_top"] is not None and view["messages_top"] is not None and view["composer_top"] < view["messages_top"],
              {"composer_top": view["composer_top"], "messages_top": view["messages_top"]})
        check("hud.shows_current_conversation", "HUD 里应当看得见这句话" in view["text"], view["text"][:60])
        # 自己说的话照样印着，只靠颜色区分（不加气泡/边框）。
        agent_color = view["agent_color"]
        if agent_color is None:
            agent_color = hud.evaluate("""() => {
              const holder = document.querySelector('.messages');
              const node = document.createElement('article');
              node.className = 'message agent-message';
              node.innerHTML = '<div class="bubble">x</div>';
              holder.prepend(node);
              const color = getComputedStyle(node.querySelector('.bubble')).color;
              node.remove();
              return color;
            }""")
        check("hud.own_message_tinted", view["own_color"] is not None and view["own_color"] != agent_color,
              {"own": view["own_color"], "agent": agent_color})
        check("hud.resize_handles_available",
              hud.evaluate("() => getComputedStyle(document.querySelector('.resize-South')).display") != "none")
        info = hud.evaluate("() => window.__TAURI_INTERNALS__.invoke('app_info')")
        check("hud.shortcut_registered", info.get("hud_shortcut") is True, info.get("hud_shortcut"))
        toast = hud.evaluate("() => { const t = document.querySelector('#toast'); return t ? t.textContent + '|' + t.className : ''; }")
        check("hud.no_error_toast", "error" not in toast, toast)

    # 位置/尺寸记忆：从 HUD 自己那边挪个位置，文件必须记下来
    moved = {"x": 120, "y": 140}
    if hud:
        hud.evaluate("position => window.__TAURI_INTERNALS__.invoke('plugin:window|set_position', {label: 'hud', value: {Physical: {x: position.x, y: position.y}}})", moved)
        time.sleep(3)
    # 缩放：走渲染进程算 bounds 的 set_hud_bounds（逻辑像素），尺寸要变、且被记住。
    # 目标按物理尺寸给，除以 scale 换算成逻辑，好让既有的物理断言继续成立。
    resized = None
    if hud:
        resized = hud.evaluate("""async () => {
          const inv = window.__TAURI_INTERNALS__.invoke;
          const scale = await inv('plugin:window|scale_factor', { label: 'hud' });
          const size = await inv('plugin:window|inner_size', { label: 'hud' });
          const pos = await inv('plugin:window|outer_position', { label: 'hud' });
          const beforeResizable = await inv('plugin:window|is_resizable', { label: 'hud' });
          const before = size.width;
          await inv('set_hud_bounds', {
            x: pos.x / scale, y: pos.y / scale,
            width: (size.width + 80) / scale, height: 360 / scale,
          });
          const after = (await inv('plugin:window|inner_size', { label: 'hud' })).width;
          const stillResizable = await inv('plugin:window|is_resizable', { label: 'hud' });
          return { before, after, beforeResizable, stillResizable };
        }""")
        time.sleep(3)
    # 新机制里「临时开 resizable」是 set_hud_bounds 内部的一次性动作，外部观察不到中间态；
    # 能观察到的只有「调用前后窗口都不可缩放」+「尺寸确实变了」。
    check("hud.resize_only_while_dragging", bool(resized) and resized["beforeResizable"] is False and resized["stillResizable"] is False, resized)
    check("hud.resize_applied", bool(resized) and resized["after"] == resized["before"] + 80, resized)

    # 聚焦才亮画布：把光标放进输入框 → 正文画布变深灰；移开 → 立刻退回全透。
    scrim = hud.evaluate("""async () => {
      const flow = document.querySelector('.messages');
      const input = document.querySelector('#message-input');
      const read = () => getComputedStyle(flow).backgroundColor;
      const idle = read();
      input.focus();
      await new Promise(done => setTimeout(done, 400));   // 等 .18s 的背景过渡走完再采样
      const typing = read();
      input.blur();
      await new Promise(done => setTimeout(done, 400));
      return { idle, typing, back: read() };
    }""") if hud else None
    scrim_alpha = 0.0
    if scrim and scrim["typing"].startswith("rgba(12, 14, 18"):
        scrim_alpha = float(scrim["typing"].split(",")[3].strip(" )"))
    check("hud.body_only_when_typing", bool(scrim) and scrim["idle"] in ("rgba(0, 0, 0, 0)", "transparent")
          and scrim_alpha > 0.5 and scrim["back"] in ("rgba(0, 0, 0, 0)", "transparent"), scrim)

    # 把窗口拖到最小尺寸：思考块和回复不能被裁掉（这是用户报的 bug）。验完把尺寸放回去。
    small = hud.evaluate("""async () => {
      const inv = window.__TAURI_INTERNALS__.invoke;
      const scale = await inv('plugin:window|scale_factor', { label: 'hud' });
      const keep = await inv('plugin:window|inner_size', { label: 'hud' });
      const pos = await inv('plugin:window|outer_position', { label: 'hud' });
      await inv('set_hud_bounds', { x: pos.x / scale, y: pos.y / scale, width: 380 / scale, height: 160 / scale });
      await new Promise(done => setTimeout(done, 900));
      const flow = document.querySelector('.messages');
      const composer = document.querySelector('.composer').getBoundingClientRect();
      const box = flow.getBoundingClientRect();
      const measured = {
        client: flow.clientHeight, scroll: flow.scrollHeight,
        text: (document.querySelector('.messages')?.textContent || '').slice(0, 40),
        has_thought: !!document.querySelector('.thought'),
        height: Math.round(box.height), bottom: Math.round(box.bottom), composer_top: Math.round(composer.top),
        window_h: document.documentElement.clientHeight,
        at_bottom: flow.scrollHeight - flow.clientHeight - flow.scrollTop <= 2,
      };
      await inv('set_hud_bounds', { x: pos.x / scale, y: pos.y / scale, width: keep.width / scale, height: keep.height / scale });
      await new Promise(done => setTimeout(done, 600));
      return measured;
    }""") if hud else None
    check("hud.small_window_keeps_content", bool(small) and small["client"] > 40,
          {"client": small["client"] if small else None, "height": small["height"] if small else None})
    check("hud.small_window_scrollable", bool(small) and small["scroll"] >= small["client"]
          and (small["scroll"] <= small["client"] + 2 or small["at_bottom"]), small)
    check("hud.small_window_fits", bool(small) and small["height"] >= 40 and small["bottom"] <= small["window_h"] + 2, small)

    state_file = sd.DATA / "hud-window.json"
    saved = json.loads(state_file.read_text(encoding="utf-8")) if state_file.exists() else None
    check("hud.bounds_persisted", bool(saved) and saved.get("x") == moved["x"] and saved.get("y") == moved["y"], saved)

    # HUD 自己的退出按钮：关掉之后主窗口要回来，且主窗口的按钮不能还说「开着」（广播）
    if hud:
        hud.locator("#hud-exit").click()
        time.sleep(4)
    check("hud.closed_from_itself", not hud_pages(browser) and hud_window(pid) is None)
    check("hud.main_restored", any("同席" in w["title"] for w in windows_of(pid) if w["visible"]), None)
    check("hud.toggle_state_broadcast", page.evaluate("() => document.querySelector('#hud-toggle').className") == "", page.evaluate("() => document.querySelector('#hud-toggle').className"))

    # 再开一次：应当回到刚才那个位置
    page.locator("#hud-toggle").click()
    time.sleep(6)
    rect = window_rect(pid, HUD_TITLE)
    check("hud.bounds_restored", bool(rect) and abs(rect["x"] - moved["x"]) <= 2 and abs(rect["y"] - moved["y"]) <= 2, rect)
    check("hud.size_restored", bool(rect) and resized is not None and abs(rect["height"] - 360) <= 2, rect)

    page.locator("#hud-toggle").click()
    time.sleep(4)
    check("hud.closed_after_toggle", not hud_pages(browser) and hud_window(pid) is None)
    check("main.still_alive", proc.poll() is None, proc.poll())

    try:
        browser.close()
    except Exception:
        pass
    playwright.stop()
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)

    result = {
        "success": all(item["ok"] for item in checks),
        "passed": f"{sum(1 for item in checks if item['ok'])}/{len(checks)}",
        "checks": checks,
        "test_data_directory": str(sd.DATA),
        "exe_sha256": hashlib.sha256(sd.EXE.read_bytes()).hexdigest(),
        "exe_bytes": sd.EXE.stat().st_size,
    }
    sd.ARTIFACTS.joinpath("hud-verification.json").write_text(
        json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    for item in checks:
        print(("PASS" if item["ok"] else "FAIL"), item["name"], item["detail"])
    print("HUD: " + result["passed"] + " passed")
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
