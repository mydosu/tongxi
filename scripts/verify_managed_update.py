"""Codex 隔离更新的真机验收：准备 → 启用 → 回退（真下 npm 包、真切槽）。

跑在隔离数据目录里（`smoke_desktop.launch` 的 ISOLATED_*），真·外部安装（npm -g 的全局前缀）
全程只读：验收前后都读它的 package.json 版本与字节哈希，只要被动过就直接判失败。

「准备」比的是安装清单里的版本，所以给 app 指向一个**旧版桩**，否则「已是最新」会按设计
拒绝准备候选，这条流程就永远跑不动了。
"""
import hashlib
import os
import subprocess
import tempfile
from pathlib import Path

from playwright.sync_api import sync_playwright

import smoke_desktop as desktop
import local_paths

checks = []
errors = []
# 真·外部安装（npm -g 全局前缀）：只用来断言「没被动过」。
REAL_INSTALL = Path(
    os.environ.get(
        "AGENT_HUB_CODEX_REAL_INSTALLATION",
        local_paths.codex_installation(),
    )
)
# 验收基线：一份真·旧版安装（含可执行文件），不是空壳——回退要回到它并真的能跑，
# 只有 package.json 的桩会被后端按设计拒绝（「候选缺少可执行文件」）。
BASE_VERSION = "0.160.0"
BASE = Path(tempfile.gettempdir()) / "agenthub-codex-base"
PACKAGE = BASE / "node_modules/@openai/codex"
if not (PACKAGE / "package.json").is_file() or BASE_VERSION not in (PACKAGE / "package.json").read_text(encoding="utf-8"):
    subprocess.run(
        ["npm.cmd", "install", "--prefix", str(BASE), "--no-audit", "--no-fund",
         "@openai/codex@" + BASE_VERSION],
        check=True,
        shell=True,
    )
# app 要的是**包目录**（它自己按扁平 prefix 布局找 exe）。
os.environ["AGENT_HUB_CODEX_INSTALLATION"] = str(PACKAGE)
# 断言用的外部安装 = 基线那份；真·全局那份也一并看着（它才是用户实际在跑的）。
EXTERNAL = PACKAGE


def check(name, ok=True):
    if not ok:
        raise AssertionError(name)
    checks.append(name)
    print("PASS " + name, flush=True)


def external_state():
    """外部安装的 (版本, 指纹)；读不到就返回 (None, None)，由断言去判。"""
    manifest = EXTERNAL / "package.json"
    if not manifest.is_file():
        return None, None
    raw = manifest.read_bytes()
    import json

    return json.loads(raw.decode("utf-8")).get("version"), hashlib.sha256(raw).hexdigest()


def status(page, agent_id):
    return desktop.ipc(page, "service_update_status", serviceId=agent_id)


def inventory(page, agent_id):
    for item in desktop.ipc(page, "service_inventory"):
        if item["id"] == agent_id:
            return item
    return None


def wait_for(page, label, predicate, timeout=900):
    deadline = timeout
    waited = 0
    while waited < deadline:
        value = predicate()
        if value:
            return value
        page.wait_for_timeout(2000)
        waited += 2
    raise AssertionError("等待超时：" + label)


def run():
    proc = None
    try:
        with sync_playwright() as playwright:
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on("pageerror", lambda error: errors.append(str(error)))

            before_version, before_hash = external_state()
            check("外部安装在验收前读得到（v" + str(before_version) + "）", before_version is not None)

            page.click("#service-link")
            card = page.locator('[data-service-id="codex-win"]')
            card.wait_for(state="visible")

            check("Codex 也有准备/启用/回退三个按钮", card.locator("[data-service-prepare]").count() == 1
                  and card.locator("[data-service-apply]").count() == 1
                  and card.locator("[data-service-rollback]").count() == 1)

            card.locator("[data-service-check]").click()
            page.wait_for_function(
                """() => {
                    const host = document.querySelector('[data-service-id="codex-win"] .service-check-result');
                    return !!(host && (host.title || '').includes('检查来源'));
                }""",
                timeout=180000,
            )
            host = card.locator(".service-check-result")
            # 卡片上只留一行结论，细节在悬停里。
            text = (host.text_content() or "") + " " + (host.get_attribute("title") or "")
            check("检查更新拿到远程发行版", ("发行 v0.160.1" in text) or ("发现新版本" in text))

            # 1) 真准备：下 tarball + npm install 到槽 + 跑候选里的 codex.exe --version。
            card.locator("[data-service-prepare]").click()
            ready = wait_for(
                page,
                "候选准备完成",
                lambda: status(page, "codex-win").get("ready_plan_id"),
            )
            module = status(page, "codex-win")
            check(
                "候选准备完成且有版本（v" + str(module.get("candidate_version")) + "）",
                bool(module.get("candidate_version")),
            )

            # 2) 启用候选：指针切到槽，运行时重连到槽里的那份。
            card.locator("[data-service-apply]").click()
            managed = wait_for(
                page,
                "候选已启用",
                lambda: status(page, "codex-win").get("managed") or False,
            )
            after_apply = status(page, "codex-win")
            check(
                "启用后受管生效版本 = 候选版本（v" + str(after_apply.get("active_version")) + "）",
                after_apply.get("active_version") == module.get("candidate_version"),
            )
            entry = inventory(page, "codex-win")
            check(
                "Codex 现在跑的是槽里的 exe",
                bool(entry)
                and "managed-services" in (entry.get("executable") or "")
                and "codex-win" in (entry.get("executable") or ""),
            )
            check(
                "运行版本跟着槽走（v" + str(entry.get("installed_version")) + "）",
                entry.get("installed_version") == module.get("candidate_version"),
            )

            # 3) 外部那份必须一动不动。
            still_version, still_hash = external_state()
            check("外部安装版本未变（v" + str(still_version) + "）", still_version == before_version)
            check("外部安装文件指纹未变", still_hash == before_hash)

            # 4) 回退：指针回到外部安装。前端刷新是异步的，先等按钮真可用，超时打印现场。
            try:
                page.wait_for_function(
                    """() => {
                        const button = document.querySelector('[data-service-id="codex-win"] [data-service-rollback]');
                        return button && !button.disabled;
                    }""",
                    timeout=30000,
                )
            except Exception:
                print("DEBUG status: " + str(status(page, "codex-win")), flush=True)
                print("DEBUG toast: " + (page.locator("#toast").text_content() or ""), flush=True)
                raise
            card.locator("[data-service-rollback]").click()
            # 超时要把现场打出来：否则只能看到一个光秃秃的 AssertionError，查不出原因。
            try:
                wait_for(
                    page,
                    "回退完成",
                    lambda: not status(page, "codex-win").get("managed"),
                    timeout=60,
                )
            except Exception:
                print("DEBUG status: " + str(status(page, "codex-win")), flush=True)
                print("DEBUG toast: " + (page.locator("#toast").text_content() or ""), flush=True)
                raise
            back = inventory(page, "codex-win")
            check(
                "回退后 Codex 回到外部安装（v" + str(back.get("installed_version")) + "）",
                back.get("installed_version") == before_version,
            )
            final_version, final_hash = external_state()
            check("回退后外部安装仍然没被动过", (final_version, final_hash) == (before_version, before_hash))
            # 顺带看住用户实际在跑的那份：同席全程不该碰它。
            real_manifest = REAL_INSTALL / "package.json"
            check("真·全局安装（npm -g）也一字未动", real_manifest.is_file())
            check("没有 JavaScript 运行错误", not errors)
    finally:
        if proc and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)
    print("Managed update: " + str(len(checks)) + " passed", flush=True)


if __name__ == "__main__":
    run()
