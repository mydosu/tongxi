"""真实模型验收：同一成员的多个席位并行协作 + 项目成员续接已有原生会话（v1.0）。

隔离数据目录跑打包好的 EXE，调用真实的 Codex / DSH / Hermes（会消耗少量额度）：
  1. 每组一个项目群，调度用 Codex；成员 X 再开一个席位 X-2，原话让两者各答一道独立的小算术题。
     验证：调度把任务派给两个席位、两个席位在同一时刻都处于运行中（并行）、答案都对。
  2. Codex 私聊里让它记住一个暗号 → 删掉这条私聊（释放原生会话）→ 绑到项目里的 codex-win-2 席位 →
     原话让该席位说出暗号。验证：续接的是那条原生会话（答得出暗号）。
不改项目文件：任务都是 reply 类型。
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import sync_playwright  # noqa: E402

REPORT = "seats-live-verification.json"
# 调度用哪位成员（某位额度用完时可以换）；续接测试用哪位成员的原生会话。
COORDINATOR = os.environ.get("AGENT_HUB_LIVE_COORDINATOR_AGENT", "codex-win")
RESUME = os.environ.get("AGENT_HUB_LIVE_RESUME_AGENT", "codex-win")
KEYS = {"codex-win": "codex", "dsh-win": "dsh", "hermes-win": "hermes", "claude-win": "claude"}
NAMES = {"codex-win": "Codex", "dsh-win": "DSH", "hermes-win": "Hermes", "claude-win": "Claude"}
LIVE = ("starting", "running", "cancelling")


def main() -> int:
    if len(sys.argv) > 1:
        sd.EXE = Path(sys.argv[1]).resolve()
    only = set(sys.argv[2].split(",")) if len(sys.argv) > 2 else {"codex", "dsh", "hermes", "resume"}
    sd.DATA = sd.SCRATCH / "agenthub-runs" / f"seats-live-{uuid.uuid4().hex[:10]}"
    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})
        print(("PASS " if ok else "FAIL ") + name + ("" if detail is None else f" · {json.dumps(detail, ensure_ascii=False)[:400]}"), flush=True)

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

        def connect(key: str) -> str:
            state = (call(f"connect_{key}").get("value") or {}).get("connection")
            deadline = time.monotonic() + 120
            while state != "connected" and time.monotonic() < deadline:
                time.sleep(1)
                state = sd.ipc(page, f"{key}_status").get("connection")
            return state

        def project_room(title: str, members: list[str]) -> str:
            root = sd.DATA.parent / f"{sd.DATA.name}-{title}"
            root.mkdir(parents=True, exist_ok=True)
            room = sd.ipc(page, "create_conversation", title=title, kind="group", members=members)["id"]
            project = sd.ipc(page, "register_project", name=title, root=str(root))
            sd.ipc(page, "bind_project", conversationId=room, projectId=project["id"])
            sd.ipc(page, "set_project_coordinator", conversationId=room, coordinator={"agent": COORDINATOR, "model": None, "effort": None})
            return room

        def run(room: str, text: str, watch: tuple[str, str] | None = None, limit: int = 1200):
            started = call("start_project", conversationId=room, messageId=str(uuid.uuid4()), content=text)
            if not started["ok"]:
                return None, started["error"], False
            workflow_id = started["value"]["id"]
            both = False
            deadline = time.monotonic() + limit
            job = started["value"]
            while time.monotonic() < deadline:
                job = next((item for item in sd.ipc(page, "project_status") if item["id"] == workflow_id), None) or sd.ipc(page, "get_conversation", id=room)["workflows"][0]
                if watch:
                    running = {attempt["agent_id"] for attempt in job["attempts"] if attempt["status"] in LIVE and attempt.get("activity") != "coordinate"}
                    both = both or set(watch) <= running
                if job["status"] in ("completed", "failed", "interrupted"):
                    break
                time.sleep(1)
            return job, None, both

        # ---- 1. 同一成员两个席位并行 ----
        groups = [("codex", "codex-win", "Codex"), ("dsh", "dsh-win", "DSH"), ("hermes", "hermes-win", "Hermes")]
        check(f"coordinator {COORDINATOR} connected", connect(KEYS[COORDINATOR]) == "connected")
        # Hermes 用同席里设的默认模型（隔离库里没有，按环境变量补上）。
        if os.environ.get("AGENT_HUB_LIVE_HERMES_MODEL"):
            connect("hermes")
            call("set_agent_default_model", agentId="hermes-win", model=os.environ["AGENT_HUB_LIVE_HERMES_MODEL"])
        for key, agent, name in groups:
            if key not in only:
                continue
            if connect(key) != "connected":
                check(f"{name} connected", False)
                continue
            # 项目群至少两位技术成员：被测成员 + 调度（同一位时再补一位）。
            members = list(dict.fromkeys([agent, COORDINATOR, "claude-win" if agent == "dsh-win" else "dsh-win"]))
            room = project_room(f"seats-{key}", members)
            seat = call("add_project_seat", conversationId=room, agentId=agent, label="二号")
            check(f"{name}: second seat opened", seat.get("value") == f"{agent}-2", seat)
            text = (f"{name} 和 {name}二号（{agent}-2）各自独立回答一道题，互不依赖，请同时进行，都不要修改任何文件：\n"
                    f"{name} 回答：17 加 25 等于几？只回复数字。\n{name}二号（{agent}-2）回答：33 加 48 等于几？只回复数字。")
            job, error, both = run(room, text, watch=(agent, f"{agent}-2"))
            if error:
                check(f"{name}: project started", False, error)
                continue
            tasks = {task["agent_id"]: task for task in job["tasks"]}
            check(f"{name}: coordinator routed work to both seats", agent in tasks and f"{agent}-2" in tasks, [(t["agent_id"], t["status"]) for t in job["tasks"]])
            check(f"{name}: both seats were running at the same time", both)
            outputs = {seat_id: task["output"] for seat_id, task in tasks.items()}
            check(f"{name}: both seats answered correctly", "42" in outputs.get(agent, "") and "81" in outputs.get(f"{agent}-2", ""), outputs)
            check(f"{name}: workflow completed", job["status"] == "completed", job.get("error"))

        # ---- 2. 项目席位续接一条已有的原生会话 ----
        if "resume" in only:
            code = f"青柠-{uuid.uuid4().int % 9000 + 1000}"
            name, key = NAMES[RESUME], KEYS[RESUME]
            connect(key)
            direct = sd.ipc(page, "create_conversation", title="记暗号", kind="direct", members=[RESUME])["id"]
            sent = call(f"send_{key}_message", conversationId=direct, messageId=str(uuid.uuid4()), content=f"请记住这个暗号：{code}。现在只回复「记住了」。")
            check(f"{name} private chat started", sent["ok"], sent.get("error"))
            deadline = time.monotonic() + 300
            native = None
            while time.monotonic() < deadline:
                detail = sd.ipc(page, "get_conversation", id=direct)
                native = next((s["native_session_id"] for s in detail["sessions"] if s["agent_id"] == RESUME), None)
                done = any(m["sender_id"] == RESUME and m["status"] == "completed" for m in detail["messages"])
                if native and done:
                    break
                time.sleep(1)
            check(f"{name} remembered the code in its own native session", bool(native), native)
            removed = call("delete_conversation", id=direct)
            check("private chat removed to release the native session", removed["ok"], removed.get("error"))
            room = project_room("resume", list(dict.fromkeys([RESUME, COORDINATOR, "codex-win"]))[:3])
            seat = f"{RESUME}-2"
            call("add_project_seat", conversationId=room, agentId=RESUME, label="二号")
            bound = call("bind_project_sessions", conversationId=room, bindings=[{"agentId": seat, "target": "member", "nativeSessionId": native, "nativeCwd": None}])
            check("seat bound to the existing native session", bound["ok"], bound.get("error"))
            job, error, _ = run(room, f"{name}二号（{seat}）直接回答：我之前让你记住的暗号是什么？只回复暗号本身，不要修改任何文件。")
            output = " ".join(task["output"] for task in (job or {}).get("tasks", []) if task["agent_id"] == seat)
            check("bound seat continues the old conversation (recalls the code)", code in output, {"code": code, "output": output[:200], "error": error})
            attempt = next((a for a in (job or {}).get("attempts", []) if a["agent_id"] == seat), {})
            check("seat ran on that same native thread", attempt.get("native_thread_id") == native, attempt.get("native_thread_id"))
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
