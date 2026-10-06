"""共用真实会话库验收：同席当四个 agent 的共同桌面端。

目标（用户定义）：同一个会话库、同一条会话——同席里建的，她自己的客户端能看到；
她自己客户端里的历史会话，同席能列出来并接上去继续聊。

做法：桥在本进程内放宽 ACP 适配器对 `source='acp'` 的过滤（不改她的安装、不改库里的 source），
跟着 hermes_state 的 INTERNAL_LISTING_SOURCES 走，不会把 oneshot/tool/kanban 翻出来。

真实库（动手前已各自快照 state.db.bak-agenthub-20261006）：
  · Windows Hermes：%LOCALAPPDATA%/hermes/state.db
  · 阿尔比恩：/root/.hermes/profiles/albion/state.db
"""
from __future__ import annotations

import base64
import hashlib
import json
import os
import re
import sqlite3
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

os.environ.setdefault("AGENT_HUB_DSH_INSTALLATION", "D:/AI/dsh/bin/node_modules/@deepseek-ai/dsh")

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import sync_playwright  # noqa: E402

REPORT = "shared-sessions-verification.json"
HERMES_DB = Path(os.path.expanduser("~/AppData/Local/hermes/state.db"))
HERMES_HOME = Path(os.path.expanduser("~/AppData/Local/hermes"))
ALBION_DB = "/root/.hermes/profiles/albion/state.db"
ALBION_PROFILE = "/root/.hermes/profiles/albion"
PROMPT = "只回两个字：在的。"


def wsl(script: str) -> str:
    payload = base64.b64encode(script.encode()).decode()
    result = subprocess.run(
        ["wsl.exe", "-d", "Ubuntu", "--", "bash", "-lc", f"echo {payload} | base64 -d > /tmp/v.py && python3 /tmp/v.py; rm -f /tmp/v.py"],
        capture_output=True, text=True, encoding="utf-8", errors="replace",
    )
    return (result.stdout or "") + (result.stderr or "")


def hermes_session(session_id: str):
    connection = sqlite3.connect(f"file:{HERMES_DB}?mode=ro", uri=True)
    row = connection.execute("SELECT source, message_count FROM sessions WHERE id=?", (session_id,)).fetchone()
    connection.close()
    return row


def hermes_total() -> int:
    connection = sqlite3.connect(f"file:{HERMES_DB}?mode=ro", uri=True)
    total = connection.execute("SELECT count(*) FROM sessions").fetchone()[0]
    connection.close()
    return total


def albion_sessions() -> list[tuple[str, str, int]]:
    out = wsl(
        "import sqlite3\n"
        f"con = sqlite3.connect('file:{ALBION_DB}?mode=ro', uri=True)\n"
        "for r in con.execute(\"SELECT id,source,message_count FROM sessions WHERE source NOT IN ('oneshot','tool','kanban') ORDER BY last_activity_at ASC\"):\n"
        "    print('|'.join(str(x) for x in r))\n"
    ).strip()
    rows = []
    for line in out.splitlines():
        parts = line.split("|")
        if len(parts) == 3:
            try:
                rows.append((parts[0], parts[1], int(parts[2])))
            except ValueError:
                continue
    return rows


def identity_hashes() -> dict:
    mine = {}
    for name in ("SOUL.md", "config.yaml"):
        path = HERMES_HOME / name
        if path.is_file():
            mine[name] = hashlib.md5(path.read_bytes()).hexdigest()
    theirs = wsl(
        "import hashlib,pathlib\n"
        f"for name in ('SOUL.md','config.yaml'):\n"
        f"    p = pathlib.Path('{ALBION_PROFILE}')/name\n"
        "    print(name, hashlib.md5(p.read_bytes()).hexdigest() if p.is_file() else 'missing')\n"
    ).strip()
    return {"hermes": mine, "albion": theirs}


def main() -> int:
    exe = sys.argv[1]
    sd.EXE = sd.ROOT / os.path.relpath(exe, sd.ROOT) if not os.path.isabs(exe) else Path(exe)
    sd.DATA = sd.ARTIFACTS / f"shared-sessions-{uuid.uuid4().hex[:12]}"
    sd.ISOLATED_SESSION_DB = False  # 这一轮验的就是「共用真实库」

    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    before_hashes = identity_hashes()
    total_before = hermes_total()

    # 只验「新会话能不能正常用」：不动她任何一条已有会话（阿尔比恩那边的尤其不碰）。
    # 记下她现有会话的指纹，结束前比对，确认我们只新增、没改动任何一条老会话。
    albion_before = {row[0]: (row[1], row[2]) for row in albion_sessions()}

    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    browser = playwright.chromium.connect_over_cdp(endpoint)
    page = sd.page_for(browser)

    def wait_connected(agent: str) -> str:
        deadline = time.monotonic() + 180
        state = sd.ipc(page, f"connect_{agent}").get("connection")
        while state == "connecting" and time.monotonic() < deadline:
            time.sleep(2)
            state = sd.ipc(page, f"{agent}_status").get("connection")
        return state

    check("hermes.connected", wait_connected("hermes") == "connected")
    check("albion.connected", wait_connected("albion") == "connected")

    # 1) 能列出她的真实历史（含不是同席建的）
    hermes_sessions = sd.ipc(page, "native_sessions", agentId="hermes-win") or []
    check("hermes.lists_real_history", len(hermes_sessions) >= 10, {"listed": len(hermes_sessions), "db_total": total_before})
    # 她自己的会话 id 形如 20260812_013448_e5328c；同席/Tauri 建的是 uuid。
    real_history = [s["id"] for s in hermes_sessions if re.match(r"^\d{8}_\d{6}_", s["id"])]
    check("hermes.list_includes_her_own_history", len(real_history) > 0, real_history[:3])
    albion_list = sd.ipc(page, "native_sessions", agentId="albion-wsl") or []
    check("albion.panel_alive_and_lists", isinstance(albion_list, list), len(albion_list))

    # 2) 同席新建的会话就落进她的真实库，并且能在同席里列出来、接着聊
    fresh = sd.ipc(page, "create_conversation", title="共用会话验收", kind="direct", members=["hermes-win"])["id"]
    sd.ipc(page, "send_hermes_message", conversationId=fresh, messageId=str(uuid.uuid4()), content=PROMPT)
    deadline = time.monotonic() + 300
    status = "timeout"
    while time.monotonic() < deadline:
        detail = sd.ipc(page, "get_conversation", id=fresh)
        replies = [m for m in detail["messages"] if m["sender_id"] == "hermes-win"]
        if replies and replies[-1]["status"] not in ("streaming", "pending", "local_only"):
            status = replies[-1]["status"]
            break
        time.sleep(2)
    check("fresh.turn_finished", status == "completed", status)
    fresh_id = next((s["native_session_id"] for s in sd.ipc(page, "get_conversation", id=fresh)["sessions"] if s["agent_id"] == "hermes-win"), None)
    fresh_row = hermes_session(fresh_id) if fresh_id else None
    check("fresh.written_to_real_db", fresh_row is not None and fresh_row[0] == "acp", {"id": fresh_id, "row": fresh_row})
    check("fresh.visible_in_panel", any(s["id"] == fresh_id for s in (sd.ipc(page, "native_sessions", agentId="hermes-win") or [])), fresh_id)

    # 接着聊：在同一条会话里再发一轮（占用规则是用户定的——同一条原生会话只挂一处，
    # 所以这里就在原会话里继续，而不是另开一个同席会话把它抢过来）。
    sd.ipc(page, "send_hermes_message", conversationId=fresh, messageId=str(uuid.uuid4()), content="只回一个字：好。")
    deadline = time.monotonic() + 300
    status = "timeout"
    while time.monotonic() < deadline:
        detail = sd.ipc(page, "get_conversation", id=fresh)
        replies = [m for m in detail["messages"] if m["sender_id"] == "hermes-win"]
        if replies and replies[-1]["status"] not in ("streaming", "pending", "local_only"):
            status = replies[-1]["status"]
            break
        time.sleep(2)
    check("resume.turn_finished", status == "completed", status)
    after = hermes_session(fresh_id)
    check("resume.same_session_grew", after is not None and after[1] > (fresh_row[1] if fresh_row else 0), {"before": fresh_row, "after": after})
    check("resume.binding_kept", next((s["native_session_id"] for s in sd.ipc(page, "get_conversation", id=fresh)["sessions"] if s["agent_id"] == "hermes-win"), None) == fresh_id)

    # 3) Windows 侧 Hermes 的**既有会话**可以直接接上续聊（阿尔比恩那边不碰）
    connection = sqlite3.connect(f"file:{HERMES_DB}?mode=ro", uri=True)
    legacy = connection.execute(
        "SELECT id, source, message_count FROM sessions WHERE source='desktop' AND message_count BETWEEN 4 AND 60 ORDER BY last_activity_at ASC LIMIT 1"
    ).fetchone()
    connection.close()
    check("hermes.has_legacy_session", legacy is not None, legacy)

    legacy_chat = sd.ipc(page, "create_conversation", title="接她的既有会话", kind="direct", members=["hermes-win"])["id"]
    legacy_attached = page.evaluate(
        """async ({ p }) => { try { await window.__TAURI_INTERNALS__.invoke('attach_native_session', p); return { ok: true }; } catch (e) { return { ok: false, error: String(e).slice(0, 160) }; } }""",
        {"p": {"conversationId": legacy_chat, "agentId": "hermes-win", "nativeSessionId": legacy[0], "nativeCwd": None}},
    )
    check("attach.her_legacy_session_ok", legacy_attached.get("ok") is True, legacy_attached.get("error"))
    sd.ipc(page, "send_hermes_message", conversationId=legacy_chat, messageId=str(uuid.uuid4()), content="只回两个字：在的。")
    deadline = time.monotonic() + 300
    status = "timeout"
    while time.monotonic() < deadline:
        detail = sd.ipc(page, "get_conversation", id=legacy_chat)
        replies = [m for m in detail["messages"] if m["sender_id"] == "hermes-win"]
        if replies and replies[-1]["status"] not in ("streaming", "pending", "local_only"):
            status = replies[-1]["status"]
            break
        time.sleep(2)
    check("legacy.turn_finished", status == "completed", status)
    legacy_after = hermes_session(legacy[0])
    check("legacy.same_session_grew", legacy_after is not None and legacy_after[1] > legacy[2], {"before": legacy[2], "after": legacy_after[1] if legacy_after else None})
    check("legacy.source_untouched", legacy_after is not None and legacy_after[0] == "desktop", legacy_after[0] if legacy_after else None)
    check("legacy.binding_kept", next((s["native_session_id"] for s in sd.ipc(page, "get_conversation", id=legacy_chat)["sessions"] if s["agent_id"] == "hermes-win"), None) == legacy[0])

    # 4) 阿尔比恩：她的库里也能列到 + 新会话落进去
    albion_chat = sd.ipc(page, "create_conversation", title="阿尔比恩共用库", kind="direct", members=["albion-wsl"])["id"]
    sd.ipc(page, "send_albion_message", conversationId=albion_chat, messageId=str(uuid.uuid4()), content="用一句话确认你在线。")
    deadline = time.monotonic() + 300
    status = "timeout"
    while time.monotonic() < deadline:
        detail = sd.ipc(page, "get_conversation", id=albion_chat)
        replies = [m for m in detail["messages"] if m["sender_id"] == "albion-wsl"]
        if replies and replies[-1]["status"] not in ("streaming", "pending", "local_only"):
            status = replies[-1]["status"]
            break
        time.sleep(2)
    check("albion.turn_finished", status == "completed", status)
    albion_after = {row[0]: (row[1], row[2]) for row in albion_sessions()}
    check("albion.new_session_in_real_db", len(albion_after) > len(albion_before), {"before": len(albion_before), "after": len(albion_after)})
    # 她自己的 gateway（hermes -p albion gateway run）正在使用 albion-desktop，消息数会动，
    # 所以这里只断言我们**没有重标/删除**她已有的会话（source 指纹不变）。
    relabelled = [sid for sid, row in albion_before.items() if sid in albion_after and albion_after[sid][0] != row[0]]
    removed = [sid for sid in albion_before if sid not in albion_after]
    check("albion.existing_sessions_not_relabelled", not relabelled and not removed, {"relabelled": relabelled[:3], "removed": removed[:3]})

    # 5) 身份文件一字未改（写保护只放开了会话库）
    after_hashes = identity_hashes()
    check("identity.unchanged", before_hashes == after_hashes, {"before": before_hashes, "after": after_hashes})

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
        "touched_real_sessions": {"hermes_new": fresh_id, "hermes_legacy_continued": legacy[0] if legacy else None},
    }
    sd.ARTIFACTS.joinpath(REPORT).write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    for item in checks:
        print(("PASS" if item["ok"] else "FAIL"), item["name"], str(item["detail"])[:130])
    print("SharedSessions: " + result["passed"] + " passed")
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
