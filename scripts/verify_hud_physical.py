"""Guarded physical mouse drag for the packaged HUD window; no model calls."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import sys
import time
import traceback
import uuid

from playwright.sync_api import sync_playwright

import smoke_desktop as desktop
import verify_hud as hud
import verify_native_input as native


def main() -> int:
    exe = Path(sys.argv[1]).resolve()
    desktop.EXE = exe
    desktop.DATA = desktop.ARTIFACTS / f"hud-physical-{uuid.uuid4().hex}"
    checks: list[dict] = []
    proc = browser = playwright = None
    error = None
    try:
        proc, endpoint = desktop.launch()
        playwright = sync_playwright().start()
        browser = playwright.chromium.connect_over_cdp(endpoint)
        page = desktop.page_for(browser)
        page.on("pageerror", lambda exc: checks.append({"name": "javascript_error", "ok": False, "detail": str(exc)}))

        main_input = native.NativeInput(proc, page)
        main_input.click("#hud-toggle")
        hud_page = hud.hud_page(browser, timeout=20)
        if hud_page is None:
            raise RuntimeError("HUD WebView did not open")
        windows = hud.windows_of(proc.pid)
        window = next((item for item in windows if item["title"] == hud.HUD_TITLE and item["visible"]), None)
        if window is None:
            raise RuntimeError("HUD native window is not visible")

        # Bind the existing guarded input helper to the HUD's actual HWND.
        hud_input = object.__new__(native.NativeInput)
        hud_input.pid = proc.pid
        hud_input.page = hud_page
        hud_input.hwnd = native.W.HWND(window["hwnd"])
        hud_page.evaluate("""() => {
          window.physicalInputs = [];
          for (const kind of ['mousedown','mouseup'])
            document.addEventListener(kind, e => window.physicalInputs.push({kind, trusted:e.isTrusted, target:e.target.className?.toString?.() || e.target.tagName}), true);
        }""")

        box = hud_page.locator(".composer").bounding_box()
        if box is None:
            raise RuntimeError("HUD drag surface is not visible")
        viewport = hud_page.evaluate("({width: innerWidth, height: innerHeight})")
        client = native.W.RECT()
        origin = native.W.POINT(0, 0)
        if not native.USER32.GetClientRect(hud_input.hwnd, native.C.byref(client)):
            raise RuntimeError("HUD client bounds are unavailable")
        if not native.USER32.ClientToScreen(hud_input.hwnd, native.C.byref(origin)):
            raise RuntimeError("HUD screen origin is unavailable")

        # Choose only blank composer padding, never an input or button.
        candidates = hud_page.evaluate("""box => {
          const points = [];
          for (const fx of [0.015, 0.05, 0.1, 0.9, 0.95, 0.985])
            for (const fy of [0.05, 0.15, 0.3, 0.7, 0.85, 0.95])
              points.push([box.x + box.width * fx, box.y + box.height * fy]);
          const result = [];
          for (const [x,y] of points) {
            const el = document.elementFromPoint(x,y);
            if (el && el.closest('.composer') && !el.closest('button,input,textarea,select,a'))
              result.push({x,y,target:el.className?.toString?.() || el.tagName});
          }
          return result;
        }""", box)
        if not candidates:
            raise RuntimeError("No safe blank drag point exists in HUD composer")

        before = hud.window_rect(proc.pid, hud.HUD_TITLE)
        if before is None:
            raise RuntimeError("HUD bounds are unavailable before drag")
        x = y = None
        tested_points = []
        for candidate in candidates:
            candidate_x = round(origin.x + candidate["x"] * client.right / viewport["width"])
            candidate_y = round(origin.y + candidate["y"] * client.bottom / viewport["height"])
            native.USER32.SetCursorPos(candidate_x, candidate_y)
            hud_page.wait_for_timeout(40)
            point = native.W.POINT(candidate_x, candidate_y)
            hit = native.USER32.GetAncestor(native.USER32.WindowFromPoint(point), 2)
            hit_value = hit.value if hasattr(hit, "value") else int(hit or 0)
            tested_points.append({"xy": [candidate_x, candidate_y], "root_hwnd": hit_value, "target": candidate["target"]})
            if hit_value == window["hwnd"]:
                x, y = candidate_x, candidate_y
                break
        if x is None or y is None:
            checks.append({"name": "hud.drag_target_is_hit_testable", "ok": False, "detail": tested_points[:12]})
            raise RuntimeError("HUD composer is transparent to hit-testing at every safe drag point")
        checks.append({"name": "hud.drag_target_is_hit_testable", "ok": True, "detail": {"xy": [x,y], "candidates_checked": len(tested_points)}})
        hud_input.pointer_guard(x, y)

        def send_mouse(flags: int) -> None:
            inputs = (native.Input * 1)()
            inputs[0].type = 0
            inputs[0].mi = native.MouseInput(0, 0, 0, flags, 0, 0)
            if native.USER32.SendInput(1, inputs, native.C.sizeof(native.Input)) != 1:
                raise RuntimeError("Guarded HUD SendInput failed")

        native.USER32.SetCursorPos(x, y)
        hud_input.pointer_guard(x, y)
        send_mouse(0x0002)  # MOUSEEVENTF_LEFTDOWN
        time.sleep(0.15)
        native.USER32.SetCursorPos(x + 90, y + 50)
        time.sleep(0.6)
        send_mouse(0x0004)  # MOUSEEVENTF_LEFTUP
        time.sleep(1.0)

        after = hud.window_rect(proc.pid, hud.HUD_TITLE)
        inputs = hud_page.evaluate("window.physicalInputs")
        moved = bool(after and (abs(after["x"] - before["x"]) >= 20 or abs(after["y"] - before["y"]) >= 20))
        trusted_down = any(row["kind"] == "mousedown" and row["trusted"] for row in inputs)
        checks.extend([
            {"name": "hud.drag_press_is_trusted", "ok": trusted_down, "detail": inputs},
            {"name": "hud.window_moved_by_physical_drag", "ok": moved, "detail": {"before": before, "after": after}},
        ])
        if not all(item["ok"] for item in checks):
            raise AssertionError("HUD physical drag did not move the native window")
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
            "system_input_actions": 3 if not error else None,
        }
        desktop.ARTIFACTS.joinpath("hud-physical-verification.json").write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding="utf-8")
    for item in checks:
        print(("PASS" if item["ok"] else "FAIL"), item["name"], item.get("detail"), flush=True)
    if error:
        print(json.dumps({"error_class":type(error).__name__},ensure_ascii=True),flush=True)
        return 1
    print("HUD physical drag: " + str(report["passed"]) + "/" + str(len(checks)) + " passed", flush=True)
    return 0 if report["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
