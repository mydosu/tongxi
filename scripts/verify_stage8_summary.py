#!/usr/bin/env python3
"""Stage 8 verification: a shared project summary must stay isolated per conversation.

Small real-EXE run. Prints check names/counts only -- never replies or native payloads.
"""
from __future__ import annotations

import hashlib
import json
import sqlite3
import sys
import uuid
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

import probe_albion as probe
import smoke_desktop as desktop
import verify_stage3 as prior
import verify_stage5 as albion

GROUP_QUERY = "请只输出当前系统开发动态中以PROJECT_STATE_开头的标记，没有则NO_DEV_DIGEST"
FIXTURE_REQUEST = "共享摘要隔离验收"
SUMMARY_A = "PROJECT_STATE_A_xyz"
SUMMARY_B = "PROJECT_STATE_B_xyz"
MESSAGE_STATUS = "local_only"  # messages CHECK constraint only accepts local_only
ECHO = "echo stage8-control"
SENDS = 4
REPORT_NAME = "stage8-summary-verification.json"


def check(checks, name, ok):
    checks[name] = bool(ok)
    print(("PASS " if ok else "FAIL ") + name)
    if not checks[name]:
        raise AssertionError(name)


def report_write(payload):
    path = Path(desktop.ARTIFACTS) / REPORT_NAME
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")


def mark(report, checks, name):
    """Record the last reached step so a crash can be localised to one API call."""
    report["state"] = name
    check(checks, name, True)


def failure_frames(exc):
    """Traceback frames (file name + line only) -- no messages, locals or payloads."""
    frames = []
    tb = getattr(exc, "__traceback__", None)
    while tb is not None:
        frames.append({"file": Path(tb.tb_frame.f_code.co_filename).name,
                       "line": tb.tb_lineno})
        tb = tb.tb_next
    return frames


def profile_changes(before, after):
    """Count changed keys; a baseline key missing afterwards counts as changed."""
    changed = sum(1 for key, value in before.items() if after.get(key) != value)
    return changed + len(set(after) - set(before))


def fixture_db():
    for pattern in ("*.db", "*.sqlite", "*.sqlite3"):
        hits = sorted(Path(desktop.DATA).rglob(pattern))
        if hits:
            return hits[0]
    return None


def seed(db, project_id, conversation_id, summary, message_id=None, stamp=1):
    """Insert one delivered user message + completed workflow, or update its summary."""
    with sqlite3.connect(str(db)) as conn:
        mid = message_id or "msg-" + uuid.uuid4().hex
        if message_id is None:
            conn.execute(
                "INSERT INTO messages(id,conversation_id,sender_id,content,status,created_at)"
                " VALUES(?,?,?,?,?,?)",
                (mid, conversation_id, "user", FIXTURE_REQUEST, MESSAGE_STATUS, stamp))
            conn.execute(
                "INSERT INTO workflows(id,project_id,conversation_id,user_message_id,request,"
                "status,plan,summary,error,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                ("wf-" + uuid.uuid4().hex, project_id, conversation_id, mid, FIXTURE_REQUEST,
                 "completed", None, summary, None, stamp, stamp))
        else:
            conn.execute(
                "UPDATE workflows SET summary=?, updated_at=updated_at+1 WHERE conversation_id=?",
                (summary, conversation_id))
        conn.commit()
    return mid


def default_group(page):
    data = desktop.ipc(page, "list_conversations", search="", archived=False)
    items = data.get("items", data) if isinstance(data, dict) else data
    for conv in items or []:
        if conv.get("kind") == "group" or conv.get("is_default_group"):
            return conv["id"]
    raise RuntimeError("default group conversation missing")


def summary_enabled(page, conversation_id):
    conv = desktop.ipc(page, "get_conversation", id=conversation_id) or {}
    return bool((conv.get("project") or {}).get("summary_enabled", False))


def wait_proc(proc, timeout=30):
    wait = getattr(proc, "wait", None)
    if wait is None:
        return True
    try:
        wait(timeout=timeout)
    except TypeError:
        wait()
    except Exception:
        return False
    return True


def execute(report, checks):
    sends = 0
    page_errors = []
    proc = browser = page = None
    thread_id = pid = None
    mark(report, checks, "step_start")
    before = probe.profile_hashes()
    try:
        proc, endpoint = desktop.launch()
        with sync_playwright() as pw:
            browser = pw.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on("pageerror", lambda err: page_errors.append(str(err)))

            group = default_group(page)
            root = (Path(desktop.ARTIFACTS)
                    / (Path(desktop.DATA).name + "-project")).resolve()
            root.mkdir(parents=True, exist_ok=True)
            (root / "check.py").write_text("print('stage8-check')\n", encoding="utf-8")
            project = desktop.ipc(
                page, "register_project", name="stage8fixture", root=str(root))
            project_id = project["id"]
            check(checks, "project_registered", bool(project_id))
            desktop.ipc(page, "bind_project", conversationId=group, projectId=project_id)

            db = fixture_db()
            check(checks, "fixture_db_present", db is not None)
            message_id = seed(db, project_id, group, SUMMARY_A)

            check(checks, "default_summary_disabled",
                  summary_enabled(page, group) is False)
            prior.select(page, group)
            toolbar = page.locator("#share-project-summary")
            check(checks, "share_toolbar_off", toolbar.get_attribute("aria-pressed") == "false")

            mark(report, checks, "step_new_private_before")
            room_id = prior.new_private(page, "albion-wsl", "stage8 summary isolation")
            mark(report, checks, "step_new_private_after")
            mark(report, checks, "step_connect_click_before")
            page.locator("#connect-albion").click()
            mark(report, checks, "step_connect_click_after")
            prior.wait_connection(page, "albion")
            mark(report, checks, "step_wait_connection_done")
            desktop.ipc(page, "set_session_settings", id=room_id,
                        agentId="albion-wsl", model=None, reasoningEffort="none")

            mid = prior.send(page, "albion", ECHO)
            sends += 1
            report["model_requests"] = sends
            first = prior.wait_run(page, "albion", mid)
            info = albion.native_info()
            thread_id, pid = first.get("native_thread_id"), info.get("pid")
            check(checks, "control_no_digest", info.get("digest_injected") is False)

            prior.select(page, group)
            toolbar.click()
            expect(toolbar).to_have_attribute("aria-pressed", "true")
            check(checks, "share_enabled_in_group", summary_enabled(page, group) is True)

            prior.select(page, room_id)
            mid = prior.send(page, "albion", GROUP_QUERY)
            sends += 1
            report["model_requests"] = sends
            second = prior.wait_run(page, "albion", mid)
            check(checks, "digest_a_injected",
                  "PROJECT_STATE_A" in (second.get("text") or "")
                  and albion.native_info().get("digest_injected") is True)
            thread_id = second.get("native_thread_id", thread_id)

            seed(db, project_id, group, SUMMARY_B, message_id=message_id)
            mid = prior.send(page, "albion", GROUP_QUERY)
            sends += 1
            report["model_requests"] = sends
            third = prior.wait_run(page, "albion", mid)
            check(checks, "digest_b_after_update",
                  "PROJECT_STATE_B" in (third.get("text") or "")
                  and albion.native_info().get("digest_injected") is True
                  and third.get("native_thread_id") == thread_id)

            prior.select(page, group)
            toolbar = page.locator("#share-project-summary")
            toolbar.click()
            expect(toolbar).to_have_attribute("aria-pressed", "false")
            prior.select(page, room_id)
            mid = prior.send(page, "albion", ECHO)
            sends += 1
            report["model_requests"] = sends
            fourth = prior.wait_run(page, "albion", mid)
            info = albion.native_info()
            pid = info.get("pid", pid)
            check(checks, "digest_off_after_toolbar",
                  info.get("digest_injected") is False
                  and fourth.get("native_thread_id") == thread_id)

            desktop.ipc(page, "disconnect_albion")
            check(checks, "native_child_exited",
                  pid is not None and albion.wait_linux_exit(pid) is True)

        if proc.poll() is None:
            proc.terminate()
        check(checks, "app_exited", wait_proc(proc))
        changed = profile_changes(before, probe.profile_hashes())
        report["profile_changed"] = changed
        check(checks, "profile_unchanged", changed == 0)
        check(checks, "no_frontend_errors", not page_errors)
        mark(report, checks, "step_done")
    finally:
        report["model_requests"] = sends
        if browser is not None:
            try:
                browser.close()
            except Exception:
                pass
        if proc is not None:
            try:
                if proc.poll() is None:
                    proc.terminate()
                    proc.wait(timeout=10)
            except Exception:
                pass
        if report.get("profile_changed") is None:
            # Baseline keys are always compared here, even when an error aborted
            # the run; a key missing afterwards counts as a change. Never raises,
            # so the original exception/frames stay intact.
            try:
                report["profile_changed"] = profile_changes(before, probe.profile_hashes())
            except Exception:
                pass
    check(checks, "four_model_requests", sends == SENDS)


def main():
    exe = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(desktop.EXE)
    desktop.EXE = exe
    desktop.DATA = Path(desktop.ARTIFACTS) / ("stage8-summary-" + uuid.uuid4().hex)
    desktop.DATA.mkdir(parents=True, exist_ok=True)

    report = {"success": False, "passed": 0, "checks": {}, "model_requests": 0,
              "profile_changed": None, "state": None,
              "test_data_directory": str(desktop.DATA),
              "exe_sha256": None, "bytes": None, "errors_count": 0}
    checks = {}
    error_class = None
    frames = []
    try:
        raw = exe.read_bytes()
        report["exe_sha256"] = hashlib.sha256(raw).hexdigest()
        report["bytes"] = len(raw)
        execute(report, checks)
    except Exception as exc:  # noqa: BLE001
        error_class = type(exc).__name__
        frames = failure_frames(exc)  # code paths only: no str(exc), locals or payloads

    failed = sorted(name for name, ok in checks.items() if not ok)
    report["checks"] = checks
    report["passed"] = sum(1 for ok in checks.values() if ok)
    report["errors_count"] = len(failed) + (1 if error_class else 0)
    report["success"] = error_class is None and not failed and bool(checks)
    if not report["success"]:
        report["failure"] = {"exception_class": error_class, "checks": failed,
                             "frames": frames}
    report_write(report)
    print("checks: %d/%d passed, errors=%d" % (report["passed"], len(checks),
                                              report["errors_count"]))
    return 0 if report["success"] else 1


if __name__ == "__main__":
    sys.exit(main())
