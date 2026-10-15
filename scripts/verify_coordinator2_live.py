"""真实模型验收：调度 2.0（v1.1）。

隔离数据目录跑打包好的 EXE，只用 Claude（会消耗少量额度），项目群里只有 Claude 一位成员，调度也是 Claude：
  1. 普通派活：答一道算术题。验证成员和调度的 token 用量、会话上下文大小都记下了，计划账本写了，分派消息带理由。
  2. 临时会话：原话要求新开临时会话。验证任务跑在调度新开的席位上，协作结束后这个临时席位被撤掉。
  3. 自选模型：原话只说「用最便宜的模型」，不点名型号。验证调度按价格表自己选了 Haiku。
不改项目文件：任务都是 reply 类型。
"""
from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import sync_playwright  # noqa: E402

REPORT = "coordinator2-live-verification.json"
AGENT = "claude-win"


def main() -> int:
    if len(sys.argv) > 1:
        sd.EXE = Path(sys.argv[1]).resolve()
    only = set(sys.argv[2].split(",")) if len(sys.argv) > 2 else {"basic", "fresh", "cheap"}
    sd.DATA = sd.SCRATCH / "agenthub-runs" / f"coordinator2-live-{uuid.uuid4().hex[:10]}"
    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})
        print(("PASS " if ok else "FAIL ") + name + ("" if detail is None else f" · {json.dumps(detail, ensure_ascii=False)[:500]}"), flush=True)

    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    try:
        browser = playwright.chromium.connect_over_cdp(endpoint)
        page = sd.page_for(browser)

        def call(command: str, **payload):
            return page.evaluate(
                """async ({ command, payload }) => { try { return { ok: true, value: await window.__TAURI_INTERNALS__.invoke(command, payload) }; } catch (error) { return { ok: false, error: String(error) }; } }""",
                {"command": command, "payload": payload},
            )

        state = (call("connect_claude").get("value") or {}).get("connection")
        deadline = time.monotonic() + 120
        while state != "connected" and time.monotonic() < deadline:
            time.sleep(1)
            state = sd.ipc(page, "claude_status").get("connection")
        check("Claude connected", state == "connected", state)

        def project_room(title: str) -> str:
            root = sd.DATA.parent / f"{sd.DATA.name}-{title}"
            root.mkdir(parents=True, exist_ok=True)
            room = sd.ipc(page, "create_conversation", title=title, kind="group", members=[AGENT])["id"]
            project = sd.ipc(page, "register_project", name=title, root=str(root))
            sd.ipc(page, "bind_project", conversationId=room, projectId=project["id"])
            sd.ipc(page, "set_project_coordinator", conversationId=room, coordinator={"agent": AGENT, "model": None, "effort": None})
            sd.ipc(page, "set_project_preference", conversationId=room, preference="save")
            return room

        def run(room: str, text: str, limit: int = 900):
            started = call("start_project", conversationId=room, messageId=str(uuid.uuid4()), content=text)
            if not started["ok"]:
                return None, started["error"]
            workflow_id = started["value"]["id"]
            deadline = time.monotonic() + limit
            job = started["value"]
            while time.monotonic() < deadline:
                job = next((item for item in sd.ipc(page, "get_conversation", id=room)["workflows"] if item["id"] == workflow_id), job)
                if job["status"] in ("completed", "failed", "interrupted"):
                    break
                time.sleep(2)
            return job, None

        def dispatch_notes(job: dict) -> list[str]:
            return [m["content"] for m in job.get("team_messages", []) if "调度已分派" in m.get("content", "")]

        if "basic" in only:
            room = project_room("coord2-basic")
            check("preference saved", sd.ipc(page, "get_conversation", id=room).get("project_preference") == "save")
            job, error = run(room, "Claude 回答：17 加 25 等于几？只回复数字，不要修改任何文件。")
            check("basic: workflow completed", bool(job) and job["status"] == "completed", error or (job or {}).get("error"))
            if job:
                outputs = " ".join(task["output"] for task in job["tasks"])
                check("basic: answer correct", "42" in outputs, outputs[:200])
                member = [a for a in job["attempts"] if a.get("activity") != "coordinate" and a["status"] == "completed"]
                manager = [a for a in job["attempts"] if a.get("activity") == "coordinate"]
                usage = (member[-1].get("usage") if member else None) or {}
                check("basic: member usage recorded", usage.get("input", 0) > 0 and usage.get("output", 0) > 0, usage)
                check("basic: member session context recorded", usage.get("context", 0) > 0, usage.get("context"))
                check("basic: coordinator usage recorded", any((a.get("usage") or {}).get("input", 0) > 0 for a in manager), [a.get("usage") for a in manager])
                ledger = (job.get("coordination") or {}).get("ledger")
                check("basic: coordinator wrote a plan ledger", bool(ledger and ledger.strip()), ledger)
                check("basic: dispatch message explains the choice", bool(dispatch_notes(job)), dispatch_notes(job))
                check("basic: user's reply format wins (no summary appended)", "交付摘要" not in outputs, outputs[-200:])

        if "fresh" in only:
            room = project_room("coord2-fresh")
            job, error = run(room, "请新开一个临时会话（fresh session），让 Claude 在那个新会话里回答：33 加 48 等于几？只回复数字，不要修改任何文件。")
            check("fresh: workflow completed", bool(job) and job["status"] == "completed", error or (job or {}).get("error"))
            if job:
                seats = {task["agent_id"] for task in job["tasks"]}
                check("fresh: task ran on a new temporary seat", any(seat.startswith(f"{AGENT}-") for seat in seats), sorted(seats))
                outputs = " ".join(task["output"] for task in job["tasks"])
                check("fresh: answer correct", "81" in outputs, outputs[:200])
                check("fresh: member does not try to rearrange sessions itself", "会话" not in outputs, outputs[:200])
                db = sqlite3.connect(sd.DATA / "hub.db")
                left = db.execute("SELECT seat_id FROM project_seats WHERE conversation_id=? AND temporary_for IS NOT NULL", (room,)).fetchall()
                db.close()
                check("fresh: temporary seat released after the workflow", not left, left)
                check("fresh: dispatch marks the new session", any("临时会话" in note for note in dispatch_notes(job)), dispatch_notes(job))

        if "cheap" in only:
            room = project_room("coord2-cheap")
            job, error = run(room, "Claude 回答：法国的首都是哪座城市？只回复城市名，不要修改任何文件。这种小问题请用最便宜的模型。")
            check("cheap: workflow completed", bool(job) and job["status"] == "completed", error or (job or {}).get("error"))
            if job:
                models = [task.get("model") for task in job["tasks"]]
                check("cheap: coordinator picked the cheapest model (Haiku) on its own", any(m and "haiku" in m.lower() for m in models), {"models": models, "notes": dispatch_notes(job)})
                outputs = " ".join(task["output"] for task in job["tasks"])
                check("cheap: answer correct", "巴黎" in outputs or "Paris" in outputs, outputs[:200])
                check("cheap: member does not discuss switching models", "模型" not in outputs, outputs[:200])
    finally:
        try:
            playwright.stop()
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
    passed = all(item["ok"] for item in checks)
    (sd.ARTIFACTS / REPORT).write_text(json.dumps({"passed": passed, "checks": checks}, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"{'PASSED' if passed else 'FAILED'} {sum(c['ok'] for c in checks)}/{len(checks)}")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
