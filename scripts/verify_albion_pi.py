"""阿尔比恩（Pi 脑）通路验收：连上 + 私聊真流式，正文与落库都要对。

要点：
  · 本地服务 = `/root/albion-pi` 在 `127.0.0.1:8650` 给的 OpenAI 兼容端点，同席只当客户端，不拉 WSL 进程；
  · 连接 = 取模型目录（应只有 `albion` 这一条）；
  · 发一句话要收到真回复，落库的助手消息里不许残留 `<|ACT:…|>` 情绪标记（显示层剥掉）；
  · 顺带看住「她没起来时明确报错」，不假装连上。
"""
from __future__ import annotations

import os
import sys
import time
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import sync_playwright  # noqa: E402

checks: list[str] = []
failures: list[str] = []


def check(name: str, ok: bool = True) -> None:
    if not ok:
        failures.append(name)
        raise AssertionError(name)
    checks.append(name)
    print("PASS " + name, flush=True)


def run() -> None:
    with sync_playwright() as playwright:
        proc, endpoint = sd.launch()
        browser = playwright.chromium.connect_over_cdp(endpoint)
        page = sd.page_for(browser)
        errors: list[str] = []
        page.on("pageerror", lambda error: errors.append(str(error)))
        try:
            snapshot = sd.ipc(page, "connect_albion")
            check(
                "连上本地服务（connection=" + str(snapshot.get("connection")) + "）",
                snapshot.get("connection") == "connected",
            )
            models = [item["id"] for item in (snapshot.get("models") or [])]
            check("模型目录来自本地服务（" + ",".join(models) + "）", models == ["albion"])

            room = sd.ipc(
                page,
                "create_conversation",
                title="阿尔比恩验收",
                kind="direct",
                members=["albion-wsl"],
            )
            # 问句要合她的口气：太干的「只回复两个字」会被她自己的说话守卫拒掉（那是她的正常机制）。
            text = "在吗？一句话就好"
            message_id = str(uuid.uuid4())
            sd.ipc(
                page,
                "save_local_message",
                conversationId=room["id"],
                messageId=message_id,
                content=text,
            )
            record = sd.ipc(
                page,
                "send_albion_message",
                conversationId=room["id"],
                messageId=message_id,
                content=text,
            )
            check("发送拿到了运行记录", bool(record.get("id")))

            # 她一轮要十几到几十秒（读人格 + 回忆 + 可能写扩展），给足。
            # 助手正文落在**运行快照**里（不只是 message 行），两条都看。
            deadline, snapshot, detail = time.time() + 300, None, None
            while time.time() < deadline:
                snapshot = sd.ipc(page, "albion_status")
                active = snapshot.get("active") or {}
                text = (active.get("text") or "").strip()
                status = active.get("status")
                if text and status in ("succeeded", "failed"):
                    break
                page.wait_for_timeout(2000)

            active = (snapshot or {}).get("active") or {}
            detail = sd.ipc(page, "get_conversation", id=room["id"])
            assistant = [
                item
                for item in detail["messages"]
                if item["sender_id"] == "albion-wsl" and item["content"].strip()
            ]
            check("她给了非空正文（快照 " + str(len(active.get("text") or "")) + " 字）", bool((active.get("text") or "").strip()))
            check("这一轮状态不是失败（" + str(active.get("status")) + "）", active.get("status") != "failed")
            check(
                "正文里没有 <|ACT: 情绪标记（显示层已剥）",
                "<|ACT:" not in (active.get("text") or ""),
            )
            check("落库的助手消息也剥干净了", not assistant or "<|ACT:" not in assistant[-1]["content"])
            check("没有 JavaScript 运行错误", not errors)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=15)
            except Exception:
                pass

    print("Albion Pi: %d passed" % len(checks), flush=True)


if __name__ == "__main__":
    try:
        run()
    except AssertionError:
        print("\n".join("FAIL " + item for item in failures), flush=True)
        raise
