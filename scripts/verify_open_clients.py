"""在同席里一键打开各成员客户端的验收。

覆盖：Hermes 桌面端（真的拉起那个 exe）、codex（cmd 里跑 codex）、DSH（起 dsh web 并拿到
带 token 的地址交给浏览器）、阿尔比恩（按用户要求不做，应给诚实提示）；外加界面按钮的有无。

会真的在桌面上开窗口/浏览器——这是功能本身。测试自己拉起的进程在结束时关掉。

判据说明：
  · Hermes 桌面端通常**本来就在运行**（单实例），所以不拿「新增进程」当判据，而是命令返回
    成功 + 目标 exe 真实存在 + 有 Hermes 进程在；「真的把进程起起来了」由 codex（多一个跑
    codex 的命令行）和 DSH（服务起来并给出带 token 的地址）两项实测。
  · cmd 的窗口标题读不可靠，改用进程命令行判断。
"""
from __future__ import annotations

import json
import os
import re
import socket
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

os.environ.setdefault("AGENT_HUB_DSH_INSTALLATION", "D:/AI/dsh/bin/node_modules/@deepseek-ai/dsh")

import smoke_desktop as sd  # noqa: E402

from playwright.sync_api import sync_playwright  # noqa: E402

REPORT = "open-clients-verification.json"
DSH_PORT = 3080
HERMES_EXE = Path(os.path.expandvars(r"%LOCALAPPDATA%")) / "hermes/hermes-agent/apps/desktop/release/win-unpacked/Hermes.exe"


def powershell(script: str) -> str:
    result = subprocess.run(["powershell", "-NoProfile", "-Command", script],
                            capture_output=True, text=True, encoding="utf-8", errors="replace")
    return result.stdout or ""


def hermes_pids() -> set[int]:
    out = powershell("Get-Process Hermes -ErrorAction SilentlyContinue | ForEach-Object { $_.Id }")
    return {int(line) for line in out.split() if line.strip().isdigit()}


def codex_cmd_pids() -> set[int]:
    out = powershell(
        "Get-CimInstance Win32_Process -Filter \"Name='cmd.exe'\" | "
        "Where-Object { $_.CommandLine -like '*codex*' } | ForEach-Object { $_.ProcessId }"
    )
    return {int(line) for line in out.split() if line.strip().isdigit()}


def dsh_web_pids() -> set[int]:
    out = powershell(
        f"Get-NetTCPConnection -State Listen -LocalPort {DSH_PORT} -ErrorAction SilentlyContinue | "
        "Select-Object -First 1 -Expand OwningProcess"
    )
    return {int(line) for line in out.split() if line.strip().isdigit()}


def port_open(port: int) -> bool:
    with socket.socket() as sock:
        sock.settimeout(0.5)
        return sock.connect_ex(("127.0.0.1", port)) == 0


def kill(pids: set[int]) -> None:
    for pid in pids:
        powershell(f"taskkill /PID {pid} /T /F 2>$null | Out-Null")


def open_chat(page, conversation: str) -> None:
    page.evaluate("id => localStorage.setItem('hub.selected', id)", conversation)
    page.reload()
    page.locator("#message-input").wait_for(state="visible", timeout=30000)


def main() -> int:
    exe = sys.argv[1]
    sd.EXE = sd.ROOT / os.path.relpath(exe, sd.ROOT) if not os.path.isabs(exe) else Path(exe)
    sd.DATA = sd.ARTIFACTS / f"open-clients-{uuid.uuid4().hex[:12]}"
    sd.ISOLATED_SESSION_DB = True  # 这里不验会话库，别碰她的真实库

    checks: list[dict] = []
    started_hermes: set[int] = set()
    started_codex: set[int] = set()
    started_dsh: set[int] = set()

    def check(name: str, ok: bool, detail: object = None) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    hermes_before = hermes_pids()
    codex_before = codex_cmd_pids()
    dsh_was_running = port_open(DSH_PORT)

    proc, endpoint = sd.launch()
    playwright = sync_playwright().start()
    browser = playwright.chromium.connect_over_cdp(endpoint)
    page = sd.page_for(browser)

    try:
        # 1) Hermes 桌面端
        message = sd.ipc(page, "open_agent_client", agentId="hermes-win")
        time.sleep(4)
        check("hermes.command_ok", message == "已打开 Hermes 桌面端", message)
        check("hermes.exe_exists", HERMES_EXE.is_file(), str(HERMES_EXE))
        present = hermes_pids()
        check("hermes.client_present", bool(present),
              {"pids": sorted(present)[:4], "already_running": bool(hermes_before)})
        started_hermes = present - hermes_before

        # 2) codex：真的多出一个跑着 codex 的命令行
        message = sd.ipc(page, "open_agent_client", agentId="codex-win")
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and not (codex_cmd_pids() - codex_before):
            time.sleep(2)
        started_codex = codex_cmd_pids() - codex_before
        check("codex.terminal_launched", bool(started_codex), {"message": message, "pids": sorted(started_codex)})

        # 4) DSH：起 dsh web 并取到带 token 的地址
        message = sd.ipc(page, "open_agent_client", agentId="dsh-win")
        url_file = Path(sd.DATA) / "dsh-web-url.txt"
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline and not (port_open(DSH_PORT) and url_file.is_file()):
            time.sleep(2)
        url = url_file.read_text(encoding="utf-8").strip() if url_file.is_file() else ""
        check("dsh.web_started", port_open(DSH_PORT), {"message": message, "port": DSH_PORT})
        check("dsh.url_has_token", bool(re.match(r"^http://127\.0\.0\.1:3080/\?token=\S+$", url)), url[:60])
        again = sd.ipc(page, "open_agent_client", agentId="dsh-win")
        check("dsh.reuses_known_url", "已经在运行" in again, again)
        check("dsh.started_by_us", dsh_was_running or "已启动" in message, {"was_running": dsh_was_running, "message": message})
        if not dsh_was_running:
            started_dsh = dsh_web_pids()

        # 5) 阿尔比恩：按用户要求不做，应给诚实提示而不是假装成功
        try:
            sd.ipc(page, "open_agent_client", agentId="albion-wsl")
            check("albion.honest_message", False, "居然成功了")
        except Exception as error:
            check("albion.honest_message", "不做" in str(error) or "没有独立" in str(error), str(error)[:120])

        # 5) 界面：该有客户端的成员才有「打开客户端」按钮
        hermes_chat = sd.ipc(page, "create_conversation", title="打开客户端验收", kind="direct", members=["hermes-win"])["id"]
        open_chat(page, hermes_chat)
        check("ui.button_for_hermes", page.locator("#open-client").count() == 1, page.locator("#open-client").count())
        albion_chat = sd.ipc(page, "create_conversation", title="阿尔比恩（不该有按钮）", kind="direct", members=["albion-wsl"])["id"]
        open_chat(page, albion_chat)
        check("ui.no_button_for_albion", page.locator("#open-client").count() == 0, page.locator("#open-client").count())
    finally:
        kill(codex_cmd_pids() - codex_before)
        kill(started_hermes)
        if started_dsh:
            kill(started_dsh)  # 测试起的 DSH 服务收掉，免得占住端口
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
    print(f"OpenClients: {passed}/{len(checks)} passed")
    (sd.ARTIFACTS / REPORT).write_text(json.dumps(
        {"passed": passed, "total": len(checks), "checks": checks,
         "started": {"hermes": sorted(started_hermes), "codex_windows": sorted(started_codex), "dsh_web": sorted(started_dsh)}},
        ensure_ascii=False, indent=2), encoding="utf-8")
    return 0 if passed == len(checks) else 1


if __name__ == "__main__":
    raise SystemExit(main())
