"""项目会话绑定 + 磨砂主题验收（v0.22.0）。

隔离数据目录里跑打包好的 EXE：
  · 新建项目时，在「各成员的会话」里给 Codex 成员挑一个它已有的原生会话，其余保持新建；
  · 项目中途打开「会话绑定」，把 Codex 换回新建、把调度管家绑到一个会话，再验证落库；
  · 截图主界面、两个弹窗和阿尔比恩私聊，确认主题与立绘渲染。
只读列出 Codex 已有会话、只改隔离库里的映射，不发任何消息。
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

from playwright.sync_api import expect, sync_playwright  # noqa: E402

REPORT = "project-sessions-verification.json"


def main() -> int:
    if len(sys.argv) > 1:
        sd.EXE = Path(sys.argv[1]).resolve()
    sd.DATA = sd.SCRATCH / "agenthub-runs" / f"project-sessions-{uuid.uuid4().hex[:10]}"
    # 项目目录不能落在同席数据目录里，放到旁边。
    root = sd.DATA.parent / f"{sd.DATA.name}-project"
    root.mkdir(parents=True, exist_ok=True)
    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})
        print(("PASS " if ok else "FAIL ") + name + ("" if detail is None else f" · {detail}"), flush=True)

    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    try:
        browser = playwright.chromium.connect_over_cdp(endpoint)
        page = sd.page_for(browser)
        page.set_viewport_size({"width": 1440, "height": 900})
        shot = lambda name: page.screenshot(path=str(sd.ARTIFACTS / f"project-sessions-{name}.png"))
        page.wait_for_timeout(600)
        shot("empty")
        check("theme backdrop rendered", page.locator(".theme-backdrop .bg-layer.visible").count() == 1)

        state = sd.ipc(page, "connect_codex").get("connection")
        deadline = time.monotonic() + 90
        while state == "connecting" and time.monotonic() < deadline:
            time.sleep(1)
            state = sd.ipc(page, "codex_status").get("connection")
        check("codex connected", state == "connected", state)
        codex_sessions = sd.ipc(page, "native_sessions", agentId="codex-win", conversationId=None)
        free = [s for s in codex_sessions if not s.get("occupied_by")]
        check("codex lists existing native sessions", len(free) >= 3, len(free))

        room = sd.ipc(page, "create_conversation", title="会话绑定验收", kind="group", members=["codex-win", "hermes-win", "claude-win"])
        page.evaluate("() => location.reload()")
        page.wait_for_timeout(1200)
        sd.choose(page, room["id"])
        page.locator("#bind-project").click()
        expect(page.locator("#session-bind-section")).to_be_visible()
        page.locator("#project-name").fill("会话绑定验收项目")
        page.locator("#project-root").fill(str(root))
        rows = page.locator(".session-bind-row")
        check("new-project dialog lists every member plus the coordinator", rows.count() == 4, rows.count())
        codex_row = page.locator('.session-bind-row[data-bind-agent="codex-win"][data-bind-target="member"] select')
        expect(codex_row.locator(f'option[value="{free[0]["id"]}"]')).to_have_count(1, timeout=20000)
        codex_row.select_option(free[0]["id"])
        # 同一成员再开一个会话（席位），绑到另一个已有会话。
        page.locator('.session-bind-row[data-bind-agent="codex-win"][data-bind-target="member"] [data-seat-plus]').click()
        seat_row = page.locator('.session-bind-row[data-bind-target="seat"] select')
        expect(seat_row.locator(f'option[value="{free[2]["id"]}"]')).to_have_count(1, timeout=20000)
        seat_row.select_option(free[2]["id"])
        page.wait_for_timeout(400)
        shot("new-project")
        page.locator("#project-form button.primary").click()
        page.wait_for_function("() => !document.querySelector('#modal').open || document.querySelector('#project-form .form-error').textContent", timeout=30000)
        error = page.locator("#project-form .form-error").text_content() if page.evaluate("() => document.querySelector('#modal').open") else ""
        check("new project with session choices saves", not error, error)
        expect(page.locator("#modal")).not_to_have_attribute("open", "", timeout=5000)
        bindings = sd.ipc(page, "project_session_bindings", conversationId=room["id"])
        codex = next(b for b in bindings if b["agent_id"] == "codex-win" and b["target"] == "member")
        check("codex member continues the chosen native session", codex["native_session_id"] == free[0]["id"], codex)
        seat = next((b for b in bindings if b["agent_id"] == "codex-win-2"), None)
        check("extra codex seat opened and bound to its own native session", bool(seat) and seat["native_session_id"] == free[2]["id"], seat)
        others = [b for b in bindings if b["agent_id"] not in ("codex-win", "codex-win-2") or b["target"] != "member"]
        check("unbound members and coordinator stay new", all(b["native_session_id"] is None for b in others), others)
        check("coordinator row present", any(b["target"] == "coordinator" for b in bindings))
        page.wait_for_timeout(500)
        shot("project-room")

        # 项目中途：Codex 换回新建；调度（默认 Hermes→这里改成 Codex 管家）绑另一个已有会话。
        sd.ipc(page, "set_project_coordinator", conversationId=room["id"], coordinator={"agent": "codex-win", "model": None, "effort": None})
        page.locator("[data-project-sessions]").first.click()
        expect(page.locator("#project-session-form")).to_be_visible()
        member_select = page.locator('.session-bind-row[data-bind-agent="codex-win"][data-bind-target="member"] select')
        coordinator_select = page.locator('.session-bind-row[data-bind-target="coordinator"] select')
        expect(coordinator_select.locator(f'option[value="{free[1]["id"]}"]')).to_have_count(1, timeout=20000)
        taken = coordinator_select.locator(f'option[value="{free[0]["id"]}"]')
        check("session bound to a member is shown as occupied for the coordinator", taken.is_disabled())
        member_select.select_option("")
        coordinator_select.select_option(free[1]["id"])
        shot("switch")
        page.locator("#project-session-form button.primary").click()
        page.wait_for_function("() => !document.querySelector('#modal').open || document.querySelector('#project-session-form .form-error').textContent", timeout=30000)
        error = page.locator("#project-session-form .form-error").text_content() if page.evaluate("() => document.querySelector('#modal').open") else ""
        check("mid-project switch saves", not error, error)
        expect(page.locator("#modal")).not_to_have_attribute("open", "", timeout=5000)
        bindings = sd.ipc(page, "project_session_bindings", conversationId=room["id"])
        codex = next(b for b in bindings if b["agent_id"] == "codex-win" and b["target"] == "member")
        coordinator = next(b for b in bindings if b["target"] == "coordinator")
        check("mid-project switch back to a new member session", codex["native_session_id"] is None, codex)
        check("mid-project coordinator binding saved", coordinator["agent_id"] == "codex-win" and coordinator["native_session_id"] == free[1]["id"], coordinator)
        owners = sd.ipc(page, "native_sessions", agentId="codex-win", conversationId=None)
        owner = next((s.get("occupied_by") for s in owners if s["id"] == free[1]["id"]), None)
        check("coordinator binding blocks attaching elsewhere", bool(owner and "调度" in owner), owner)

        # 成员配置：席位单独一行，可以选自己的模型。
        page.locator("[data-project-configure]").first.click()
        expect(page.locator("#project-member-settings")).to_be_visible()
        page.wait_for_timeout(400)
        check("project settings show the extra seat as its own row", page.locator('.project-member-settings-row.seat-row[data-settings-seat="codex-win-2"]').count() == 1)
        name = page.locator('[data-seat-rename="codex-win-2"]')
        name.fill("审稿")
        name.press("Enter")
        expect(page.locator('[data-project-module="codex-win-2"]')).to_contain_text("审稿", timeout=10000)
        check("seat renamed inline and the sidebar follows", True)
        shot("member-settings")
        page.keyboard.press("Escape")
        page.wait_for_timeout(300)
        chats = sd.ipc(page, "get_conversation", id=room["id"])["project_agent_chats"]
        check("renamed seat label is stored", any(c["agent_id"] == "codex-win-2" and c.get("label") == "审稿" for c in chats), chats)
        check("sidebar lists the seat's own chat", page.locator('[data-project-module="codex-win-2"]').count() == 1)

        # 已归档的项目群不再占着目录：可以改名沿用，也可以「解除登记」。
        sd.ipc(page, "archive_conversation", id=room["id"], archived=True)
        renamed = sd.ipc(page, "register_project", name="改个名字", root=str(root))
        check("archived group's directory can be registered under a new name", renamed["name"] == "改个名字", renamed["name"])
        other = sd.ipc(page, "create_conversation", title="解除登记验收", kind="group", members=["codex-win", "hermes-win"])
        page.evaluate("() => location.reload()")
        page.wait_for_timeout(1200)
        sd.choose(page, other["id"])
        page.locator("#bind-project").click()
        page.locator("#project-choice").select_option(renamed["id"])
        release = page.locator("#release-project")
        check("release button shows for a registered project", release.is_visible())
        release.click(); release.click()
        expect(page.locator(f'#project-choice option[value="{renamed["id"]}"]')).to_have_count(0, timeout=10000)
        check("released project leaves the list and pre-fills its directory", page.locator("#project-root").input_value() == str(root), page.locator("#project-root").input_value())
        shot("release-project")
        page.locator("#project-name").fill("重新登记")
        page.locator("#project-form button.primary").click()
        page.wait_for_function("() => !document.querySelector('#modal').open || document.querySelector('#project-form .form-error').textContent", timeout=30000)
        bound = sd.ipc(page, "get_conversation", id=other["id"]).get("project") or {}
        check("freed directory binds as a new project", bound.get("name") == "重新登记" and bound.get("id") != renamed["id"], bound.get("name"))

        # 窄窗口：成员栏改成浮出的抽屉，头像堆照样能开合。
        page.set_viewport_size({"width": 940, "height": 820})
        page.wait_for_timeout(300)
        if page.locator(".chat-layout.members-open").count():
            page.locator("#toggle-members").click()
        page.locator("#toggle-members").click()
        page.wait_for_timeout(450)
        width = page.locator(".members-panel").evaluate("el => getComputedStyle(el).display === 'none' ? 0 : el.getBoundingClientRect().width")
        check("narrow window opens the members drawer", width > 150, width)
        shot("narrow-drawer")
        page.locator("#toggle-members").click()
        page.set_viewport_size({"width": 1440, "height": 900})

        albion = sd.ipc(page, "create_conversation", title="和阿尔比恩聊聊", kind="direct", members=["albion-wsl"])
        page.evaluate("() => location.reload()")
        page.wait_for_timeout(1200)
        sd.choose(page, albion["id"])
        page.wait_for_timeout(500)
        # 伴侣头像来自用户自己的配置（v1.0 起不打包任何角色图片），这里只确认私聊页正常打开。
        check("companion chat opens", page.locator(".chat-heading h1").count() == 1)
        shot("albion")

        # 成员栏：头像堆展开/收起，状态记住。
        page.locator("#toggle-members").click()
        page.wait_for_timeout(400)
        check("member stack opens the members panel", page.locator(".chat-layout.members-open").count() == 1)
        shot("members-open")
        page.locator("#toggle-members").click()

        # 主题：每张背景都能切过去并截图；浓淡写进 --veil。
        page.locator("#theme-link").click()
        expect(page.locator(".theme-picker")).to_be_visible()
        page.wait_for_timeout(500)
        shot("theme-picker")
        ids = page.locator("[data-theme-id]").evaluate_all("els => els.map(el => el.dataset.themeId)")
        check("theme picker lists the public backdrops", ids == ["chenwu", "bohe", "xingguang", "haiyan", "muzi", "plain"], ids)
        page.keyboard.press("Escape")
        for theme in ids:
            page.locator("#theme-link").click()
            page.locator(f'[data-theme-id="{theme}"]').click()
            page.keyboard.press("Escape")
            page.wait_for_timeout(900)
            visible = page.locator(".bg-layer.visible").evaluate_all("els => els.map(el => el.dataset.theme)")
            if theme != ids[0]:
                check(f"theme {theme} applied", len(visible) == 1 and visible[0].startswith(f"{theme}|"), visible)
            shot(f"theme-{theme}")
        page.locator("#theme-link").click()
        page.locator('[data-veil="clear"]').click()
        veil = page.evaluate("() => getComputedStyle(document.documentElement).getPropertyValue('--veil').trim()")
        check("veil control adjusts the backdrop", veil == "0", veil)
        page.locator('[data-theme-id="chenwu"]').click()
        page.locator('[data-veil="medium"]').click()
        page.locator("[data-theme-done]").click()
        check("no page errors", not sd.errors, sd.errors)
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
