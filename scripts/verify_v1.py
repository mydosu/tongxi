"""同席 v1.0 新功能验收（打包好的 EXE，隔离数据目录）。

  · 接入成员：启动时自动扫描并接入本机已装的成员；接入横条与每张卡的安装状态、安装确认弹窗（不真的装）；
  · 陪伴：本地起一个模拟的 OpenAI 兼容服务，走界面配置名字/头像/地址/Key/人设 → 测试连接 → 保存 →
    私聊发消息拿到流式回复；验证 Key 只以 Bearer 发给服务、库里没有明文、人设作为 system 消息发出；
  · 主题：内置公用背景、导入自己的图片（多选）、切换立绘/铺满、删除。
不安装任何东西、不调用任何真实模型。
"""
from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import expect, sync_playwright  # noqa: E402

REPORT = "v1-verification.json"
SECRET = "sk-verify-" + uuid.uuid4().hex[:8]
SEEN: list[dict] = []


class Companion(BaseHTTPRequestHandler):
    def log_message(self, *args):  # 安静
        pass

    def _record(self, body: str = ""):
        SEEN.append({"path": self.path, "auth": self.headers.get("Authorization"), "body": body})

    def do_GET(self):
        self._record()
        if self.path != "/v1/models":
            self.send_response(404); self.end_headers(); return
        payload = json.dumps({"data": [{"id": "companion-mini"}, {"id": "companion-pro"}]}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(payload))); self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0))).decode()
        self._record(body)
        self.send_response(200); self.send_header("Content-Type", "text/event-stream"); self.end_headers()
        for piece in ["我在", "，", "一直都在。"]:
            self.wfile.write(f"data: {json.dumps({'choices': [{'delta': {'content': piece}}]}, ensure_ascii=False)}\n\n".encode())
            self.wfile.flush(); time.sleep(0.15)
        self.wfile.write(b"data: [DONE]\n\n")


def main() -> int:
    if len(sys.argv) > 1:
        sd.EXE = Path(sys.argv[1]).resolve()
    sd.DATA = sd.SCRATCH / "agenthub-runs" / f"v1-{uuid.uuid4().hex[:10]}"
    os.environ["AGENT_HUB_AUTOCONNECT"] = "1"
    server = ThreadingHTTPServer(("127.0.0.1", 0), Companion)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    endpoint = f"http://127.0.0.1:{server.server_address[1]}"
    checks: list[dict] = []

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})
        print(("PASS " if ok else "FAIL ") + name + ("" if detail is None else f" · {detail}"), flush=True)

    proc, cdp = sd.launch()
    playwright = sync_playwright().start()
    try:
        browser = playwright.chromium.connect_over_cdp(cdp)
        page = sd.page_for(browser)
        page.set_viewport_size({"width": 1440, "height": 900})
        shot = lambda name: page.screenshot(path=str(sd.ARTIFACTS / f"v1-{name}.png"))

        # ---- 接入成员：自动扫描 + 自动接入 ----
        scan = sd.ipc(page, "scan_agents")
        installed = [item["id"] for item in scan if item["installed"]]
        check("scan finds the agents installed on this machine", len(scan) == 4 and bool(installed), installed)
        deadline = time.monotonic() + 120
        state: dict = {}
        while time.monotonic() < deadline:
            state = {agent: sd.ipc(page, f"{key}_status")["connection"] for agent, key in [("codex-win", "codex"), ("claude-win", "claude"), ("hermes-win", "hermes"), ("dsh-win", "dsh")]}
            if all(state[agent] == "connected" for agent in installed):
                break
            time.sleep(2)
        check("installed agents are connected automatically at startup", all(state.get(agent) == "connected" for agent in installed), state)
        page.locator("#service-link").click()
        expect(page.locator("#setup-center")).to_be_visible()
        expect(page.locator("#setup-center strong")).to_contain_text(f"{len(installed)}/4", timeout=20000)
        check("setup banner reports the scan", True)
        lines = page.locator(".service-card .setup-line").count()
        check("every member card shows its install line", lines == 5, lines)
        missing = [item["id"] for item in scan if not item["installed"]]
        probe = missing[0] if missing else "codex-win"
        page.evaluate("id => document.querySelector(`[data-agent-install=\"${id}\"]`)?.removeAttribute('hidden')", probe)
        page.locator(f'[data-agent-install="{probe}"]').click()
        expect(page.locator(".install-confirm")).to_be_visible()
        command = page.locator(".install-confirm pre").first.text_content()
        expected = next(item["install_command"] for item in scan if item["id"] == probe)
        check("install asks first and shows the exact official command", command == expected, command)
        shot("install-confirm")
        page.locator("[data-install-cancel]").click()
        page.evaluate("id => document.querySelector(`[data-agent-install=\"${id}\"]`)?.setAttribute('hidden', '')", probe)
        # 每张卡同样的结构：三个主按钮 + 「更多」菜单。
        visible = page.locator(".service-card").evaluate_all("cards => cards.map(card => [...card.querySelectorAll('.service-actions > button, .service-actions > .service-more')].filter(el => getComputedStyle(el).display !== 'none').length)")
        check("every member card has the same three main actions", visible == [3] * 5, visible)
        page.locator('.service-card[data-service-id="codex-win"] [data-service-more]').click()
        menu = page.locator('.service-card[data-service-id="codex-win"] .service-menu')
        check("more menu holds setup, docs and isolated updates", menu.is_visible() and "登录 / 初次配置" in menu.inner_text() and "准备更新" in menu.inner_text(), menu.inner_text())
        shot("service-menu")
        page.keyboard.press("Escape")
        # 原生头像：Codex、Claude 打包图标，Hermes、DSH 从本机安装读取；可以换，也能恢复。
        faces = {agent["id"]: (agent.get("avatar") or "")[:22] for agent in sd.ipc(page, "list_agents")}
        check("members start with their native icons", faces["codex-win"].startswith("data:image/svg") and faces["claude-win"].startswith("data:image/svg") and all(faces[i].startswith("data:image/") for i in installed), faces)
        page.locator('[data-avatar-edit="codex-win"]').set_input_files(str(next(Path(r"D:\Albion picture").glob("*.jpg"))))
        page.wait_for_function("() => true")
        deadline = time.monotonic() + 15
        face = ""
        while time.monotonic() < deadline and not face.startswith("data:image/webp"):
            face = next(a.get("avatar") or "" for a in sd.ipc(page, "list_agents") if a["id"] == "codex-win"); time.sleep(0.5)
        check("member avatar can be changed", face.startswith("data:image/webp"), face[:30])
        # 纯色头像：点头像弹出面板，点色块即换，字取名字首字。
        page.locator('[data-avatar-menu="codex-win"]').click()
        expect(page.locator('.service-card[data-service-id="codex-win"] .avatar-picker')).to_be_visible()
        shot("avatar-picker")
        page.locator('.service-card[data-service-id="codex-win"] [data-avatar-color="#8b80c4"]').click()
        expect(page.locator('.service-card[data-service-id="codex-win"] .avatar.solid')).to_have_text("C", timeout=10000)
        face = next(a.get("avatar") or "" for a in sd.ipc(page, "list_agents") if a["id"] == "codex-win")
        check("solid colour avatar shows the name's first letter", face == "color:#8b80c4", face)
        page.locator('[data-avatar-menu="codex-win"]').click()
        page.locator('[data-avatar-reset="codex-win"]').click()
        page.wait_for_timeout(1200)
        face = next(a.get("avatar") or "" for a in sd.ipc(page, "list_agents") if a["id"] == "codex-win")
        check("reset restores the native icon", face.startswith("data:image/svg"), face[:30])
        shot("native-avatars")
        page.locator('.service-card[data-service-id="claude-win"] .avatar-edit').hover()
        page.wait_for_timeout(400)
        hint = page.locator('.service-card[data-service-id="claude-win"] .avatar-edit-hint')
        check("avatar edit hint is a small icon, not text over the face", (hint.text_content() or "").strip() == "" and hint.locator("svg").count() == 1)
        page.locator('.service-card[data-service-id="claude-win"] .service-card-head').screenshot(path=str(sd.ARTIFACTS / "v1-avatar-hover.png"))
        # 改成员名（全局）：私聊等所有地方都跟着变；清空恢复默认。
        page.locator('.service-card[data-service-id="codex-win"]').hover()
        page.locator('[data-rename-agent="codex-win"]').click()
        page.locator(".agent-name-input").fill("小码")
        page.locator(".agent-name-input").press("Enter")
        expect(page.locator('.service-card[data-service-id="codex-win"] [data-agent-name]')).to_have_text("小码", timeout=10000)
        check("member renamed globally", any(agent["id"] == "codex-win" and agent["name"] == "小码" for agent in sd.ipc(page, "list_agents")))
        shot("setup-center")
        page.locator('.service-card[data-service-id="codex-win"]').hover()
        page.locator('[data-rename-agent="codex-win"]').click()
        page.locator(".agent-name-input").fill("")
        page.locator(".agent-name-input").press("Enter")
        expect(page.locator('.service-card[data-service-id="codex-win"] [data-agent-name]')).to_have_text("Codex", timeout=10000)
        check("clearing a member name restores the default", True)

        # ---- 陪伴：配置 → 测试 → 保存 → 连接 → 私聊 ----
        page.locator("[data-companion-config]").click()
        form = page.locator("#companion-form")
        expect(form).to_be_visible()
        form.locator("[name=name]").fill("小夜")
        form.locator("[name=subtitle]").fill("测试陪伴")
        form.locator("[name=endpoint]").fill(endpoint)
        form.locator("[name=key_source]").select_option("inline")
        form.locator("[name=secret]").fill(SECRET)
        form.locator("[name=persona]").fill("你叫小夜，说话温柔。")
        avatar = next(Path(r"D:\Albion picture").glob("*.jpg"), None)
        if avatar:
            form.locator("[data-companion-avatar]").set_input_files(str(avatar))
            page.wait_for_function("() => !!document.querySelector('[data-avatar-preview]').style.backgroundImage")
        form.locator("[data-companion-test]").click()
        expect(form.locator("[data-test-result]")).to_contain_text("companion-mini", timeout=15000)
        check("test connection lists the companion's models", True)
        shot("companion-config")
        form.locator("button.primary").click()
        expect(page.locator("#modal")).not_to_have_attribute("open", "", timeout=10000)
        view = sd.ipc(page, "companion_config")
        check("companion saved without exposing the key", view["name"] == "小夜" and view["has_secret"] and "key_secret" not in view, {k: view[k] for k in ("name", "has_secret", "key_source")})
        db = sqlite3.connect(str(sd.DATA / "hub.db"))
        stored = db.execute("SELECT value FROM metadata WHERE key='companion'").fetchone()[0]
        db.close()
        check("key is encrypted at rest", SECRET not in stored and '"key_secret"' in stored)
        check("member list carries the companion's own name", any(agent["name"] == "小夜" for agent in sd.ipc(page, "list_agents")))
        sd.ipc(page, "connect_albion")
        room = sd.ipc(page, "create_conversation", title="和小夜聊聊", kind="direct", members=["albion-wsl"])
        page.evaluate("() => location.reload()")
        page.wait_for_timeout(1200)
        sd.choose(page, room["id"])
        check("companion avatar shows in the chat header", page.locator(".chat-heading .avatar.albion-portrait").count() == 1 if avatar else True)
        page.locator("#message-input").fill("在吗？")
        page.locator("#message-input").press("Enter")
        expect(page.locator(".message.agent-message .bubble").last).to_contain_text("一直都在", timeout=20000)
        check("companion replies through the configured interface", True)
        page.wait_for_timeout(800)
        saved = [m for m in sd.ipc(page, "get_conversation", id=room["id"])["messages"] if m["sender_id"] == "albion-wsl"]
        check("companion reply is stored as completed", bool(saved) and saved[-1]["status"] == "completed" and "一直都在" in saved[-1]["content"], [(m["status"], m["content"]) for m in saved])
        chat = next(item for item in SEEN if item["path"] == "/v1/chat/completions")
        payload = json.loads(chat["body"])
        check("key is sent only as a Bearer header", chat["auth"] == f"Bearer {SECRET}")
        check("persona goes first as a system message", payload["messages"][0] == {"role": "system", "content": "你叫小夜，说话温柔。"} and payload["model"] == "companion-mini", payload["messages"][:2])
        shot("companion-chat")

        # ---- 设置收进胶囊 ----
        codex_room = sd.ipc(page, "create_conversation", title="胶囊检查", kind="direct", members=["codex-win"])
        page.evaluate("() => location.reload()")
        page.wait_for_timeout(1200)
        sd.choose(page, codex_room["id"])
        chip = page.locator("#live-summary")
        check("model settings are folded into a summary chip", chip.is_visible() and not page.locator("#live-settings").is_visible(), chip.text_content())
        chip.click()
        expect(page.locator("#live-settings")).to_be_visible()
        efforts = page.locator("#live-effort option").evaluate_all("els => els.map(el => el.textContent.trim())")
        target = efforts[-1]
        page.locator("#live-effort").select_option(index=len(efforts) - 1)
        page.wait_for_timeout(600)
        check("chip summary follows the chosen setting", target in (chip.text_content() or ""), (target, chip.text_content()))
        shot("settings-chip-open")
        page.keyboard.press("Escape")
        expect(page.locator("#live-settings")).to_be_hidden()
        check("Esc folds the settings back", True)
        group = sd.ipc(page, "create_conversation", title="群聊胶囊", kind="group", members=["codex-win", "claude-win", "hermes-win"])
        page.evaluate("() => location.reload()")
        page.wait_for_timeout(1200)
        sd.choose(page, group["id"])
        discussion = page.locator("#discussion-summary")
        check("group discussion options are a chip too", discussion.is_visible() and "3 人参与" in (discussion.text_content() or ""), discussion.text_content())
        discussion.click()
        page.locator('[data-discussion-member="hermes-win"]').uncheck()
        page.wait_for_timeout(400)
        check("discussion chip counts the chosen members", "2 人参与" in (discussion.text_content() or ""), discussion.text_content())
        shot("discussion-chip-open")
        page.mouse.click(700, 600)
        expect(page.locator("#discussion-controls")).to_be_hidden()
        check("clicking outside folds the panel", True)

        # ---- 主题：内置 + 导入 ----
        page.locator("#theme-link").click()
        expect(page.locator(".theme-picker")).to_be_visible()
        builtin = page.locator(".theme-card[data-theme-id]").evaluate_all("els => els.map(el => el.dataset.themeId)")
        check("built-in themes are the public set", builtin == ["chenwu", "bohe", "xingguang", "haiyan", "muzi", "plain"], builtin)
        pictures = sorted(Path(r"D:\Albion picture").glob("*"))[:3]
        page.locator("[data-theme-import]").set_input_files([str(path) for path in pictures])
        expect(page.locator('.theme-card[data-theme-id^="custom:"]')).to_have_count(len(pictures), timeout=60000)
        page.wait_for_timeout(1200)
        visible = page.locator(".bg-layer.visible").evaluate_all("els => els.map(el => el.dataset.theme)")
        check("imported pictures are saved and the last one is applied", bool(visible) and visible[0].startswith("custom:"), visible)
        shot("theme-imported")
        first = page.locator('.theme-card[data-theme-id^="custom:"]').first
        before = first.locator(".theme-mode").text_content()
        first.locator(".theme-mode").click()
        page.wait_for_timeout(500)
        after = page.locator('.theme-card[data-theme-id^="custom:"]').first.locator(".theme-mode").text_content()
        check("custom picture can switch between figure and cover", before != after, (before, after))
        page.locator('.theme-card[data-theme-id^="custom:"]').first.locator(".theme-remove").click()
        expect(page.locator('.theme-card[data-theme-id^="custom:"]')).to_have_count(len(pictures) - 1, timeout=10000)
        check("custom picture can be removed", True)
        page.locator('[data-theme-id="chenwu"]').click()
        page.locator("[data-theme-done]").click()
        page.wait_for_timeout(900)
        shot("theme-public")
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
            server.shutdown()
    passed = all(item["ok"] for item in checks)
    (sd.ARTIFACTS / REPORT).write_text(json.dumps({"passed": passed, "checks": checks}, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"{'PASSED' if passed else 'FAILED'} {sum(c['ok'] for c in checks)}/{len(checks)}")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
