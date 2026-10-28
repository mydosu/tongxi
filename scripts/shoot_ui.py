"""界面截图留档：隔离数据目录里导入一张阿尔比恩背景，造私聊、群聊、项目群，截主要页面。

用法：python scripts/shoot_ui.py <exe> <标签>   截图存到 artifacts/ui-<标签>-*.png
不连接成员、不发消息；只往隔离库写演示用的本地消息。
"""
from __future__ import annotations

import base64
import os
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import expect, sync_playwright  # noqa: E402

PICTURE = Path(os.environ.get("TONGXI_SHOT_BG", r"D:\Albion picture\130689130_p1_master1200.jpg"))


def main() -> int:
    sd.EXE = Path(sys.argv[1]).resolve()
    label = sys.argv[2] if len(sys.argv) > 2 else "now"
    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    try:
        page = sd.page_for(playwright.chromium.connect_over_cdp(endpoint))
        page.set_viewport_size({"width": 1280, "height": 800})
        shot = lambda name: page.screenshot(path=str(sd.ARTIFACTS / f"ui-{label}-{name}.png"))
        if PICTURE.exists():
            data = "data:image/jpeg;base64," + base64.b64encode(PICTURE.read_bytes()).decode()
            bg = sd.ipc(page, "add_background", name="阿尔比恩", mode="cover", image=data, thumb=data if len(data) < 300_000 else "data:image/png;base64,iVBORw0KGgo=")
            page.evaluate("id => localStorage.setItem('hub.theme', JSON.stringify({ id: 'custom:' + id, veil: 'medium' }))", bg)
        say = lambda room, text: sd.ipc(page, "save_local_message", conversationId=room, messageId=str(uuid.uuid4()), content=text)
        direct = sd.ipc(page, "create_conversation", title="与 Codex 私聊", kind="direct", members=["codex-win"])
        for text in ["帮我看看成员面板的动画为什么没生效", "窄窗口下面板是 display 切换的，过渡不会触发。改成始终浮层、用 translate 和 opacity 过渡就好。", "好，顺便把点外部收起也做了"]:
            say(direct["id"], text)
        group = sd.ipc(page, "create_conversation", title="周末计划", kind="group", members=["codex-win", "dsh-win", "hermes-win"])
        say(group["id"], "这周把安卓端的方案定下来")
        project = sd.ipc(page, "create_conversation", title="阿尔比恩重构优化", kind="group", members=["codex-win", "claude-win"])
        page.evaluate("() => location.reload()")
        page.wait_for_timeout(1500)
        shot("home")
        sd.choose(page, direct["id"])
        page.wait_for_timeout(800)
        shot("direct")
        page.locator("#toggle-members").click()
        page.wait_for_timeout(600)
        shot("direct-panel")
        page.keyboard.press("Escape")
        sd.choose(page, group["id"])
        page.wait_for_timeout(800)
        shot("group")
        page.locator("#toggle-members").click()
        page.wait_for_timeout(600)
        shot("group-panel")
        page.keyboard.press("Escape")
        root = Path(tempfile.mkdtemp(prefix="tongxi-shot-"))
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        sd.choose(page, project["id"])
        page.locator("#bind-project").click()
        page.locator("#project-name").fill("阿尔比恩重构优化")
        page.locator("#project-root").fill(str(root))
        page.locator("#project-form button.primary").click()
        expect(page.locator("#modal")).not_to_have_attribute("open", "", timeout=30000)
        page.wait_for_timeout(1000)
        shot("project")
        page.locator("[data-project-configure]").click()
        page.wait_for_timeout(1500)
        shot("project-panel")
        page.keyboard.press("Escape")
        page.locator("#service-link").click()
        page.wait_for_timeout(1200)
        shot("services")
        page.locator("#theme-link").click()
        page.wait_for_timeout(700)
        shot("theme")
        page.evaluate("() => document.querySelector('#modal').open && document.querySelector('#modal').close()")
        page.locator("#back-to-chat").click()
        page.wait_for_timeout(500)
        page.locator("#new-conversation").click()
        page.wait_for_timeout(600)
        shot("create")
        print("errors:", sd.errors, flush=True)
        return 0
    finally:
        playwright.stop()
        proc.terminate()
        proc.wait(timeout=10)


if __name__ == "__main__":
    raise SystemExit(main())
