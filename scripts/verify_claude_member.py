"""Claude 成员的发布 EXE 真实验收：连接与模型目录、私聊回复与续聊、停止、原生会话列表、
Claude 作为隐藏管家调度（无工具）以及 Claude 成员在隔离项目中真实改码并合并。

会发送真实模型请求：只用 haiku + low。设置 AGENT_HUB_LIVE_CLAUDE=1 授权运行。
数据目录与项目都是一次性的临时目录，成功后清理。
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import time
import uuid

from playwright.sync_api import sync_playwright

import smoke_desktop as desktop
import verify_coordinator_live as live

ROOT = Path(__file__).resolve().parents[1]
EXE = Path(os.environ.get("AGENT_HUB_EXE", str(ROOT / "release" / "Agent Hub.exe"))).resolve()
REPORT = ROOT / "artifacts" / "claude-member-verification.json"
AGENT = "claude-win"


def wait_reply(page, room: str, after: int, timeout: int = 300) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        messages = desktop.ipc(page, "get_conversation", id=room)["messages"]
        replies = [m for m in messages[after:] if m["sender_id"] == AGENT]
        if replies and replies[-1]["status"] in ("completed", "failed", "interrupted"):
            return replies[-1]
        time.sleep(0.5)
    raise AssertionError("Claude reply timed out")


def send(page, room: str, text: str) -> int:
    count = len(desktop.ipc(page, "get_conversation", id=room)["messages"])
    desktop.ipc(page, "send_claude_message", conversationId=room, messageId=str(uuid.uuid4()), content=text)
    return count


def session_of(page, room: str) -> dict:
    return next(s for s in desktop.ipc(page, "get_conversation", id=room)["sessions"] if s["agent_id"] == AGENT)


def main() -> None:
    if os.environ.get("AGENT_HUB_LIVE_CLAUDE") != "1":
        raise SystemExit("Set AGENT_HUB_LIVE_CLAUDE=1 to run the real-model Claude member acceptance.")
    project_root = Path(tempfile.mkdtemp(prefix="tongxi-claude-project-")).resolve()
    desktop.EXE = EXE
    desktop.DATA = Path(tempfile.gettempdir()) / ("tongxi-claude-data-" + uuid.uuid4().hex[:10])
    desktop.ISOLATED_SESSION_DB = True
    desktop.errors.clear()
    live.checks.clear()
    evidence: dict[str, object] = {"executable_sha256": hashlib.sha256(EXE.read_bytes()).hexdigest()}
    proc = browser = page = None
    stage = "fixture"
    success = False
    try:
        (project_root / "calc.py").write_text("def square(value):\n    return value\n", encoding="utf-8")
        for args in (["init", "-q"], ["config", "user.name", "Claude Member Test"], ["config", "user.email", "claude-member@localhost"],
                     ["add", "calc.py"], ["commit", "-q", "-m", "claude member fixture"]):
            subprocess.run(["git", *args], cwd=project_root, check=True, timeout=30)

        with sync_playwright() as playwright:
            stage = "launch"
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on("pageerror", lambda error: desktop.errors.append(str(error)))

            stage = "connect"
            catalog = live.connected_models(page, AGENT, "claude")
            haiku = next((m for m in catalog if m["id"] == "haiku"), None)
            live.check("Claude catalog lists haiku with low effort", bool(haiku) and "low" in haiku["efforts"])
            status = desktop.ipc(page, "claude_status")
            live.check("Claude reports the native CLI version", bool(status.get("version")))
            evidence["claude_version"] = status.get("version")

            stage = "private_chat"
            room = next(r["id"] for r in desktop.ipc(page, "list_conversations", search="", archived=False)
                        if r["kind"] == "direct" and r["members"] == [AGENT])
            desktop.ipc(page, "set_session_settings", id=room, agentId=AGENT, model="haiku", reasoningEffort="low")
            reply = wait_reply(page, room, send(page, room, "只回答两个字：在的"))
            live.check("private reply completes", reply["status"] == "completed" and "在" in reply["content"])
            first_session = session_of(page, room)["native_session_id"]
            live.check("private chat binds a native Claude session", bool(first_session))
            reply = wait_reply(page, room, send(page, room, "我上一条消息让你回答哪两个字？只回答那两个字。"))
            live.check("second turn resumes the same native context", reply["status"] == "completed" and "在的" in reply["content"])
            live.check("native session id stays stable", session_of(page, room)["native_session_id"] == first_session)
            after = send(page, room, "从1数到400，每个数字一行，不要使用工具。")
            time.sleep(5)
            desktop.ipc(page, "cancel_claude_run", conversationId=room)
            reply = wait_reply(page, room, after, timeout=60)
            live.check("stop interrupts the running Claude turn", reply["status"] == "interrupted")
            native = desktop.ipc(page, "native_sessions", agentId=AGENT, conversationId=room)
            live.check("native session list includes the bound session", any(s["id"] == first_session and s["current"] for s in native))

            stage = "project"
            project = desktop.ipc(page, "register_project", name="Claude member smoke", root=str(project_root))
            group = desktop.ipc(page, "create_conversation", title="Claude member smoke", kind="group", members=[AGENT, "codex-win"])["id"]
            desktop.ipc(page, "bind_project", conversationId=group, projectId=project["id"])
            detail = desktop.ipc(page, "get_conversation", id=group)
            chat = next(c["conversation_id"] for c in detail["project_agent_chats"] if c["agent_id"] == AGENT)
            desktop.ipc(page, "set_session_settings", id=chat, agentId=AGENT, model="haiku", reasoningEffort="low")
            live.set_coordinator(page, group, AGENT, {"id": "haiku"})
            # 经 IPC 新建的群不会自动出现在侧栏：刷新页面再选中。
            page.reload()
            page.locator(f'[data-conversation="{group}"]').wait_for(timeout=30000)
            desktop.choose(page, group)
            request = "请Claude修复calc.py：让square返回value的平方，只修改calc.py这一个文件，完成后用一句话说明改动。"
            workflow_id, _ = live.start(page, group, request)
            job = live.wait_workflow(page, group, workflow_id, timeout=1200)
            live.check("project workflow completes", job["status"] == "completed", {"status": job["status"], "error": job.get("error")})
            manager = [a for a in job["attempts"] if a.get("activity") == "coordinate"]
            workers = [a for a in job["attempts"] if a.get("task_id")]
            live.check("Claude coordinated the workflow", bool(manager) and all(a["agent_id"] == AGENT for a in manager))
            live.check("Claude coordinator made no tool calls", all(not live.manager_tool_events(a) for a in manager))
            live.check("Claude implemented the routed task", bool(workers) and all(a["agent_id"] == AGENT and a["status"] == "completed" for a in workers))
            live.check("worker used tools on the project", any(a.get("tool_events") for a in workers))
            source = (project_root / "calc.py").read_text(encoding="utf-8")
            result = subprocess.run(["python", "-c", "import calc; assert calc.square(7) == 49"], cwd=project_root, capture_output=True, timeout=30)
            live.check("merged calc.py now squares its input", result.returncode == 0, {"source": source[:200]})
            changed = subprocess.run(["git", "diff", "--name-only", "HEAD~1"], cwd=project_root, capture_output=True, text=True, timeout=30).stdout.split()
            live.check("only calc.py changed", changed == ["calc.py"], {"changed": changed})
            with sqlite3.connect(desktop.DATA / "hub.db") as db:
                row = db.execute("SELECT native_session_id FROM coordinator_sessions WHERE workflow_id=?", (workflow_id,)).fetchone()
            worker_sid = session_of(page, chat)["native_session_id"]
            live.check("coordinator and worker sessions are separate", bool(row) and bool(worker_sid) and row[0] != worker_sid)
            evidence["workflow"] = {"status": job["status"], "attempts": [{k: a.get(k) for k in ("agent_id", "activity", "stage", "status", "model", "reasoning_effort")} for a in job["attempts"]]}
            live.check("window reported no JavaScript errors", not desktop.errors)
            success = True
    except BaseException as exc:
        print(f"FAIL {stage}: {type(exc).__name__}: {str(exc)[:300]}", flush=True)
        success = False
    finally:
        if page is not None:
            try:
                page.keyboard.press("Alt+F4")
                if proc is not None:
                    proc.wait(timeout=12)
            except Exception:
                pass
        if proc is not None and proc.poll() is None:
            subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"], capture_output=True, timeout=12)
        if browser is not None:
            try:
                browser.close()
            except Exception:
                pass
        if success:
            for target in (project_root, desktop.DATA):
                shutil.rmtree(target, ignore_errors=True)
    passed = len(live.checks)
    report = {"success": success, "failed_stage": None if success else stage, "failed_check": None if success else live.failure_check,
              "passed": passed, "checks": live.checks, "evidence": evidence}
    REPORT.parent.mkdir(exist_ok=True)
    REPORT.write_text(json.dumps(report, ensure_ascii=False, indent=2, default=str), encoding="utf-8")
    print(json.dumps({"success": success, "passed": passed, "report": str(REPORT)}, ensure_ascii=False))
    if not success:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
