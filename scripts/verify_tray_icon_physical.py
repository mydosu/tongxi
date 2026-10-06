"""Guarded physical click of the Agent Hub notification-area icon; no model calls."""
from __future__ import annotations

import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import traceback
import uuid

import smoke_desktop as desktop
import verify_native_input as native
import verify_tray as tray
from playwright.sync_api import sync_playwright


UIA_QUERY = r"""$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8
Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$names = @('同席 · Agent Hub','同席','Agent Hub')
$hits = @{}
foreach ($name in $names) {
  $condition = [System.Windows.Automation.PropertyCondition]::new(
    [System.Windows.Automation.AutomationElement]::NameProperty,
    $name,
    [System.Windows.Automation.PropertyConditionFlags]::IgnoreCase)
  $elements = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants,$condition)
  foreach ($element in $elements) {
    try {
      $current = $element.Current
      $rect = $current.BoundingRectangle
      if ($rect.Width -gt 2 -and $rect.Height -gt 2) {
        $key = "$($current.ProcessId):$($current.Name):$([int]$rect.X):$([int]$rect.Y)"
        $hits[$key] = [pscustomobject]@{name=$current.Name;class=$current.ClassName;control=$current.ControlType.ProgrammaticName;pid=$current.ProcessId;x=[int]$rect.X;y=[int]$rect.Y;width=[int]$rect.Width;height=[int]$rect.Height}
      }
    } catch {}
  }
}
foreach ($item in $hits.Values) { [Console]::WriteLine(($item | ConvertTo-Json -Compress)) }
""".replace("\\$", "$")
UIA_FOCUSED_QUERY = r"""$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8
Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes
$element = [System.Windows.Automation.AutomationElement]::FocusedElement
if ($null -eq $element) { [Console]::WriteLine('{}'); exit 0 }
$current = $element.Current
$rect = $current.BoundingRectangle
[Console]::WriteLine((([pscustomobject]@{name=$current.Name;class=$current.ClassName;control=$current.ControlType.ProgrammaticName;pid=$current.ProcessId;x=[int]$rect.X;y=[int]$rect.Y;width=[int]$rect.Width;height=[int]$rect.Height}) | ConvertTo-Json -Compress))
""".replace("\\$", "$")


def query_tray_candidates() -> list[dict]:
    powershell = shutil.which("pwsh") or shutil.which("powershell")
    if not powershell:
        raise RuntimeError("PowerShell is unavailable for taskbar accessibility query")
    encoded = base64.b64encode(UIA_QUERY.encode("utf-16le")).decode("ascii")
    result = subprocess.run(
        [powershell, "-NoProfile", "-EncodedCommand", encoded],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=15,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    if result.returncode != 0:
        raise RuntimeError("Taskbar accessibility query failed")
    matches = []
    for line in result.stdout.splitlines():
        try:
            item = json.loads(line.lstrip("\ufeff"))
        except json.JSONDecodeError:
            continue
        if isinstance(item, dict):
            matches.append(item)
    return matches


def query_focused_element() -> dict:
    powershell = shutil.which("pwsh") or shutil.which("powershell")
    if not powershell:
        raise RuntimeError("PowerShell is unavailable for accessibility focus query")
    encoded = base64.b64encode(UIA_FOCUSED_QUERY.encode("utf-16le")).decode("ascii")
    result = subprocess.run(
        [powershell, "-NoProfile", "-EncodedCommand", encoded],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=10,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    if result.returncode != 0:
        raise RuntimeError("Focused taskbar accessibility query failed")
    return json.loads(result.stdout.strip().lstrip("\ufeff") or "{}")


def is_agent_hub_icon(item: dict) -> bool:
    name = str(item.get("name", "")).casefold()
    return "同席" in name or "agent hub" in name


def physical_click_shell_target(item: dict) -> str:
    x = int(item.get("x", 0)) + max(1, int(item.get("width", 0)) // 2)
    y = int(item.get("y", 0)) + max(1, int(item.get("height", 0)) // 2)
    if int(item.get("width", 0)) <= 2 or int(item.get("height", 0)) <= 2:
        raise RuntimeError("Tray icon accessibility element has no usable bounds")
    native.USER32.SetCursorPos(x, y)
    time.sleep(0.1)
    actual = native.W.POINT()
    if not native.USER32.GetCursorPos(native.C.byref(actual)) or abs(actual.x - x) > 1 or abs(actual.y - y) > 1:
        raise RuntimeError("Cursor did not reach the accessible tray icon bounds")
    hwnd = native.USER32.GetAncestor(native.USER32.WindowFromPoint(actual), 2)
    owner = native.W.DWORD()
    native.USER32.GetWindowThreadProcessId(hwnd, native.C.byref(owner))
    shell_process = native.process_name(owner.value)
    if shell_process not in ("explorer.exe", "shellexperiencehost.exe", "startmenuexperiencehost.exe"):
        raise RuntimeError("Tray icon target is not owned by the Windows shell; stopped before click")
    inputs = (native.Input * 2)()
    inputs[0].type = inputs[1].type = 0
    inputs[0].mi.dwFlags = 0x0002
    inputs[1].mi.dwFlags = 0x0004
    if native.USER32.SendInput(2, inputs, native.C.sizeof(native.Input)) != 2:
        raise RuntimeError("Guarded tray click SendInput failed")
    return shell_process


def focus_notification_area() -> None:
    # The generic scan-code helper omits KEYEVENTF_EXTENDEDKEY for Win; use the
    # documented virtual-key API for Win+B and always release both keys.
    win = 0x5B
    letter_b = 0x42
    native.USER32.keybd_event(win, 0, 0x0001, 0)
    try:
        native.USER32.keybd_event(letter_b, 0, 0, 0)
        native.USER32.keybd_event(letter_b, 0, 0x0002, 0)
    finally:
        native.USER32.keybd_event(win, 0, 0x0001 | 0x0002, 0)


def main() -> int:
    exe = Path(sys.argv[1]).resolve()
    desktop.EXE = exe
    desktop.DATA = desktop.ARTIFACTS / f"tray-physical-{uuid.uuid4().hex}"
    checks: list[dict] = []
    proc = browser = playwright = None
    error = None
    try:
        proc, endpoint = desktop.launch()
        playwright = sync_playwright().start()
        browser = playwright.chromium.connect_over_cdp(endpoint)
        page = desktop.page_for(browser)
        control = native.NativeInput(proc, page)

        control.click("#window-minimize")
        page.wait_for_timeout(800)
        check_hidden = not tray.main_window_visible(proc.pid) and tray.alive(proc.pid)
        checks.append({"name": "tray.app_hidden_but_alive", "ok": check_hidden})
        if not check_hidden:
            raise RuntimeError("Main window did not enter the tray while process remained alive")

        matches = [item for item in query_tray_candidates() if is_agent_hub_icon(item)]
        matches = [item for item in matches if int(item.get("pid", -1)) != proc.pid]
        unique = {
            (item.get("pid"), item.get("name"), item.get("x"), item.get("y"), item.get("width"), item.get("height")): item
            for item in matches
        }
        matches = list(unique.values())
        icon = matches[0] if len(matches) == 1 else None
        found_by = "desktop-uia" if icon else None
        focus_steps = 0
        focus_trace = []
        if icon is not None:
            pass
        else:
            # Keyboard focus is restricted to Windows' own notification area. We only
            # press Enter after the exact Agent Hub accessible name is focused.
            focus_notification_area()  # Win+B
            page.wait_for_timeout(150)
            seen = set()
            for step in range(40):
                focus_steps = step + 1
                focused = query_focused_element()
                key = (focused.get("pid"), focused.get("name"), focused.get("class"))
                focused_pid = int(focused.get("pid", -1))
                focused_process = native.process_name(focused_pid) if focused_pid > 0 else ""
                is_target = is_agent_hub_icon(focused)
                focus_trace.append({"class": focused.get("class"), "control": focused.get("control"), "pid": focused_pid, "process": focused_process, "is_agent_hub": is_target})
                if key in seen and not is_agent_hub_icon(focused):
                    break
                seen.add(key)
                if focused_process not in ("explorer.exe", "shellexperiencehost.exe", "startmenuexperiencehost.exe"):
                    break
                if is_target:
                    icon = focused
                    found_by = "taskbar-keyboard-focus"
                    break
                name = str(focused.get("name", "")).casefold()
                if any(text in name for text in ("show hidden icons", "显示隐藏图标", "隐藏的图标", "more icons")):
                    native.system_hotkey([0x0D])  # Enter opens the shell overflow flyout.
                    page.wait_for_timeout(250)
                else:
                    native.system_hotkey([0x27])  # Right arrow only moves focus; it does not activate icons.
                    page.wait_for_timeout(100)
        checks.append({"name": "tray_icon_uniquely_accessible", "ok": icon is not None, "detail": {"name": icon.get("name") if icon else None,"found_by":found_by,"focus_steps":focus_steps,"direct_matches":len(matches),"focus_trace":focus_trace}})
        if icon is None:
            raise RuntimeError("Agent Hub tray icon was not found in taskbar accessibility or keyboard focus")
        shell_process = physical_click_shell_target(icon)
        time.sleep(1.5)
        restored = tray.main_window_visible(proc.pid)
        checks.append({"name": "physical_tray_icon_click_restores_window", "ok": restored, "detail": {"shell_process": shell_process,"icon_name":icon.get("name")}})
        if not restored:
            raise AssertionError("Physical click on Agent Hub tray icon did not restore the main window")
    except Exception as exc:
        error = exc
    finally:
        if browser is not None:
            try:
                browser.close()
            except Exception:
                pass
        if playwright is not None:
            playwright.stop()
        if proc is not None and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except Exception:
                proc.kill()
                proc.wait(timeout=10)
        report = {
            "success": error is None and all(item["ok"] for item in checks),
            "passed": sum(1 for item in checks if item["ok"]),
            "checks": checks,
            "error_class": type(error).__name__ if error else None,
            "traceback": [f"{Path(frame).name}:{line}" for frame,line,*_ in traceback.extract_tb(error.__traceback__)] if error else [],
            "test_data_directory": str(desktop.DATA),
            "exe_sha256": hashlib.sha256(exe.read_bytes()).hexdigest(),
            "exe_bytes": exe.stat().st_size,
            "system_input_actions": 2 + focus_steps if not error else None,
        }
        desktop.ARTIFACTS.joinpath("tray-icon-physical-verification.json").write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding="utf-8")
    for item in checks:
        print(("PASS" if item["ok"] else "FAIL"), item["name"], item.get("detail"), flush=True)
    if error:
        print(json.dumps({"error_class":type(error).__name__},ensure_ascii=True),flush=True)
        return 1
    return 0 if report["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
