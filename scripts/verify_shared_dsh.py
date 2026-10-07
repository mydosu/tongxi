"""DSH 共用真实会话库验收：同席当 DSH 的共同桌面端。

目标（用户定义）：同一个会话库、同一条会话——同席里建的 DSH 会话，她自己的 DSH 客户端能看到；
她自己客户端里的历史会话，同席能列出来并接上去继续聊。

做法：桥把 DSH_HOME 指向她真实的 ~/.dsh、持久化根指向 ~/.dsh/sessions、
编码对齐她客户端的 zstd（隔离模式下保持不变）。动手前已快照 ~/.dsh.bak-agenthub-20261006。

真实库：C:/Users/<user>/.dsh（sessions/<cwd>/session-<uuid>/session.jsonl.zstd + storages/session_projcache）
"""
from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

os.environ.pop("AGENT_HUB_DSH_HOME", None)  # 这一轮验的就是「共用真实家目录」

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import sync_playwright  # noqa: E402

REPORT = "shared-dsh-verification.json"
DSH_HOME = Path(os.path.expanduser("~/.dsh"))
BACKUP = Path(os.path.expanduser("~/.dsh.bak-agenthub-20261006"))
PROMPT = "只回两个字：在的。"


def store_sessions() -> dict[str, dict]:
    """她真实库里的会话：id → {path, size, cwd}。"""
    found: dict[str, dict] = {}
    sessions_root = DSH_HOME / "sessions"
    if not sessions_root.is_dir():
        return found
    for directory in sessions_root.iterdir():
        if not directory.is_dir():
            continue
        for item in directory.iterdir():
            # 两套命名：她自己客户端写 `session-<uuid>`，同席新建的是裸 `<uuid>`，两种都算。
            if not item.is_dir():
                continue
            logs = [f for f in item.iterdir() if f.name.startswith("session.jsonl")]
            if not logs:
                continue
            log = logs[0]
            found[item.name] = {"path": str(log), "size": log.stat().st_size, "cwd": directory.name}
    return found


def file_hashes() -> dict[str, str]:
    out = {}
    for name in (".credentials.yaml", "settings.yaml"):
        path = DSH_HOME / name
        out[name] = hashlib.md5(path.read_bytes()).hexdigest() if path.is_file() else "missing"
    return out


def main() -> int:
    exe = sys.argv[1]
    sd.EXE = sd.ROOT / os.path.relpath(exe, sd.ROOT) if not os.path.isabs(exe) else Path(exe)
    sd.DATA = sd.ARTIFACTS / f"shared-dsh-{uuid.uuid4().hex[:12]}"
    sd.ISOLATED_SESSION_DB = False  # 验的就是共用真实库

    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    before = store_sessions()
    hashes_before = file_hashes()
    check("backup.exists", BACKUP.is_dir(), str(BACKUP))
    check("store.has_history", len(before) >= 5, len(before))

    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    browser = playwright.chromium.connect_over_cdp(endpoint)
    page = sd.page_for(browser)

    def wait_connected(agent: str) -> str:
        deadline = time.monotonic() + 240
        state = sd.ipc(page, f"connect_{agent}").get("connection")
        while state == "connecting" and time.monotonic() < deadline:
            time.sleep(2)
            state = sd.ipc(page, f"{agent}_status").get("connection")
        return state

    def wait_turn(conversation: str) -> str:
        deadline = time.monotonic() + 420
        status = "timeout"
        while time.monotonic() < deadline:
            detail = sd.ipc(page, "get_conversation", id=conversation)
            replies = [m for m in detail["messages"] if m["sender_id"] == "dsh-win"]
            if replies and replies[-1]["status"] not in ("streaming", "pending", "local_only"):
                status = replies[-1]["status"]
                break
            time.sleep(2)
        return status

    try:
        check("dsh.connected", wait_connected("dsh") == "connected")

        # 1) 能列出她自己客户端建的历史会话
        listed = sd.ipc(page, "native_sessions", agentId="dsh-win") or []
        ids = [item["id"] for item in listed]
        check("dsh.lists_her_history", len(ids) >= 5, {"listed": len(ids), "store": len(before)})
        # 对比**当下**的库：启动时新建的 catalog 会话本来就该在列表里（before 是启动前的快照）。
        current = store_sessions()
        check("dsh.list_from_real_store", bool(ids) and all(i in current for i in ids), [i for i in ids if i not in current][:3])
        check("dsh.list_has_cwd", all(item.get("cwd") for item in listed), [item.get("cwd") for item in listed][:2])
        # 同席自己建的会话（裸 uuid）也在同一份列表里——本进程正在用的那一条会被适配器过滤，
        # 同一个 store 的其它客户端照样看得到。
        hub_made = [i for i in ids if not i.startswith("session-")]
        check("dsh.list_includes_hub_made", len(hub_made) > 0, hub_made[:3])

        # 2) 同席新建的 DSH 会话落进她真实库（她的客户端就能看到）
        fresh = sd.ipc(page, "create_conversation", title="DSH 共用验收", kind="direct", members=["dsh-win"])["id"]
        sd.ipc(page, "send_dsh_message", conversationId=fresh, messageId=str(uuid.uuid4()), content=PROMPT)
        check("fresh.turn_finished", wait_turn(fresh) == "completed")
        fresh_id = next((s["native_session_id"] for s in sd.ipc(page, "get_conversation", id=fresh)["sessions"] if s["agent_id"] == "dsh-win"), None)
        after_new = store_sessions()
        check("fresh.lands_in_real_store", bool(fresh_id) and fresh_id in after_new, {"id": fresh_id, "new": sorted(set(after_new) - set(before))[:3]})
        # 她客户端读的是同一个 store：新会话已落库即可见（同席自己那条因为正被本进程使用，
        # ACP 列表会先隐藏它，重启后同样列得出来）。
        check("fresh.seen_by_store_api", bool(fresh_id) and fresh_id in store_sessions(), fresh_id)

        # 3) 接她自己的一条历史会话继续聊（挑最小的，少打扰）
        candidates = {i: v for i, v in before.items() if i.startswith("session-")}
        target = min(candidates, key=lambda i: candidates[i]["size"]) if candidates else None
        check("resume.has_target", target is not None, target)
        # 原生会话按工作目录分桶，续聊必须带它自己的 cwd（面板也是这么给的）
        target_cwd = next((item.get("cwd") for item in listed if item["id"] == target), None)
        chat = sd.ipc(page, "create_conversation", title="接 DSH 既有会话", kind="direct", members=["dsh-win"])["id"]
        attached = page.evaluate(
            """async ({ p }) => { try { await window.__TAURI_INTERNALS__.invoke('attach_native_session', p); return { ok: true }; } catch (e) { return { ok: false, error: String(e).slice(0, 160) }; } }""",
            {"p": {"conversationId": chat, "agentId": "dsh-win", "nativeSessionId": target, "nativeCwd": target_cwd}},
        )
        check("attach.her_session_ok", attached.get("ok") is True, attached.get("error"))
        sd.ipc(page, "send_dsh_message", conversationId=chat, messageId=str(uuid.uuid4()), content="只回两个字：在的。")
        check("resume.turn_finished", wait_turn(chat) == "completed")
        after_resume = store_sessions()
        grew = target in after_resume and target in before and after_resume[target]["size"] > before[target]["size"]
        check("resume.same_session_grew", grew, {"before": before.get(target, {}).get("size"), "after": after_resume.get(target, {}).get("size")})
        check("resume.still_zstd", target in after_resume and after_resume[target]["path"].endswith(".jsonl.zstd"), after_resume.get(target, {}).get("path"))
        check("resume.only_that_one_touched",
              all(after_resume[i]["size"] == v["size"] for i, v in before.items() if i != target),
              [i for i, v in before.items() if i != target and after_resume.get(i, {}).get("size") != v["size"]][:3])
        check("store.no_session_lost", set(before) <= set(after_resume), sorted(set(before) - set(after_resume))[:3])
        check("identity.files_unchanged", file_hashes() == hashes_before, {"before": hashes_before, "after": file_hashes()})
    finally:
        try:
            playwright.stop()
        except Exception:
            pass
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except Exception:
            proc.kill()

    passed = sum(1 for item in checks if item["ok"])
    for item in checks:
        print(f"{'PASS' if item['ok'] else 'FAIL'} {item['name']} {item['detail']}")
    print(f"SharedDsh: {passed}/{len(checks)} passed")
    (sd.ARTIFACTS / REPORT).write_text(json.dumps(
        {"passed": passed, "total": len(checks), "checks": checks,
         "touched_real_sessions": {"created": fresh_id, "continued": target}}, ensure_ascii=False, indent=2), encoding="utf-8")
    return 0 if passed == len(checks) else 1


if __name__ == "__main__":
    raise SystemExit(main())
