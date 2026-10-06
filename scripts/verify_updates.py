"""服务页「检查更新」的 DOM 验收：远程比较是不是真的发生了（不开截图，会真连 npm 注册表）。

盯四件事：
1. Codex 现在做真实远程比较（检查来源 npm-registry、给出发行版本号与明确结论），
   但**不给更新入口**——同席不代用户更新外部安装的工具。
2. 卡片上那行「更新：」跟着结论走（点完还写「未检测」＝ 看着像没生效）。
3. DSH 的远程比较仍然成立，且它是有隔离安装槽的那一个：有新版时按钮带版本号。
4. Hermes 保持只做本地检测（检查来源 local），同样没有更新入口。
"""
import re

from playwright.sync_api import sync_playwright

import smoke_desktop as desktop

checks = []
errors = []


def check(name, condition=True):
    if not condition:
        raise AssertionError(name)
    checks.append(name)
    print("PASS " + name, flush=True)


def card_result(page, agent_id, timeout=120000):
    """点这张卡的「检查更新」，等结果落进 .service-check-result，返回 (卡片, 文字+悬停全文)。

    卡片上现在只留一行结论，细节在 title 里，所以断言要连 title 一起读。"""
    card = page.locator('[data-service-id="' + agent_id + '"]')
    card.locator("[data-service-check]").click()
    page.wait_for_function(
        """() => {
            const card = document.querySelector('[data-service-id="%s"]');
            const host = card && card.querySelector('.service-check-result');
            return !!(host && (host.title || '').includes('检查来源'));
        }"""
        % agent_id,
        timeout=timeout,
    )
    host = card.locator(".service-check-result")
    return card, (host.text_content() or "") + " " + (host.get_attribute("title") or "")


def source_of(text):
    found = re.search(r"检查来源 (\S+)", text)
    return found.group(1) if found else "?"


def run():
    proc = None
    try:
        with sync_playwright() as playwright:
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on("pageerror", lambda error: errors.append(str(error)))

            page.click("#service-link")
            page.locator('[data-service-id="codex-win"]').wait_for(state="visible")

            codex, text = card_result(page, "codex-win")
            check("Codex 走真实远程比较（检查来源 " + source_of(text) + "）", "检查来源 npm-registry" in text)
            released = re.search(r"发行 v(\d+\.\d+\.\d+)", text)
            check("Codex 拿到了远程发行版本号（" + (released.group(1) if released else "无") + "）", released is not None)
            check("Codex 给出了明确结论", ("发现新版本" in text) or ("无更新" in text))
            # 徽标那条要等 inventory 那批渲染落地（后端 service_inventory 要探测四个成员，慢一点）。
            badge_locator = codex.locator("[data-service-update]")
            badge_locator.wait_for(state="attached", timeout=60000)
            badge = badge_locator.text_content() or ""
            check("卡片上的「更新：」跟着结论走（" + badge.strip() + "）", "未检测" not in badge and ("发现新版本" in badge or "最新" in badge))
            check("Codex 也有更新入口（隔离槽，可准备候选）", codex.locator("[data-service-prepare]").count() == 1)
            check("Codex 的结论里说明可以准备隔离候选", "可准备隔离候选后再更新" in text)

            dsh, text = card_result(page, "dsh-win")
            check("DSH 的远程比较仍然成立（检查来源 " + source_of(text) + "）", "检查来源 npm-registry" in text)
            prepare = dsh.locator("[data-service-prepare]")
            check("DSH 有更新入口", prepare.count() == 1)
            label = (prepare.text_content() or "").strip()
            check("DSH 有新版时按钮带版本号（" + label + "）", ("发现新版本" not in text) or ("v" in label))

            hermes, text = card_result(page, "hermes-win")
            check("Hermes 走它自己的更新通道（检查来源 " + source_of(text) + "）", "检查来源 hermes-update" in text)
            check("Hermes 报出落后多少个提交", ("commits behind" in text) or ("已是最新" in text))
            check("Hermes 的本地版本被读出来了（不再写「未检测」）", "安装版本未检测" not in text)
            badge = hermes.locator("[data-service-update]").text_content() or ""
            check("Hermes 的徽标也跟着结论走（" + badge.strip() + "）", "未检测" not in badge)
            check("Hermes 目前没有更新入口", hermes.locator("[data-service-prepare]").count() == 0)
            check("没有 JavaScript 运行错误", not errors)
    finally:
        if proc and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)
    print("Updates: " + str(len(checks)) + " passed", flush=True)


if __name__ == "__main__":
    run()
