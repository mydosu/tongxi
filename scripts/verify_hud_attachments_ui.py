"""Playwright checks for attachment compose/render paths and compact HUD layout.

All Tauri IPC is synthetic. The script starts and closes its own Vite process.
"""
from __future__ import annotations

import json
from pathlib import Path
import subprocess
import time
import urllib.request

from playwright.sync_api import expect, sync_playwright

from verify_model_provider_ui import MOCK

ROOT = Path(__file__).resolve().parents[1]
PORT = "1422"
BASE = f"http://127.0.0.1:{PORT}"
ARTIFACTS = ROOT / "artifacts"
checks: list[str] = []


def check(name: str, condition: bool = True) -> None:
    if not condition:
        raise AssertionError(name)
    checks.append(name)
    print("PASS " + name, flush=True)


IPC_FIXTURE = r"""
(() => {
  const original = __TAURI_INTERNALS__.invoke;
  const files = new Map();
  const demo = globalThis.__providerDemo;
  const groupDetail = demo.detail;
  if (new URLSearchParams(location.search).get('win') === 'hud') {
    groupDetail.conversation.kind = 'direct';
    groupDetail.conversation.members = ['codex-win'];
    groupDetail.project = null;
    groupDetail.messages = [{id:'hud-tool-message',conversation_id:groupDetail.conversation.id,sender_id:'codex-win',content:'HUD 回复正文',thought:'',tool_events:[{event_id:'hud-event-1',kind:'read_file',title:'读取文件',input:'src/main.ts',output:'已读取',status:'completed',created_at:Date.now()}],status:'completed',created_at:Date.now()}];
  }
  const directConversation = {...demo.group, id:'direct-test', title:'Codex 私聊', kind:'direct', members:['codex-win'], project_id:null};
  const directDetail = {...groupDetail, conversation:directConversation, project:null, workflows:[], project_agent_chats:[], project_team_messages:[], sessions:[{agent_id:'codex-win',session_key:'codex-win',native_session_id:null,model:null,reasoning_effort:null}], messages:[]};
  const conversationList = [directConversation, demo.group];
  let sequence = 0;
  globalThis.__attachmentCalls = [];
  __TAURI_INTERNALS__.invoke = async (command, args = {}) => {
    if (command === 'stage_attachment') {
      const item = {id:'fixture-'+(++sequence), name:args.name, mime:args.mime, bytes:atob(args.data).length, path:'fixture/'+sequence};
      files.set(args.conversationId+':'+item.id, 'data:'+item.mime+';base64,'+args.data);
      globalThis.__attachmentCalls.push({command, args:{...args, data:'[omitted]'}});
      return item;
    }
    if (command === 'attachment_data') return 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jKqkAAAAASUVORK5CYII=';
    if (command === 'paste_clipboard_files') {
      globalThis.__attachmentCalls.push({command,args});
      return globalThis.__nativePasteOverflow ? Array.from({length:20},(_,index)=>({id:'native-overflow-'+index,name:'native-'+index+'.txt',mime:'text/plain',bytes:1,path:'fixture/native-'+index})) : [];
    }
    if (command === 'list_conversations') return conversationList;
    if (command === 'get_conversation' && args.id === directConversation.id) return directDetail;
    if (command === 'codex_status') return {revision:1,connection:'connected',executable:null,version:'test',error:null,active:null,models:demo.models['codex-win'],default_model:demo.models['codex-win'][0].id,default_effort:'low'};
    if (command === 'send_codex_message') { globalThis.__attachmentCalls.push({command,args}); return {id:args.messageId,conversation_id:args.conversationId,agent_id:'codex-win',status:'running',created_at:Date.now(),updated_at:Date.now()}; }
    if (command === 'save_local_message') {
      const target = args.conversationId === directConversation.id ? directDetail : groupDetail;
      const message = {id:args.messageId, conversation_id:args.conversationId, sender_id:'user', content:args.content, thought:'', tool_events:[], status:'local_only', created_at:Date.now()};
      target.messages.push(message);
      globalThis.__attachmentCalls.push({command,args});
      return message;
    }
    globalThis.__attachmentCalls.push({command,args});
    return original(command, args);
  };
})();
"""


def wait_server() -> subprocess.Popen:
    server = subprocess.Popen(
        ["node", str(ROOT / "node_modules/vite/bin/vite.js"), "--host", "127.0.0.1", "--port", PORT, "--strictPort"],
        cwd=ROOT,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    for _ in range(100):
        if server.poll() is not None:
            subprocess.run(["taskkill", "/PID", str(server.pid), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
            raise RuntimeError("Vite exited before becoming ready")
        try:
            urllib.request.urlopen(BASE, timeout=.3).close()
            return server
        except OSError:
            time.sleep(.2)
    server.terminate()
    server.wait(timeout=10)
    subprocess.run(["taskkill", "/PID", str(server.pid), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    raise TimeoutError("Vite did not become ready on port " + PORT)


def stage_picker(page, name: str, mime: str) -> None:
    picker = page.locator("input[data-attachment-picker]").last
    expect(picker).to_have_count(1)
    picker.set_input_files({"name": name, "mimeType": mime, "buffer": b"fixture-image-bytes"})
    tray = page.locator(".attachment-tray").last
    expect(tray.locator("[data-attachment-remove]")).to_have_count(1)
    expect(tray).to_contain_text(name)


def verify_picker_and_remove(page, composer: str, name: str) -> None:
    stage_picker(page, name, "image/png")
    tray = page.locator(composer).locator(".attachment-tray").last
    expect(tray.locator("img[data-attachment-preview]")).to_be_visible()
    expect(tray.locator("img")).to_have_attribute("src", "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jKqkAAAAASUVORK5CYII=")
    check("image attachment has a rendered preview in " + composer)
    check("malicious filename remains escaped in " + composer, tray.locator("img").count() == 1 and tray.locator("[onerror]").count() == 0)
    tray.locator("[data-attachment-remove]").last.click()
    expect(tray).to_be_hidden()
    check("attachment can be removed from " + composer)


def main() -> None:
    server = wait_server()
    success = False
    browser = None
    page_errors: list[str] = []
    try:
        ARTIFACTS.mkdir(exist_ok=True)
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            page = browser.new_page(viewport={"width": 1280, "height": 860}, device_scale_factor=1)
            page.on("pageerror", lambda error: page_errors.append(str(error)))
            page.add_init_script(MOCK)
            page.add_init_script(IPC_FIXTURE)
            page.goto(BASE, wait_until="domcontentloaded")
            page.locator("#message-input").wait_for(state="visible", timeout=15000)
            expect(page.locator("#message-input")).to_be_visible()

            # Stage an image through FileList, verify preview, removal, then paste a file.
            malicious_name = 'photo"><img src=x onerror=alert(1)>.png'
            verify_picker_and_remove(page, "#composer", malicious_name)

            # Simulate a real clipboard FileList without touching the Windows clipboard.
            page.locator("#message-input").evaluate("""el => {
              const file = new File(['pasted-data'], 'pasted-document.txt', {type:'text/plain'});
              const transfer = new DataTransfer(); transfer.items.add(file);
              el.dispatchEvent(new ClipboardEvent('paste', {clipboardData:transfer, bubbles:true, cancelable:true}));
            }""")
            expect(page.locator(".attachment-tray [data-attachment-remove]")).to_have_count(1)
            expect(page.locator(".attachment-tray")).to_contain_text("pasted-document.txt")
            check("paste event stages a synthetic clipboard file")

            # Attachment-only local draft must hide its wire-format block and consume the tray item.
            page.locator("#save-message").click()
            message = page.locator(".message.own").last
            expect(message.locator(".bubble")).to_contain_text("请查看附件。")
            check("attachment-only draft receives a readable default prompt", message.locator(".bubble").inner_text().strip() == "请查看附件。")
            check("attachment protocol remains hidden in the transcript", "tongxi-attachments" not in message.inner_text())
            expect(message.locator("[data-attachment-download]")).to_have_count(1)
            expect(page.locator(".attachment-tray").last).to_be_hidden()
            check("saved draft exposes a download and clears staged attachments")

            # The normal private composer uses a distinct attachment room.
            page.locator('[data-conversation="direct-test"]').click()
            expect(page.locator("#message-input")).to_be_visible()
            verify_picker_and_remove(page, "#composer", "private-image.png")
            stage_picker(page, "restore-me.txt", "text/plain")
            page.locator('[data-conversation="provider-test"]').click()
            page.locator('[data-conversation="direct-test"]').click()
            expect(page.locator(".attachment-tray")).to_contain_text("restore-me.txt")
            check("staged attachment draft returns after leaving and reopening a conversation")
            page.locator(".attachment-tray [data-attachment-remove]").last.click()
            stage_picker(page, "only-attachment.txt", "text/plain")
            page.locator("#send-codex").click()
            page.wait_for_function("__attachmentCalls.some(call => call.command==='send_codex_message')")
            sent = page.evaluate("__attachmentCalls.find(call => call.command==='send_codex_message').args.content")
            check("attachment-only send submits the default message plus attachment protocol", sent.startswith("请查看附件。\n\n```tongxi-attachments\n") and "only-attachment.txt" in sent)
            expect(page.locator(".attachment-tray").last).to_be_hidden()

            # Project member transcript mounts a separate composer.
            page.locator('[data-conversation="provider-test"]').click()
            page.locator('[data-project-module="codex-win"]').click()
            expect(page.locator("[data-project-agent-chat-input]")).to_be_visible()
            verify_picker_and_remove(page, "[data-project-agent-chat-form]", "member-image.png")
            native_calls_before = page.evaluate("__attachmentCalls.filter(call => call.command==='paste_clipboard_files').length")
            page.locator("[data-project-agent-chat-input]").evaluate("""el => {
              const transfer = new DataTransfer(); transfer.setData('text/plain','ordinary pasted text');
              el.dispatchEvent(new ClipboardEvent('paste',{clipboardData:transfer,bubbles:true,cancelable:true}));
            }""")
            check("plain-text paste does not scan the native file clipboard", page.evaluate("__attachmentCalls.filter(call => call.command==='paste_clipboard_files').length") == native_calls_before)
            stage_picker(page, "preserve-this.txt", "text/plain")
            page.evaluate("globalThis.__nativePasteOverflow = true")
            page.locator("[data-project-agent-chat-input]").evaluate("""el => {
              const transfer = new DataTransfer();
              el.dispatchEvent(new ClipboardEvent('paste',{clipboardData:transfer,bubbles:true,cancelable:true}));
            }""")
            page.wait_for_function("__attachmentCalls.some(call => call.command==='paste_clipboard_files')")
            expect(page.locator(".attachment-tray [data-attachment-remove]")).to_have_count(1)
            expect(page.locator(".attachment-tray")).to_contain_text("preserve-this.txt")
            expect(page.locator("#toast")).to_contain_text("一条消息最多20个附件")
            check("native clipboard overflow reports the limit and preserves existing attachments")
            page.evaluate("globalThis.__nativePasteOverflow = false")

            # Reload into HUD with a synthetic completed tool event for visual/state checks.
            page.goto(BASE + "/?win=hud", wait_until="domcontentloaded")
            page.locator("#message-input").wait_for(state="visible", timeout=15000)
            expect(page.locator("html[data-hud='1']")).to_have_count(1)
            expect(page.locator("#message-input")).to_be_visible()
            check("HUD mode mounts the standard message composer")
            activity = page.locator(".tool-activity")
            expect(activity).to_have_count(1)
            check("HUD shows tool activity separately from the reply body", activity.locator(".tool-latest").inner_text() == "读取文件" and page.locator(".bubble").last.inner_text() == "HUD 回复正文")
            check("HUD tool card background is transparent", page.evaluate("getComputedStyle(document.querySelector('.tool-activity')).backgroundColor") in ("rgba(0, 0, 0, 0)", "transparent"))
            activity.locator("summary").first.click()
            expect(activity.locator(".tool-history")).to_be_visible()
            check("HUD tool detail expands only after clicking the summary")

            def check_hud_bounds(label: str) -> None:
                expect(page.locator(".members-panel")).to_be_hidden()
                check(label + " hides the member sidebar", page.locator(".members-panel").evaluate("el => getComputedStyle(el).display === 'none'"))
                geometry = page.evaluate("""() => {
              const messages=document.querySelector('.messages').getBoundingClientRect();
              const composer=document.querySelector('#composer').getBoundingClientRect();
              return {viewport:innerWidth, doc:document.documentElement.scrollWidth,
                messages:{left:messages.left,right:messages.right}, composer:{left:composer.left,right:composer.right}};
            }""")
                check(label + " has no horizontal overflow", geometry["doc"] <= geometry["viewport"])
                aligned = abs(geometry["messages"]["left"]-geometry["composer"]["left"]) < 2 and abs(geometry["messages"]["right"]-geometry["composer"]["right"]) < 2
                check(label + " aligns transcript edges to the composer", aligned)

            page.set_viewport_size({"width":1280,"height":860})
            check_hud_bounds("wide HUD 1280x860")
            check("wide HUD starts in transparent, unfocused mode", not page.locator("html").evaluate("el => el.hasAttribute('data-hud-typing')") and page.evaluate("getComputedStyle(document.querySelector('.messages')).backgroundColor") == "rgba(0, 0, 0, 0)")
            page.screenshot(path=str(ARTIFACTS / "hud-attachments-ui-wide-transparent.png"), full_page=True)
            page.locator("#message-input").focus()
            expect(page.locator("html[data-hud-typing]")).to_have_count(1)
            page.wait_for_function("getComputedStyle(document.querySelector('.messages')).backgroundColor === 'rgba(12, 14, 18, 0.62)'", timeout=3000)
            focused_bg = page.evaluate("getComputedStyle(document.querySelector('.messages')).backgroundColor")
            check("focused HUD turns on the text canvas scrim (" + focused_bg + ")", focused_bg.replace(" ", "") == "rgba(12,14,18,0.62)")
            page.screenshot(path=str(ARTIFACTS / "hud-attachments-ui-wide-focused.png"), full_page=True)

            page.set_viewport_size({"width":400,"height":540})
            check_hud_bounds("narrow HUD 400x540")
            page.screenshot(path=str(ARTIFACTS / "hud-attachments-ui-narrow-focused.png"), full_page=True)
            page.locator("#message-input").evaluate("el => el.blur()")
            expect(page.locator("html[data-hud-typing]")).to_have_count(0)
            page.wait_for_function("getComputedStyle(document.querySelector('.messages')).backgroundColor === 'rgba(0, 0, 0, 0)'", timeout=3000)
            check("narrow HUD returns to transparent canvas on blur", page.evaluate("getComputedStyle(document.querySelector('.messages')).backgroundColor") == "rgba(0, 0, 0, 0)")
            page.screenshot(path=str(ARTIFACTS / "hud-attachments-ui-narrow-transparent.png"), full_page=True)
            page.screenshot(path=str(ARTIFACTS / "hud-attachments-ui.png"), full_page=True)
            check("no JavaScript errors", not page_errors)
            success = True
            browser.close()
            browser = None
    finally:
        if browser:
            try:
                browser.close()
            except Exception:
                pass
        subprocess.run(["taskkill", "/PID", str(server.pid), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait(timeout=5)
        (ARTIFACTS / "hud-attachments-ui.json").write_text(
            json.dumps({"success": success, "passed": len(checks), "checks": checks, "page_errors": page_errors}, ensure_ascii=False, indent=2),
            encoding="utf-8",
        )


if __name__ == "__main__":
    main()
