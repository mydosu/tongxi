"""Stage8: real mouse/keyboard check of the project summary sharing switch (no model)."""
import ctypes
import json
import sys
import uuid
from hashlib import sha256
from pathlib import Path

import smoke_desktop as desktop
import verify_stage3 as prior
import verify_native_input as native
from playwright.sync_api import expect, sync_playwright

CHECKBOX = "#project-summary"
SHARE = "#share-project-summary"
MODAL = "#modal"
REPORT = Path("artifacts/stage8-native-verification.json")


def main():
    exe = Path(sys.argv[1])
    desktop.EXE = exe
    desktop.DATA = (desktop.ARTIFACTS / f"stage8-native-{uuid.uuid4().hex}").resolve()
    root = (desktop.ARTIFACTS / f"{desktop.DATA.name}-project").resolve()
    root.mkdir(parents=True, exist_ok=True)
    (root / "check.py").write_text("import sys\nsys.exit(0)\n", encoding="utf-8")

    cursor = native.W.POINT()
    native.USER32.GetCursorPos(ctypes.byref(cursor))
    checks, proc, browser = [], None, None
    try:
        if getattr(native, "MWB_BUSY", False):
            raise RuntimeError("mouse helper held by MWB")
        proc, endpoint = desktop.launch()
        with sync_playwright() as pw:
            browser = pw.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            control = native.NativeInput(proc, page)

            convs = desktop.ipc(page, "list_conversations", search="", archived=False)
            group = next(c for c in convs if c.get("kind") == "group")
            other = next(c for c in convs if c.get("kind") != "group")
            group_id = group["id"]
            other_id = other["id"]
            prior.select(page, group_id)

            control.click("#bind-project")                      # physical 1
            expect(page.locator(CHECKBOX)).not_to_be_checked()
            checks.append("summary-default-unchecked")

            control.click(CHECKBOX)                             # physical 2
            expect(page.locator(CHECKBOX)).to_be_checked()
            checks.append("mouse-click-checks")

            control.hotkey(0x20)                                # physical 3: space
            expect(page.locator(CHECKBOX)).not_to_be_checked()
            checks.append("real-space-unchecks")

            control.hotkey(0x1B)                                # physical 4: escape
            expect(page.locator(MODAL)).not_to_be_visible()
            checks.append("real-escape-closes")

            proj = desktop.ipc(page, "register_project", name="stage8", root=str(root), checks=[
                {"name": "ok", "program": sys.executable,
                 "args": ["check.py"], "timeout_seconds": 10}])
            project_id = proj["id"]
            desktop.ipc(page, "bind_project", conversationId=group_id, projectId=project_id)

            prior.select(page, other_id)
            prior.select(page, group_id)                        # force UI refresh
            checks.append("group-ui-refreshed")

            control.click(SHARE)                                # physical 5
            expect(page.locator(SHARE)).to_have_attribute("aria-pressed", "true")
            assert desktop.ipc(page, "get_conversation", id=group_id)["project"]["summary_enabled"] is True
            checks.append("share-on-synced")

            control.click(SHARE)                                # physical 6
            expect(page.locator(SHARE)).to_have_attribute("aria-pressed", "false")
            assert desktop.ipc(page, "get_conversation", id=group_id)["project"]["summary_enabled"] is False
            checks.append("share-off-synced")

            return {"success": True, "passed": len(checks), "checks": checks,
                    "model_requests": 0, "native_input_action_count": len(native.actions),
                    "errors_count": 0, "exe_sha256": sha256(exe.read_bytes()).hexdigest(),
                    "bytes": exe.stat().st_size, "DATA": str(desktop.DATA)}
    finally:
        native.USER32.SetCursorPos(cursor.x, cursor.y)
        if browser is not None:
            try:
                browser.close()
            except Exception:
                pass
        if proc is not None and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)


if __name__ == "__main__":
    try:
        report = main()
    except Exception as exc:
        REPORT.parent.mkdir(parents=True, exist_ok=True)
        REPORT.write_text(json.dumps({"success": False, "model_requests": 0,
                                      "error_class": type(exc).__name__}), encoding="utf-8")
        print(type(exc).__name__)
        sys.exit(1)
    REPORT.parent.mkdir(parents=True, exist_ok=True)
    REPORT.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps({"success": report["success"], "checks": report["checks"],
                      "passed": report["passed"]}, indent=2))
