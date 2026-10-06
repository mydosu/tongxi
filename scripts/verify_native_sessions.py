"""原生会话列表验收（只读阶段）：各成员自己的历史会话要能被列出来，且不惊动它。

要点：
  · codex 走 app-server 的 thread/list，DSH/Hermes/Albion 走 ACP 的 session/list；
  · 未连接的成员要给出明确错误，不是崩掉或空列表糊过去；
  · 界面上的「原生会话」按钮要真的弹出只读列表。
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

REPORT = "native-sessions-verification.json"


def main() -> int:
    exe = sys.argv[1]
    sd.EXE = sd.ROOT / os.path.relpath(exe, sd.ROOT) if not os.path.isabs(exe) else Path(exe)
    sd.DATA = sd.ARTIFACTS / f"native-sessions-{uuid.uuid4().hex[:12]}"

    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    browser = playwright.chromium.connect_over_cdp(endpoint)
    page = sd.page_for(browser)

    def wait_connected(agent: str) -> str:
        deadline = time.monotonic() + 90
        state = sd.ipc(page, f"connect_{agent}").get("connection")
        while state == "connecting" and time.monotonic() < deadline:
            time.sleep(1)
            state = sd.ipc(page, f"{agent}_status").get("connection")
        return state

    check("codex.connected", wait_connected("codex") == "connected")
    check("dsh.connected", wait_connected("dsh") == "connected")

    def list_sessions(agent_id: str):
        return page.evaluate(
            """async (agentId) => {
              try {
                const value = await window.__TAURI_INTERNALS__.invoke('native_sessions', { agentId });
                return { ok: true, sessions: value };
              } catch (error) {
                return { ok: false, error: String(error).slice(0, 160) };
              }
            }""",
            agent_id,
        )

    codex = list_sessions("codex-win")
    check("codex.list_ok", bool(codex.get("ok")), codex.get("error"))
    codex_sessions = codex.get("sessions") or []
    check("codex.has_history", len(codex_sessions) > 0, len(codex_sessions))
    check(
        "codex.entries_well_formed",
        all(isinstance(item.get("id"), str) and item["id"] for item in codex_sessions),
        codex_sessions[0] if codex_sessions else None,
    )

    dsh = list_sessions("dsh-win")
    check("dsh.list_ok", bool(dsh.get("ok")), dsh.get("error"))
    check("dsh.returns_list", isinstance(dsh.get("sessions"), list), len(dsh.get("sessions") or []))

    hermes = list_sessions("hermes-win")
    check("hermes.offline_is_clear_error", hermes.get("ok") is False and "未连接" in (hermes.get("error") or ""), hermes.get("error"))

    unknown = list_sessions("nobody")
    check("unknown_member_rejected", unknown.get("ok") is False and "成员不存在" in (unknown.get("error") or ""), unknown.get("error"))

    # 界面：私聊里的「原生会话」按钮要弹出只读列表
    conversation = sd.ipc(page, "create_conversation", title="原生会话验收", kind="direct", members=["codex-win"])
    page.evaluate("id => localStorage.setItem('hub.selected', id)", conversation["id"])
    page.reload()
    page.locator("#message-input").wait_for(state="visible")
    page.locator("#native-sessions").click()
    page.locator(".native-session-list").wait_for(state="visible")
    # 面板先显示「正在读取…」，等它被真实内容替换（codex 那边要查一下它的会话库）。
    page.wait_for_function(
        "() => !document.querySelector('.native-session-list')?.textContent.includes('正在读取')",
        timeout=30000,
    )
    rows = page.evaluate("() => [...document.querySelectorAll('.native-session')].map(node => node.dataset.sessionId || '')")
    check("ui.panel_lists_sessions", len(rows) > 0, len(rows))
    page.locator("#modal-close").click()

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
        print(("PASS" if item["ok"] else "FAIL"), item["name"], str(item["detail"])[:120])
    print("NativeSessions: " + result["passed"] + " passed")
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
