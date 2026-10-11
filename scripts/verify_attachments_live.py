"""Verify attachment picker/paste and Rust IPC against the packaged desktop EXE.

Uses a fresh isolated app data directory and synthetic File objects. It never
reads or writes the user's Windows clipboard and never connects an agent.
"""
from __future__ import annotations

import base64
import argparse
import json
import os
from pathlib import Path
import struct
import tempfile
import time
import uuid
import zlib

from playwright.sync_api import sync_playwright, expect

import smoke_desktop as desktop


ROOT = Path(__file__).resolve().parents[1]
checks: list[str] = []


def check(label: str, condition: bool = True) -> None:
    if not condition:
        raise AssertionError(label)
    checks.append(label)
    print("PASS " + label, flush=True)


def solid_png(red: int, green: int, blue: int, width: int = 128, height: int = 128) -> bytes:
    def chunk(kind: bytes, payload: bytes) -> bytes:
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    row = b"\x00" + bytes((red, green, blue)) * width
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(row * height))
        + chunk(b"IEND", b"")
    )


def create_direct_room(page, title: str, agent: str) -> str:
    page.locator("#new-conversation").click()
    page.locator('[data-kind="direct"]').click()
    page.locator('#create-form [name="title"]').fill(title)
    page.locator(f'#create-form [name="member"][value="{agent}"]').check()
    page.locator('#create-form [type="submit"]').click()
    expect(page.locator(".chat-heading h1")).to_have_text(title)
    return page.evaluate("localStorage.getItem('hub.selected')")


def wait_for_reply(page, room: str, agent: str, nonce: str, timeout_seconds: int = 300) -> tuple[str, str]:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        detail = desktop.ipc(page, "get_conversation", id=room)
        user = next((message for message in detail["messages"] if message["sender_id"] == "user" and nonce in message["content"]), None)
        reply = next((message for message in reversed(detail["messages"]) if message["sender_id"] == agent and message["status"] in ("completed", "succeeded")), None)
        if reply and user:
            return user["content"], reply["content"]
        failed = next((message for message in reversed(detail["messages"]) if message["sender_id"] == agent and message["status"] == "failed"), None)
        if failed:
            raise AssertionError(f"{agent} native run failed before producing a reply")
        page.wait_for_timeout(500)
    raise TimeoutError(f"{agent} did not finish one attachment request within {timeout_seconds}s")


def native_attachment_reads(page, only_agents: list[str]) -> None:
    tasks = [
        ("codex-win", "codex", "gpt-5.6-luna", "image"),
        ("hermes-win", "hermes", None, "text"),
        ("dsh-win", "dsh", None, "text"),
    ]
    for agent, key, requested_model, kind in tasks:
        if agent not in only_agents:
            continue
        title = "附件原生验收 " + agent
        room = create_direct_room(page, title, agent)
        nonce = "ATTACHMENT_NONCE_" + uuid.uuid4().hex[:12].upper()
        if kind == "image":
            name = "blue-fixture.png"
            payload = solid_png(20, 80, 235)
            question = f"请观察附件图片，说明主色，并原样回复校验码 {nonce}。只需一句话。"
            mime = "image/png"
        else:
            name = f"{key}-nonce.txt"
            payload = f"{nonce}\n这是供附件读取验收的合成文本。\n".encode("utf-8")
            question = f"请读取唯一附件，原样回复其中的校验码 {nonce}。只需回复校验码。"
            mime = "text/plain"

        page.locator("[data-attachment-picker]").set_input_files({"name": name, "mimeType": mime, "buffer": payload})
        page.wait_for_function(
            "room => JSON.parse(localStorage.getItem(`hub.attachments.${room}`) || '[]').length === 1",
            arg=room,
            timeout=15000,
        )

        snapshot = desktop.ipc(page, key + "_status")
        if snapshot.get("connection") != "connected":
            snapshot = desktop.ipc(page, "connect_" + key)
        if snapshot.get("connection") != "connected":
            raise AssertionError(f"{agent} native client did not connect")
        models = snapshot.get("models", [])
        if requested_model:
            model = next((item for item in models if item.get("id") == requested_model), None)
        else:
            model = next(
                (
                    item
                    for item in models
                    if item.get("provider_id") == "command-code-daily-2"
                    or "daily two" in (item.get("provider_name") or "").lower()
                    if "v4.1" in (item.get("id") or "").lower()
                ),
                None,
            )
        if model is None:
            raise AssertionError(f"{agent} target model was not present in its connected catalog")
        desktop.ipc(page, "set_session_settings", id=room, agentId=agent, model=model["id"], reasoningEffort="low")

        input_box = page.locator("#message-input")
        input_box.fill(question)
        send = page.locator(f"#send-{key}")
        send.wait_for(state="visible", timeout=10000)
        deadline = time.monotonic() + 15
        while not send.is_enabled() and time.monotonic() < deadline:
            page.wait_for_timeout(200)
        if not send.is_enabled():
            raise AssertionError(f"{agent} send button did not become enabled")
        send.click()
        stored_content, reply = wait_for_reply(page, room, agent, nonce)
        stored_items = json.loads(stored_content.split("```tongxi-attachments\n", 1)[1].rsplit("\n```", 1)[0])
        check(f"{agent} used one {model['id']} low-effort attachment turn", len(stored_items) == 1 and stored_items[0]["name"] == name)
        check(f"{agent} native response contains attachment evidence", nonce in reply and (kind != "image" or ("blue" in reply.lower() or "蓝" in reply)))


def run() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--native", action="store_true", help="also make one real native attachment request per authorized agent")
    parser.add_argument("--native-agents", nargs="+", choices=("codex-win", "hermes-win", "dsh-win"), help="run only the listed not-yet-tested agent requests")
    args = parser.parse_args()
    desktop.EXE = Path(os.environ.get("AGENT_HUB_EXE", str(ROOT / "release" / "Agent Hub.exe")))
    desktop.DATA = Path(tempfile.gettempdir()) / ("tongxi-attachment-live-" + uuid.uuid4().hex[:12])
    desktop.ISOLATED_SESSION_DB = True
    proc = None
    try:
        check("packaged EXE exists", desktop.EXE.is_file())
        with tempfile.TemporaryDirectory(prefix="tongxi-attachment-fixtures-") as temporary, sync_playwright() as playwright:
            fixture_dir = Path(temporary)
            picker_bytes = b"attachment-picker-fixture\x00bytes"
            pasted_bytes = b"attachment-paste-fixture\x00bytes"
            picker_file = fixture_dir / "picker-fixture.txt"
            picker_file.write_bytes(picker_bytes)
            pasted_file = fixture_dir / "paste-fixture.txt"
            pasted_file.write_bytes(pasted_bytes)

            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            info = desktop.ipc(page, "app_info")
            check("app database uses isolated test directory", Path(info["database_path"]) == desktop.DATA / "hub.db")

            page.locator("#new-conversation").click()
            page.locator('[data-kind="direct"]').click()
            page.locator('#create-form [name="title"]').fill("真实附件验收")
            page.locator('#create-form [type="submit"]').click()
            expect(page.locator(".chat-heading h1")).to_have_text("真实附件验收")
            room = {"id": page.evaluate("localStorage.getItem('hub.selected')")}
            expect(page.locator("#message-input")).to_be_visible()

            picker = page.locator("[data-attachment-picker]")
            picker.set_input_files(str(picker_file))
            page.wait_for_function(
                "room => JSON.parse(localStorage.getItem(`hub.attachments.${room}`) || '[]').length === 1",
                arg=room["id"],
                timeout=15000,
            )
            saved = page.evaluate("room => JSON.parse(localStorage.getItem(`hub.attachments.${room}`) || '[]')", room["id"])
            picker_item = saved[0]
            check("file picker stores attachment through real Tauri IPC", picker_item["name"] == picker_file.name and picker_item["bytes"] == len(picker_bytes))
            expect(page.locator("[data-attachment-download]")).to_have_count(1)
            picker_data = desktop.ipc(page, "attachment_data", conversationId=room["id"], attachmentId=picker_item["id"])
            check("picker bytes round-trip through Rust", base64.b64decode(picker_data.split(",", 1)[1]) == picker_bytes)

            page.evaluate(
                """([room, name, bytes]) => {
                  const file = new File([Uint8Array.from(bytes)], name, { type: 'text/plain' });
                  const transfer = new DataTransfer(); transfer.items.add(file);
                  const event = new ClipboardEvent('paste', { clipboardData: transfer, bubbles: true, cancelable: true });
                  document.querySelector('#message-input').dispatchEvent(event);
                }""",
                [room["id"], pasted_file.name, list(pasted_bytes)],
            )
            page.wait_for_function(
                "room => JSON.parse(localStorage.getItem(`hub.attachments.${room}`) || '[]').length === 2",
                arg=room["id"],
                timeout=15000,
            )
            saved = page.evaluate("room => JSON.parse(localStorage.getItem(`hub.attachments.${room}`) || '[]')", room["id"])
            pasted_item = next(item for item in saved if item["name"] == pasted_file.name)
            pasted_data = desktop.ipc(page, "attachment_data", conversationId=room["id"], attachmentId=pasted_item["id"])
            check("synthetic pasted File reaches the Rust attachment store", base64.b64decode(pasted_data.split(",", 1)[1]) == pasted_bytes)
            expect(page.locator("[data-attachment-download]")).to_have_count(2)

            body = "验收消息"
            metadata = "```tongxi-attachments\n" + json.dumps(saved, ensure_ascii=False) + "\n```"
            content = body + "\n\n" + metadata
            desktop.ipc(page, "save_local_message", conversationId=room["id"], messageId=str(uuid.uuid4()), content=content)
            detail = desktop.ipc(page, "get_conversation", id=room["id"])
            stored = next(message for message in detail["messages"] if message["content"].startswith(body))
            check("saved message contains metadata without embedded file payloads", all(base64.b64encode(value).decode() not in stored["content"] for value in (picker_bytes, pasted_bytes)))
            stored_items = json.loads(stored["content"].split("```tongxi-attachments\n", 1)[1].rsplit("\n```", 1)[0])
            check("saved message retains attachment identifiers and paths", stored_items == saved)

            if args.native or args.native_agents:
                native_attachment_reads(page, args.native_agents or ["codex-win", "hermes-win", "dsh-win"])

            check("no JavaScript runtime errors", not desktop.errors)
            print(f"Attachment live verification: {len(checks)} passed", flush=True)
    finally:
        if proc is not None and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)
        # Keep DATA as isolated evidence for inspection; session DB/cache paths
        # are directed there by smoke_desktop.launch().


if __name__ == "__main__":
    run()
