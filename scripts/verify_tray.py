"""托盘验收：最小化/关闭应收进托盘（进程存活、窗口不可见），再次启动应把窗口唤回。

托盘图标本身的点击无法自动化；唤回路径与托盘菜单「显示主窗口」共用 main.rs 的
focus_main_window，这里用「二次启动（单实例插件）」覆到同一条路径。
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


def visible_windows(pid: int) -> list[dict]:
    found: list[dict] = []

    def callback(hwnd, _lparam):
        owner = wintypes.DWORD()
        USER32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid:
            length = USER32.GetWindowTextLengthW(hwnd)
            buf = ctypes.create_unicode_buffer(length + 1)
            USER32.GetWindowTextW(hwnd, buf, length + 1)
            if USER32.IsWindowVisible(hwnd):
                found.append({"hwnd": int(hwnd), "title": buf.value})
        return True

    USER32.EnumWindows(ENUM_PROC(callback), 0)
    return found


def main_window_visible(pid: int) -> bool:
    """只看主窗口：进程还带着单实例插件的辅助窗口，它们的可见性不代表主窗口状态。"""
    return any("同席" in window["title"] for window in visible_windows(pid))


def alive(pid: int) -> bool:
    out = subprocess.run(
        ["tasklist", "/FI", f"PID eq {pid}", "/NH"], capture_output=True, text=True
    ).stdout
    return str(pid) in out


def relaunch() -> None:
    env = os.environ.copy()
    env["AGENT_HUB_DATA_DIR"] = str(sd.DATA)
    subprocess.Popen([str(sd.EXE)], env=env, creationflags=subprocess.CREATE_NO_WINDOW)


def main() -> int:
    exe = sys.argv[1]
    sd.EXE = sd.ROOT / exe if not os.path.isabs(exe) else sd.ROOT / os.path.relpath(exe, sd.ROOT)
    sd.DATA = sd.ARTIFACTS / f"tray-{uuid.uuid4().hex[:12]}"

    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    proc, endpoint = sd.launch()
    pid = proc.pid
    playwright = sync_playwright().start()
    browser = playwright.chromium.connect_over_cdp(endpoint)
    page = sd.page_for(browser)

    check("initial.window_visible", main_window_visible(pid))

    page.locator("#window-minimize").click()
    time.sleep(5)
    check("minimize.hidden", not main_window_visible(pid), visible_windows(pid))
    check("minimize.process_alive", alive(pid))

    relaunch()
    time.sleep(9)
    check("restore.visible_after_relaunch", main_window_visible(pid))

    page.locator("#window-close").click()
    time.sleep(5)
    check("close.hidden", not main_window_visible(pid), visible_windows(pid))
    check("close.process_alive", alive(pid))

    relaunch()
    time.sleep(9)
    check("restore.visible_after_second_relaunch", main_window_visible(pid))
    check("restore.same_process", proc.poll() is None, proc.poll())

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
    sd.ARTIFACTS.joinpath("tray-verification.json").write_text(
        json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    for item in checks:
        print(("PASS" if item["ok"] else "FAIL"), item["name"], item["detail"])
    print("Tray: " + result["passed"] + " passed")
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
