"""思考块「边想边看」验收：Codex 真跑一轮，核对推理事件是否进入消息并正确展示。

断言：思考确实产生 → 流式期间块保持展开、标签「思考中…」、内容在增长 → 结束后收成一行「思考过程」。
"""
from __future__ import annotations

import json
import os
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))


import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import sync_playwright  # noqa: E402

REPORT = "thought-stream-verification.json"
PROMPT = "先在心里逐步推理（至少三步），最后才给答案：一个 3 升桶和一个 5 升桶，怎么量出恰好 4 升水？"
TARGETS = [("codex", "codex-win")]

SAMPLE = """() => {
  const block = document.querySelector('.message .thought');
  if (!block) return null;
  const body = block.querySelector('.thought-body')?.textContent || '';
  return { open: block.open, length: body.length, summary: block.querySelector('summary')?.textContent || '' };
}"""


def main() -> int:
    exe = sys.argv[1]
    sd.EXE = sd.ROOT / os.path.relpath(exe, sd.ROOT) if not os.path.isabs(exe) else Path(exe)
    sd.DATA = sd.ARTIFACTS / f"thought-stream-{uuid.uuid4().hex[:12]}"

    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    browser = playwright.chromium.connect_over_cdp(endpoint)
    page = sd.page_for(browser)
    page.evaluate("() => localStorage.setItem('hub.screen','chat')")

    for key, agent in TARGETS:
        state = sd.ipc(page, f"connect_{key}").get("connection")
        deadline = time.monotonic() + 150
        while state == "connecting" and time.monotonic() < deadline:
            time.sleep(2)
            state = sd.ipc(page, f"{key}_status").get("connection")
        check(f"{key}.connected", state == "connected", state)
        if state != "connected":
            continue

        conversation = sd.ipc(page, "create_conversation", title=f"思考流式验收 {agent}", kind="direct", members=[agent])["id"]
        sd.ipc(page, "set_session_settings", id=conversation, agentId=agent, model=None, reasoningEffort="high")
        page.evaluate("id => localStorage.setItem('hub.selected', id)", conversation)
        page.reload()
        page.locator("#message-input").wait_for(state="visible")

        sd.ipc(page, f"send_{key}_message", conversationId=conversation, messageId=str(uuid.uuid4()), content=PROMPT)

        live = {"samples": 0, "open": 0, "max": 0, "grew": 0, "wrong_label": 0}
        detail = None
        deadline = time.monotonic() + 300
        while time.monotonic() < deadline:
            detail = sd.ipc(page, "get_conversation", id=conversation)
            replies = [m for m in detail["messages"] if m["sender_id"] == agent]
            if replies and replies[-1]["status"] not in ("streaming", "pending", "local_only"):
                break
            sampled = page.evaluate(SAMPLE)
            if sampled:
                live["samples"] += 1
                if sampled["open"]:
                    live["open"] += 1
                    if not sampled["summary"].startswith("思考中"):
                        live["wrong_label"] += 1
                    if sampled["length"] > live["max"]:
                        live["max"] = sampled["length"]
                        live["grew"] += 1
            time.sleep(0.25)

        replies = [m for m in (detail or {}).get("messages", []) if m["sender_id"] == agent]
        stored = len((replies[-1].get("thought") or "")) if replies else 0
        check(f"{key}.thought_produced", stored > 0, stored)
        check(f"{key}.streamed_open", live["open"] >= 2, live)
        check(f"{key}.label_while_thinking", live["open"] > 0 and live["wrong_label"] == 0, live)
        check(f"{key}.grew_while_streaming", live["grew"] >= 2 and live["max"] > 0, live)

        page.wait_for_timeout(1200)
        done = page.evaluate(SAMPLE)

        check(f"{key}.collapsed_when_done", bool(done) and done["open"] is False, done)
        check(f"{key}.label_when_done", bool(done) and done["summary"] == "思考过程", done)
        check(f"{key}.kept_full_text", bool(done) and done["length"] == stored, {"dom": done, "stored": stored})

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
    print("ThoughtStream: " + result["passed"] + " passed")
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
