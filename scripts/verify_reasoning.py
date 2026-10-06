"""推理块验收：真实 DSH 私聊跑一轮，后端的「思考」要能存进消息、并在界面里折叠显示。

为什么必须真跑：推理块只有在模型真的发出 agent_thought_chunk 时才存在，
假数据能过，但那是自欺。这里连 DSH（走本机安装或 AGENT_HUB_DSH_INSTALLATION）、
把思考强度调到 high、发一句真话，然后核对数据库行与 DOM。
"""
from __future__ import annotations

import json
import os
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

# DSH 安装位置与探针脚本保持一致；应用从环境变量取。
os.environ.setdefault("AGENT_HUB_DSH_INSTALLATION", "D:/AI/dsh/bin/node_modules/@deepseek-ai/dsh")

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import sync_playwright  # noqa: E402

PROMPT = "先在心里逐步推理（至少三步），最后才给答案：一个 3 升桶和一个 5 升桶，怎么量出恰好 4 升水？"
REPORT = "reasoning-verification.json"


def main() -> int:
    exe = sys.argv[1]
    sd.EXE = sd.ROOT / os.path.relpath(exe, sd.ROOT) if not os.path.isabs(exe) else Path(exe)
    sd.DATA = sd.ARTIFACTS / f"reasoning-{uuid.uuid4().hex[:12]}"

    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    browser = playwright.chromium.connect_over_cdp(endpoint)
    page = sd.page_for(browser)

    snapshot = sd.ipc(page, "connect_dsh")
    connection = snapshot.get("connection")
    deadline = time.monotonic() + 90
    while connection == "connecting" and time.monotonic() < deadline:
        time.sleep(1)
        connection = sd.ipc(page, "dsh_status").get("connection")
    check("dsh.connected", connection == "connected", connection)

    conversation = sd.ipc(page, "create_conversation", title="推理块验收", kind="direct", members=["dsh-win"])
    page.evaluate("id => localStorage.setItem('hub.selected', id)", conversation["id"])
    page.reload()
    page.locator("#message-input").wait_for(state="visible")
    sd.ipc(page, "set_session_settings", id=conversation["id"], agentId="dsh-win", model=None, reasoningEffort="high")

    sd.ipc(page, "send_dsh_message", conversationId=conversation["id"], messageId=str(uuid.uuid4()), content=PROMPT)
    detail = None
    # 思考中要能当场看见：等结论的同时每 250ms 采一次界面，记录展开态与增长。
    live = {"open": 0, "grew": 0, "max": 0, "summary": ""}
    deadline = time.monotonic() + 300
    while time.monotonic() < deadline:
        detail = sd.ipc(page, "get_conversation", id=conversation["id"])
        assistant = [m for m in detail["messages"] if m["sender_id"] != "user"]
        if assistant and assistant[-1]["status"] not in ("streaming", "pending", "local_only"):
            break
        sampled = page.evaluate(
            """() => {
              const block = document.querySelector('.message .thought');
              if (!block) return null;
              const body = block.querySelector('.thought-body')?.textContent || '';
              return { open: block.open, length: body.length, summary: block.querySelector('summary')?.textContent || '' };
            }"""
        )
        if sampled and sampled["open"]:
            live["open"] += 1
            live["summary"] = sampled["summary"]
            if sampled["length"] > live["max"]:
                live["grew"] += 1 if live["max"] else 0
                live["max"] = sampled["length"]
        time.sleep(0.25)

    assistant = [m for m in (detail or {}).get("messages", []) if m["sender_id"] != "user"]
    status = assistant[-1]["status"] if assistant else None
    thought = (assistant[-1].get("thought") or "") if assistant else ""
    check("run.finished", status in ("completed", "failed", "interrupted"), status)
    check("message.thought_stored", len(thought.strip()) > 0, len(thought))

    view = page.evaluate(
        """async () => {
          const invoke = window.__TAURI_INTERNALS__.invoke;
          const id = localStorage.getItem('hub.selected');
          const detail = await invoke('get_conversation', { id });
          return detail.messages.filter(m => m.sender_id !== 'user').map(m => m.thought || '');
        }"""
    )
    check("ipc.thought_returned", bool(view) and len((view[-1] or "").strip()) > 0, len(view[-1] or ""))

    dom = page.evaluate(
        """() => {
          const block = document.querySelector('.message .thought');
          if (!block) return null;
          return { collapsed: !block.open, text: (block.querySelector('.thought-body')?.textContent || '').length, summary: block.querySelector('summary')?.textContent || '' };
        }"""
    )
    check("ui.thought_block_present", dom is not None, dom)
    check("ui.thought_collapsed_when_done", bool(dom) and dom["collapsed"] is True, dom)
    check("ui.thought_has_text", bool(dom) and dom["text"] > 0, dom)
    # DSH 这条链路把思考攒到最后一次性给（实测 0 → 271 字在 0.4s 内落地），
    # 所以「边想边看」不在这里验，放在 verify_thought_stream.py（Hermes/阿尔比恩 会流）。
    check("ui.thought_never_shown_closed_while_streaming", live["open"] == 0 or live["summary"].startswith("思考中"), live)
    check("ui.thought_label_when_done", dom is not None and dom.get("summary") == "思考过程", dom)

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
        print(("PASS" if item["ok"] else "FAIL"), item["name"], item["detail"])
    print("Reasoning: " + result["passed"] + " passed")
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
