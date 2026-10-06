"""Exercise the packaged Tauri EXE and real Rust IPC in isolated app data.

Requires the locally available Python Playwright package. No browser mocks,
model calls, external agent settings, or production database are used.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import socket
import subprocess
import time
import urllib.request
import uuid

from playwright.sync_api import sync_playwright, expect

ROOT = Path(__file__).resolve().parents[1]
EXE = ROOT / "release" / "Agent Hub.exe"
ARTIFACTS = ROOT / "artifacts"
# 每轮跑的应用数据目录放进会被自动清理的 scratch（Hermes 的缓存目录，闲置即清），
# 别往仓库的 artifacts/ 里堆：一轮就是一份 WebView2 数据目录（约 11 MB）。
# 验收证据（verify 脚本写的 JSON）仍然留在 artifacts/。
SCRATCH = Path(os.environ.get("TMPDIR") or (Path(os.environ.get("LOCALAPPDATA", str(Path.home()))) / "hermes" / "cache" / "scratch"))
DATA = SCRATCH / "agenthub-runs" / ("desktop-test-" + uuid.uuid4().hex[:10])
ARTIFACTS.mkdir(exist_ok=True)
checks: list[str] = []
errors: list[str] = []


def check(name: str, condition: bool = True) -> None:
    if not condition:
        raise AssertionError(name)
    checks.append(name)
    print("PASS " + name, flush=True)


# 默认把会话库隔离到测试数据目录；要验「共用真实库」的脚本把它设成 False。
ISOLATED_SESSION_DB = True


def launch():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    env = os.environ.copy()
    env["AGENT_HUB_DATA_DIR"] = str(DATA)
    # 会话库默认指向真实库（同席要能列到/接着聊她自己的历史会话）；测试一律用自己数据目录里的
    # 隔离库，绝不在测试里写用户的真实 state.db。
    if ISOLATED_SESSION_DB:
        env["AGENT_HUB_HERMES_SESSION_DB"] = str(DATA / "hermes-native" / "sessions.db")
        # DSH 同理：默认共用她真实的 ~/.dsh，测试一律用自己数据目录里的隔离家目录。
        env["AGENT_HUB_DSH_HOME"] = str(DATA / "dsh-native")
    env["WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"] = f"--remote-debugging-port={port} --remote-debugging-address=127.0.0.1"
    proc = subprocess.Popen([str(EXE)], env=env, creationflags=subprocess.CREATE_NO_WINDOW)
    endpoint = f"http://127.0.0.1:{port}"
    try:
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise RuntimeError(f"Desktop exited: {proc.returncode}")
            try:
                with urllib.request.urlopen(endpoint + "/json/version", timeout=1) as response:
                    json.load(response)
                with urllib.request.urlopen(endpoint + "/json/list", timeout=1) as response:
                    print("CDP targets: " + json.dumps([{k: t.get(k) for k in ("type", "url", "title")} for t in json.load(response)], ensure_ascii=False), flush=True)
                return proc, endpoint
            except OSError:
                time.sleep(0.25)
        raise RuntimeError("WebView2 CDP endpoint did not become ready")
    except BaseException:
        proc.terminate()
        proc.wait(timeout=10)
        raise


def page_for(browser):
    print("CDP pages: " + json.dumps([[p.url for p in c.pages] for c in browser.contexts]), flush=True)
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        for context in browser.contexts:
            for page in context.pages:
                if "localhost" in page.url:
                    page.on("pageerror", lambda error: errors.append(str(error)))
                    expect(page.locator("#new-conversation")).to_be_enabled(timeout=20000)
                    return page
        pages = [page for context in browser.contexts for page in context.pages]
        if pages:
            # Pump Playwright events while the initial about:blank navigates.
            pages[0].wait_for_timeout(200)
        else:
            time.sleep(0.2)
    raise RuntimeError("Tauri application page not found")


def ipc(page, command, **args):
    return page.evaluate("([command, args]) => window.__TAURI_INTERNALS__.invoke(command, args)", [command, args])


def choose(page, conversation_id):
    page.locator(f'[data-conversation="{conversation_id}"]').click()
    page.wait_for_function("id => localStorage.getItem('hub.selected') === id", arg=conversation_id)
    expect(page.locator("#message-input")).to_be_visible()


def settings(page):
    page.locator("#conversation-menu").click()
    expect(page.locator("#rename-form")).to_be_visible()


def run():
    proc = None
    try:
        with sync_playwright() as playwright:
            proc, endpoint = launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = page_for(browser)
            info = ipc(page, "app_info")
            check("packaged native IPC and isolated SQLite", Path(info["database_path"]) == DATA / "hub.db")
            roster = ipc(page, "list_agents")
            check("four real role records, all disconnected", len(roster) == 4 and all(a["status"] == "not_connected" for a in roster))
            initial = ipc(page, "list_conversations", search="", archived=False)
            check("first launch seeds four private rooms and one group", len(initial) == 5)
            check("desktop UI lists five rooms", page.locator("[data-conversation]").count() == 5)
            page.screenshot(path=str(ARTIFACTS / "desktop-group.png"))

            page.locator("#new-conversation").click()
            page.locator('[data-kind="direct"]').click()
            title = '<img src=x onerror="window.bad=1"> 独立私聊'
            page.locator('#create-form [name="title"]').fill(title)
            page.locator('#create-form [type="submit"]').click()
            expect(page.locator(".chat-heading h1")).to_have_text(title)
            direct_id = page.evaluate("localStorage.getItem('hub.selected')")
            check("private room created via UI and title rendered literally", page.locator(".chat-heading img").count() == 0)
            content = '<script>window.bad=1</script> 开发需求\n第二行'
            page.locator("#message-input").fill(content)
            page.locator("#save-message").click()
            expect(page.locator(".bubble")).to_have_text(content)
            expect(page.locator(".message-delivery")).to_contain_text("尚未发送")
            saved = ipc(page, "get_conversation", id=direct_id)
            check("message persisted by Rust as user/local_only", saved["messages"][0]["status"] == "local_only" and saved["messages"][0]["sender_id"] == "user")
            check("user markup never executes", page.evaluate("window.bad === undefined") and page.locator(".bubble script").count() == 0)

            page.locator("#new-conversation").click()
            page.locator('#create-form [name="title"]').fill("桌面测试群")
            page.locator('#create-form [type="submit"]').click()
            expect(page.locator(".chat-heading h1")).to_have_text("桌面测试群")
            group_id = page.evaluate("localStorage.getItem('hub.selected')")
            group = ipc(page, "get_conversation", id=group_id)
            check("group starts without private room history", len(group["messages"]) == 0)
            check("Codex group and private session mappings differ", next(s["session_key"] for s in group["sessions"] if s["agent_id"] == "codex-win") != saved["sessions"][0]["session_key"])
            settings(page)
            page.locator("#members-action").click()
            page.locator('#members-form [value="albion-wsl"]').check()
            page.locator("#members-form button").click()
            expect(page.locator(".member-card")).to_have_count(4)
            updated = ipc(page, "get_conversation", id=group_id)
            check("group roster update preserves existing session keys", all(s in updated["sessions"] for s in group["sessions"]))
            check("Albion can be explicitly invited", "albion-wsl" in updated["conversation"]["members"])

            settings(page)
            page.locator('#rename-form [name="title"]').fill("框架开发讨论")
            page.locator("#rename-form button").click()
            expect(page.locator(".chat-heading h1")).to_have_text("框架开发讨论")
            check("rename preserves conversation identity", page.evaluate("localStorage.getItem('hub.selected')") == group_id)
            page.locator("#message-input").fill("只属于群聊的待发送需求")
            page.locator("#save-message").click()
            expect(page.locator(".bubble")).to_have_text("只属于群聊的待发送需求")
            check("saving group message does not change private history", len(ipc(page, "get_conversation", id=direct_id)["messages"]) == 1)
            settings(page)
            page.locator("#archive-action").click()
            expect(page.locator("#message-input")).to_be_disabled()
            check("archived conversation blocks composer")
            settings(page)
            page.locator("#archive-action").click()
            expect(page.locator("#message-input")).to_be_enabled()
            check("restore keeps saved group message", len(ipc(page, "get_conversation", id=group_id)["messages"]) == 1)

            page.locator("#search").fill("第二行")
            expect(page.locator("[data-conversation]")).to_have_count(1)
            check("sidebar searches persisted message text", page.locator("[data-conversation]").get_attribute("data-conversation") == direct_id)
            page.locator("#search").fill("")
            expect(page.locator("[data-conversation]")).to_have_count(7)
            choose(page, direct_id)
            expect(page.locator(".bubble")).to_have_text(content)
            page.locator("#message-input").fill("重启后应恢复的未保存输入")
            page.locator("#service-link").click()
            expect(page.locator(".service-card")).to_have_count(4)
            check("all four members have independent native connect controls", all(page.locator('#service-'+key+'-control').is_enabled() for key in ('codex','hermes','dsh','albion')))
            page.screenshot(path=str(ARTIFACTS / "desktop-services.png"))
            page.locator("#back-to-chat").click()
            expect(page.locator("#message-input")).to_have_value("重启后应恢复的未保存输入")
            check("draft survives service-page navigation")

            second = subprocess.Popen([str(EXE)], env={**os.environ, "AGENT_HUB_DATA_DIR": str(DATA)}, creationflags=subprocess.CREATE_NO_WINDOW)
            try:
                check("second launch exits through single-instance handling", second.wait(timeout=12) == 0)
            finally:
                if second.poll() is None:
                    second.terminate()
                    second.wait(timeout=10)

            proc.terminate()
            proc.wait(timeout=10)
            proc = None
            time.sleep(0.7)
            proc, endpoint = launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = page_for(browser)
            expect(page.locator(".chat-heading h1")).to_have_text(title)
            expect(page.locator(".bubble")).to_have_text(content)
            expect(page.locator("#message-input")).to_have_value("重启后应恢复的未保存输入")
            check("real EXE restart restores room, history and unsaved input")
            check("restart preserves native session mapping", ipc(page, "get_conversation", id=direct_id)["sessions"] == saved["sessions"])
            check("restart does not duplicate seeded rooms", len(ipc(page, "list_conversations", search="", archived=False)) == 7)
            settings(page)
            page.locator("#delete-action").click()
            expect(page.locator("#confirm-delete")).to_be_visible()
            page.locator("#cancel-delete").click()
            check("delete cancellation retains conversation", ipc(page, "get_conversation", id=direct_id)["conversation"]["id"] == direct_id)
            settings(page)
            page.locator("#delete-action").click()
            page.locator("#confirm-delete").click()
            expect(page.locator("[data-conversation]")).to_have_count(6)
            check("confirmed delete removes only selected room", len(ipc(page, "get_conversation", id=group_id)["messages"]) == 1)
            check("no JavaScript runtime errors", not errors)
    finally:
        if proc is not None and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)
        report = {"passed": len(checks), "checks": checks, "javascript_errors": errors, "test_data_directory": str(DATA), "executable": str(EXE)}
        (ARTIFACTS / "desktop-smoke.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")


if __name__ == "__main__":
    run()
    print(f"Desktop smoke: {len(checks)} passed", flush=True)
