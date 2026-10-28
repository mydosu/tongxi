"""成员面板验收：滑出动画、点面板外收起、私聊/群聊/项目群三种面板内容、私聊工作文件夹。

隔离数据目录里跑（默认打包 EXE，传参可换成 debug exe）；不连接成员、不发消息。
"""
from __future__ import annotations

import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import expect, sync_playwright  # noqa: E402


def main() -> int:
    if len(sys.argv) > 1:
        sd.EXE = Path(sys.argv[1]).resolve()
    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    try:
        browser = playwright.chromium.connect_over_cdp(endpoint)
        page = next(p for c in browser.contexts for p in c.pages if "1420" in p.url or "localhost" in p.url)
        page.on("pageerror", lambda error: sd.errors.append(str(error)))
        expect(page.locator("#new-conversation")).to_be_enabled(timeout=20000)
        page.set_viewport_size({"width": 900, "height": 760})
        shot = lambda name: page.screenshot(path=str(sd.ARTIFACTS / f"members-panel-{name}.png"))
        panel = page.locator(".members-panel")
        def choose(conversation_id):
            # 换会话有翻页动画，新旧两页会短暂并存；等动画放完再操作。
            page.locator(f'[data-conversation="{conversation_id}"]').click()
            page.wait_for_function("id => localStorage.getItem('hub.selected') === id", arg=conversation_id)
            page.wait_for_timeout(900)
        opened = lambda: page.evaluate("() => !!document.querySelector('.chat-layout.members-open')")

        direct = sd.ipc(page, "create_conversation", title="面板私聊", kind="direct", members=["codex-win"])
        page.evaluate("() => location.reload()")
        page.wait_for_timeout(1200)
        choose(direct["id"])
        page.locator("#toggle-members").click()
        page.wait_for_timeout(80)
        mid = float(panel.evaluate("el => getComputedStyle(el).opacity"))
        sd.check("panel animates in (mid-transition opacity)", 0 < mid < 1)
        page.wait_for_timeout(500)
        sd.check("panel open", opened() and panel.is_visible())
        expect(page.locator(".members-body #cwd-form")).to_be_visible()
        shot("direct")
        folder = tempfile.mkdtemp(prefix="tongxi-cwd-")
        page.locator("#cwd-form input").fill(folder)
        page.locator("#cwd-form button.primary").click()
        page.wait_for_timeout(600)
        saved = sd.ipc(page, "get_conversation", id=direct["id"])["sessions"][0]
        sd.check("direct chat folder saved", saved.get("native_cwd") == folder)
        page.locator("#cwd-form [data-cwd-reset]").click()
        page.wait_for_timeout(600)
        sd.check("direct chat folder reset", sd.ipc(page, "get_conversation", id=direct["id"])["sessions"][0].get("native_cwd") is None)
        page.mouse.click(120, 400)
        page.wait_for_timeout(450)
        sd.check("click outside closes panel", not opened() and not panel.is_visible())
        page.locator("#toggle-members").click()
        page.wait_for_timeout(450)
        page.keyboard.press("Escape")
        page.wait_for_timeout(450)
        sd.check("Escape closes panel", not opened())

        group = sd.ipc(page, "create_conversation", title="面板群聊", kind="group", members=["codex-win", "dsh-win"])
        page.evaluate("() => location.reload()")
        page.wait_for_timeout(1200)
        choose(group["id"])
        page.locator("#toggle-members").click()
        page.wait_for_timeout(500)
        sd.check("group panel lists member checkboxes", page.locator(".members-body [name=member]").count() >= 2)
        shot("group")
        page.mouse.click(120, 400)
        page.wait_for_timeout(450)

        root = Path(tempfile.mkdtemp(prefix="tongxi-proj-"))
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        page.locator("#bind-project").click()
        page.locator("#project-name").fill("面板项目")
        page.locator("#project-root").fill(str(root))
        page.locator("#project-form button.primary").click()
        expect(page.locator("#modal")).not_to_have_attribute("open", "", timeout=30000)
        page.wait_for_timeout(800)
        page.locator("[data-project-configure]").click()
        expect(page.locator("#project-member-settings")).to_be_visible(timeout=15000)
        page.wait_for_timeout(600)
        sd.check("project panel shows seat cards with session pickers", page.locator(".members-body .seat-row").count() >= 2 and page.locator(".members-body [data-seat-session] select").count() >= 2)
        shot("project")
        page.locator(".members-body summary", has_text="权限").click()
        page.locator(".members-body [data-project-grants]").fill("面板验收")
        page.locator("#project-member-settings button.primary").click()
        page.wait_for_timeout(1500)
        sd.check("project panel saves without connected members", page.locator(".toast").last.text_content().find("已保存") >= 0)
        sd.check("panel stays open after save", opened())
        page.locator("#service-link").click()
        page.wait_for_timeout(900)
        page.locator("#workspace-form input").fill(folder)
        page.locator("#workspace-form button").click()
        page.wait_for_timeout(500)
        page.wait_for_function("() => document.querySelector('.toast')?.textContent.includes('默认工作文件夹')", timeout=5000)
        sd.check("default work folder saved", sd.ipc(page, "default_workspace") == folder)
        shot("services")
        sd.check("no page errors", not sd.errors)
        print(f"PASS {len(sd.checks)} checks", flush=True)
        return 0
    finally:
        playwright.stop()
        proc.terminate()
        proc.wait(timeout=10)


if __name__ == "__main__":
    raise SystemExit(main())
