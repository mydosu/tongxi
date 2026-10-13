"""项目记忆分层的真实 OpenViking 验收：共享池、成员私有笔记、自主分享、作者独占修改、列出与检索范围、
旧目录归属、凭据缺失时的明确报错。按同席 Rust 侧的方式启动内嵌桥接脚本；不调用模型。
使用一次性项目 ID，结束后删除该项目目录。"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BRIDGE = (ROOT / "src-tauri" / "src" / "project_memory.py").read_text(encoding="utf-8")
PYTHON = os.environ.get("AGENT_HUB_OPENVIKING_PYTHON", "D:/AI/openviking/venv/Scripts/python.exe")
CONFIG = os.environ.get("AGENT_HUB_OPENVIKING_CONFIG", str(Path.home() / ".openviking" / "ovcli.tooling.conf"))
PROJECT = "memtest-" + uuid.uuid4().hex[:10]
passed = 0


def call(agent: str, tool: str, args: dict, config: str = CONFIG) -> dict:
    # 与同席 Rust 侧一致：不继承调用方环境里的 OpenViking 身份，只用指定的配置文件。
    env = {key: value for key, value in os.environ.items() if not key.upper().startswith("OPENVIKING_")}
    env.update({"PYTHONIOENCODING": "utf-8", "OPENVIKING_CLI_CONFIG_FILE": config})
    payload = json.dumps({"project": PROJECT, "agent": agent, "tool": tool, "args": args})
    out = subprocess.run([PYTHON, "-u", "-c", BRIDGE], input=payload.encode(), capture_output=True, env=env, timeout=90)
    return json.loads(out.stdout)


def check(label: str, condition: bool, detail: object = None) -> None:
    global passed
    if not condition:
        raise AssertionError(f"{label}: {json.dumps(detail, ensure_ascii=False)[:400]}")
    passed += 1
    print("PASS " + label, flush=True)


def main() -> None:
    root = f"viking://resources/tongxi/projects/{PROJECT}/"
    try:
        written = call("claude-win", "hub_memory_write", {"name": "图片直传结论", "content": "autoResize 关闭后原图直传，2000px 内不缩图。"})
        private_uri = written.get("result", {}).get("uri")
        check("private write lands in the author's private area", private_uri == root + "private/claude-win/图片直传结论.md", written)
        check("author reads own private note", "原图直传" in call("claude-win", "hub_memory_read", {"uri": private_uri}).get("result", {}).get("content", ""))
        denied = call("codex-win", "hub_memory_read", {"uri": private_uri})
        check("another member cannot read a private note", "error" in denied and "私有" in denied["error"], denied)

        shared = call("claude-win", "hub_memory_share", {"uri": private_uri, "note": "供 DSH 验证时参考"})
        shared_uri = shared.get("result", {}).get("uri")
        check("sharing copies the note into the shared pool", shared_uri == root + "shared/图片直传结论.md", shared)
        text = call("codex-win", "hub_memory_read", {"uri": shared_uri}).get("result", {}).get("content", "")
        check("other members read the shared copy with author and source", "Author: claude-win" in text and f"Shared-From: {private_uri}" in text and "供 DSH 验证时参考" in text, text)
        check("the private original is kept", "原图直传" in call("claude-win", "hub_memory_read", {"uri": private_uri}).get("result", {}).get("content", ""))

        clash = call("codex-win", "hub_memory_write", {"name": "图片直传结论", "content": "覆盖试试", "scope": "shared"})
        check("a shared note cannot be overwritten by another member", "error" in clash and "同名" in clash["error"], clash)
        own = call("codex-win", "hub_memory_write", {"name": "codex-决定", "content": "共享决定：先修缩图再调口吃。", "scope": "shared"})
        check("members write shared notes directly", own.get("result", {}).get("scope") == "shared", own)
        check("authors can update their own shared note", call("codex-win", "hub_memory_write", {"name": "codex-决定", "content": "更新后的共享决定。", "scope": "shared"}).get("result", {}).get("stored") is True)

        call("dsh-win", "hub_memory_write", {"name": "dsh-草稿", "content": "DSH 自己的测试草稿。"})
        time.sleep(2)
        listed = call("codex-win", "hub_memory_list", {}).get("result", {}).get("notes", [])
        uris = {note["uri"] for note in listed}
        check("listing shows shared notes", shared_uri in uris and root + "shared/codex-决定.md" in uris, listed)
        check("listing hides other members' private notes", not any("/private/claude-win/" in uri or "/private/dsh-win/" in uri for uri in uris), listed)
        mine = call("dsh-win", "hub_memory_list", {"scope": "private"}).get("result", {}).get("notes", [])
        check("private listing shows only the caller's notes", [note["uri"] for note in mine] == [root + "private/dsh-win/dsh-草稿.md"], mine)

        found = []
        for _ in range(15):
            found = call("codex-win", "hub_memory_search", {"query": "原图直传 缩图"}).get("result", {}).get("notes", [])
            if any(note["uri"] == shared_uri for note in found):
                break
            time.sleep(4)
        check("search finds shared notes once indexed", any(note["uri"] == shared_uri for note in found), found)
        check("search never returns other members' private notes", not any("/private/claude-win/" in note["uri"] or "/private/dsh-win/" in note["uri"] for note in found), found)

        bad_scope = call("claude-win", "hub_memory_write", {"name": "x", "content": "y", "scope": "public"})
        check("invalid scope is refused with a reason", "scope" in bad_scope.get("error", ""), bad_scope)
        secret = call("claude-win", "hub_memory_write", {"name": "密钥", "content": "token sk-" + "a" * 30})
        check("credential-like content is refused", "凭据" in secret.get("error", ""), secret)

        no_key = call("claude-win", "hub_memory_list", {}, config=str(Path.home() / ".openviking" / "ovcli.conf"))
        check("missing top-level key reports an authentication problem instead of a silent no-op", "认证" in no_key.get("error", "") or "不可达" in no_key.get("error", ""), no_key)
    finally:
        cleanup = (
            "import asyncio\nfrom openviking_cli.client._http_compat import AsyncHTTPClient\n"
            "async def main():\n c=AsyncHTTPClient(timeout=30); await c.initialize()\n"
            f" await c.rm('{root}', recursive=True, wait=True)\n await c.close()\nasyncio.run(main())\n"
        )
        env = {key: value for key, value in os.environ.items() if not key.upper().startswith("OPENVIKING_")}
        # 必须等服务端删完再退出，否则删除请求会随进程退出被丢掉；失败要说出来，不能静默留下测试数据。
        removed = subprocess.run([PYTHON, "-c", cleanup], env={**env, "OPENVIKING_CLI_CONFIG_FILE": CONFIG}, capture_output=True, timeout=120)
        if removed.returncode != 0:
            print(f"WARN 测试项目 {root} 未能删除，请手动清理", flush=True)
    print(f"Project memory scopes: {passed} passed")


if __name__ == "__main__":
    main()
