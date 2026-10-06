"""原生会话「接入」验收：绑上去要真的续在那个会话里，且一次只能挂一处。

用两段真实数据：
  · codex：只用用户已有的 45 个原生会话做绑定/占用/换绑/释放的机制验证，**不发消息**，不动内容；
  · DSH：在隔离数据目录里真跑两轮（先让它记住一个数字，再换个同席会话问它），
    能答出来就证明接的是原生会话本身，不是靠 Hub 重发上下文（Hub 私聊只发当条消息）。
"""
from __future__ import annotations

import json
import os
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

os.environ.setdefault("AGENT_HUB_DSH_INSTALLATION", "D:/AI/dsh/bin/node_modules/@deepseek-ai/dsh")

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import sync_playwright  # noqa: E402

REPORT = "session-attach-verification.json"
SECRET = "7391"


def main() -> int:
    exe = sys.argv[1]
    sd.EXE = sd.ROOT / os.path.relpath(exe, sd.ROOT) if not os.path.isabs(exe) else Path(exe)
    sd.DATA = sd.ARTIFACTS / f"session-attach-{uuid.uuid4().hex[:12]}"

    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    browser = playwright.chromium.connect_over_cdp(endpoint)
    page = sd.page_for(browser)
    page.evaluate("() => { localStorage.setItem('hub.screen', 'chat'); }")

    def wait_connected(agent: str) -> str:
        deadline = time.monotonic() + 90
        state = sd.ipc(page, f"connect_{agent}").get("connection")
        while state == "connecting" and time.monotonic() < deadline:
            time.sleep(1)
            state = sd.ipc(page, f"{agent}_status").get("connection")
        return state

    check("codex.connected", wait_connected("codex") == "connected")
    check("dsh.connected", wait_connected("dsh") == "connected")

    def call(command: str, **payload):
        return page.evaluate(
            """async ({ command, payload }) => {
              try {
                const value = await window.__TAURI_INTERNALS__.invoke(command, payload);
                return { ok: true, value };
              } catch (error) {
                return { ok: false, error: String(error).slice(0, 200) };
              }
            }""",
            {"command": command, "payload": payload},
        )

    def bind(conversation_id: str, agent: str, native_id: str, native_cwd: str | None = None):
        # 界面点「接入」时会把那一项自己的 cwd 一起带上（原生会话按目录分桶，续聊要原样传回）。
        return call("attach_native_session", conversationId=conversation_id, agentId=agent, nativeSessionId=native_id, nativeCwd=native_cwd)

    def bound_id(conversation_id: str, agent: str) -> str | None:
        detail = sd.ipc(page, "get_conversation", id=conversation_id)
        return next((s["native_session_id"] for s in detail["sessions"] if s["agent_id"] == agent), None)

    def sessions_of(agent: str, conversation_id: str | None = None):
        result = call("native_sessions", agentId=agent, conversationId=conversation_id)
        return result.get("value") or []

    def iconv(title: str, agent: str) -> str:
        return sd.ipc(page, "create_conversation", title=title, kind="direct", members=[agent])["id"]

    # ---------- codex：绑定 / 占用 / 换绑 / 释放（不动用户会话内容）----------
    codex_sessions = sessions_of("codex-win")
    free = [item for item in codex_sessions if not item.get("occupied_by")]
    check("codex.has_free_sessions", len(free) >= 3, len(free))
    first, second, third = (item["id"] for item in free[:3])
    cwd_of = {item["id"]: item.get("cwd") for item in codex_sessions}

    chat_a, chat_b = iconv("接入 A", "codex-win"), iconv("接入 B", "codex-win")
    before = sd.ipc(page, "get_conversation", id=chat_a)["messages"]

    result = bind(chat_a, "codex-win", first, cwd_of[first])
    check("attach.ok", result.get("ok") and bound_id(chat_a, "codex-win") == first, result.get("error"))
    check("attach.messages_kept", len(sd.ipc(page, "get_conversation", id=chat_a)["messages"]) == len(before))

    rebound_self = bind(chat_a, "codex-win", first, cwd_of[first])
    check("attach.idempotent", rebound_self.get("ok") is True, rebound_self.get("error"))

    stolen = bind(chat_b, "codex-win", first, cwd_of[first])
    check("occupancy.blocks_other_chat", stolen.get("ok") is False and "占用" in (stolen.get("error") or ""), stolen.get("error"))

    second_ok = bind(chat_b, "codex-win", second, cwd_of[second])
    check("attach.second_chat_ok", second_ok.get("ok") is True and bound_id(chat_b, "codex-win") == second, second_ok.get("error"))

    rebind = bind(chat_a, "codex-win", third, cwd_of[third])
    check("rebind.ok", rebind.get("ok") is True and bound_id(chat_a, "codex-win") == third, rebind.get("error"))

    listed = {item["id"]: item for item in sessions_of("codex-win", chat_a)}
    check("rebind.frees_previous", not listed[first].get("occupied_by"), listed[first].get("occupied_by"))
    check("list.marks_current", listed[third].get("current") is True, listed[third])
    check("list.marks_other_owner", listed[second].get("occupied_by") == "接入 B", listed[second].get("occupied_by"))
    check("attach.records_native_cwd",
          sd.ipc(page, "get_conversation", id=chat_a)["sessions"][0]["native_cwd"] == cwd_of[third],
          sd.ipc(page, "get_conversation", id=chat_a)["sessions"][0]["native_cwd"])

    empty = bind(chat_b, "codex-win", "   ")
    check("attach.rejects_empty", empty.get("ok") is False, empty.get("error"))
    absent_agent = bind(chat_b, "dsh-win", third)
    check("attach.rejects_absent_member", absent_agent.get("ok") is False and "成员" in (absent_agent.get("error") or ""), absent_agent.get("error"))

    # ---------- DSH：真的续在原会话里（隔离目录，两轮真实模型）----------
    memory_chat = iconv("记忆 A", "dsh-win")
    sd.ipc(page, "send_dsh_message", conversationId=memory_chat, messageId=str(uuid.uuid4()), content=f"请记住这个数字：{SECRET}。只回复两个字：记住。")

    def wait_reply(conversation_id: str, agent: str, timeout: float = 300) -> tuple[str, str]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            detail = sd.ipc(page, "get_conversation", id=conversation_id)
            replies = [m for m in detail["messages"] if m["sender_id"] == agent]
            if replies and replies[-1]["status"] not in ("streaming", "pending", "local_only"):
                return replies[-1]["status"], replies[-1]["content"]
            time.sleep(2)
        return "timeout", ""

    status, _ = wait_reply(memory_chat, "dsh-win")
    check("dsh.first_turn_finished", status == "completed", status)

    # DSH 的会话记录就在应用自己的数据目录里（桶名是 cwd，文件名是 session id），
    # 测试用隔离目录，直接读它来验「换个同席会话还能续上」。
    def dsh_bucket(workspace: str):
        slug = workspace.replace(":", "").replace("/", "-").replace("\\", "-")
        return sd.DATA / "dsh-native" / "sessions" / f"--{slug}--"

    def dsh_session_in(workspace: str) -> str | None:
        bucket = dsh_bucket(workspace)
        if not bucket.is_dir():
            return None
        markers = sorted(bucket.iterdir(), key=lambda item: item.stat().st_mtime, reverse=True)
        return markers[0].name if markers else None

    workspace_a = str(sd.DATA / "dsh-workspaces" / memory_chat)
    native = dsh_session_in(workspace_a)
    check("dsh.session_on_disk", bool(native), native)
    # Hub 自己建的会话，runtime 会用 bind_thread 记在会话行上（不靠接入）。
    check("dsh.auto_bound_own_session", bound_id(memory_chat, "dsh-win") == native, bound_id(memory_chat, "dsh-win"))

    recall_chat = iconv("记忆 B", "dsh-win")
    check("dsh.occupancy_blocks", bind(recall_chat, "dsh-win", native, workspace_a).get("ok") is False)

    # 先把 记忆 A 换绑到另一个原生会话（catalog），把 S_A 让出来，再从 记忆 B 接它。
    catalog = dsh_session_in(str(sd.DATA / "dsh-workspaces" / "catalog"))
    check("dsh.catalog_session_exists", bool(catalog), catalog)
    freed = bind(memory_chat, "dsh-win", catalog, str(sd.DATA / "dsh-workspaces" / "catalog"))
    check("dsh.rebind_to_catalog", freed.get("ok") is True, freed.get("error"))
    check("dsh.rebind_keeps_messages", len(sd.ipc(page, "get_conversation", id=memory_chat)["messages"]) == 2,
          len(sd.ipc(page, "get_conversation", id=memory_chat)["messages"]))

    attached = bind(recall_chat, "dsh-win", native, workspace_a)
    check("dsh.attach_ok", attached.get("ok") is True, attached.get("error"))
    check("dsh.cwd_recorded",
          sd.ipc(page, "get_conversation", id=recall_chat)["sessions"][0]["native_cwd"] == workspace_a,
          sd.ipc(page, "get_conversation", id=recall_chat)["sessions"][0]["native_cwd"])

    sd.ipc(page, "send_dsh_message", conversationId=recall_chat, messageId=str(uuid.uuid4()), content="我刚才让你记住的数字是几？只回复数字。")
    status, reply = wait_reply(recall_chat, "dsh-win")
    check("dsh.resumed_turn_finished", status == "completed", status)
    check("dsh.native_continuity", SECRET in reply, reply[:80])
    check("dsh.binding_kept", bound_id(recall_chat, "dsh-win") == native, bound_id(recall_chat, "dsh-win"))

    # ---------- 界面：从列表里点「接入」----------
    ui_chat = iconv("界面接入", "codex-win")
    page.evaluate("id => localStorage.setItem('hub.selected', id)", ui_chat)
    page.reload()
    page.locator("#message-input").wait_for(state="visible")
    page.locator("#native-sessions").click()
    page.wait_for_function("() => !document.querySelector('.native-session-list')?.textContent.includes('正在读取')", timeout=30000)
    page.locator(".native-session [data-attach]").first.click()
    page.wait_for_function(
        "() => [...document.querySelectorAll('.native-session-state')].some(node => node.textContent.includes('当前会话'))",
        timeout=30000,
    )
    check("ui.attach_updates_binding", bound_id(ui_chat, "codex-win") is not None, bound_id(ui_chat, "codex-win"))
    states = page.evaluate("() => [...document.querySelectorAll('.native-session-state')].map(node => node.textContent)")
    check("ui.shows_current_state", any("当前会话" in text for text in states), states[:3])

    try:
        browser.close()
    except Exception:
        pass
    playwright.stop()
    proc.terminate()

    result = {
        "success": all(item["ok"] for item in checks),
        "passed": f"{sum(1 for item in checks if item['ok'])}/{len(checks)}",
        "checks": checks,
        "test_data_directory": str(sd.DATA),
    }
    sd.ARTIFACTS.joinpath(REPORT).write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    for item in checks:
        print(("PASS" if item["ok"] else "FAIL"), item["name"], str(item["detail"])[:110])
    print("SessionAttach: " + result["passed"] + " passed")
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
