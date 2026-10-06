"""弹窗外观与会话模型选择器的 DOM 验收（不开截图）。

盯三件事：
1. 头部那个「模型设置」按钮已删除（模型在成员那一行的内联下拉里选）。
2. 弹窗遮罩仍是半透明 + 模糊（照 Hermes 的 `bg-black/22` + `backdrop-blur(.125rem)`）；
   弹窗现在只从 HUD 的 ⌄ 进，所以这里直接读样式表那条规则。
3. 内联的模型控件**永远是下拉**，且未连接该成员时旁边给出「连接并读取模型」；点完列表真出现。
"""
import sys

from playwright.sync_api import sync_playwright

import smoke_desktop as desktop

checks = []
errors = []


def check(name, condition=True):
    if not condition:
        raise AssertionError(name)
    checks.append(name)
    print("PASS " + name, flush=True)


def run():
    proc = None
    try:
        with sync_playwright() as playwright:
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on("pageerror", lambda error: errors.append(str(error)))

            check("头部的「模型设置」按钮已删除", page.locator("#model-settings").count() == 0)

            # 固定验收程序已按用户要求删除：绑定项目只选目录，不再要求填一条命令。
            if page.locator("#bind-project").count():
                page.click("#bind-project")
                fields = ["#check-name", "#check-program", "#check-args", "#check-timeout"]
                check("绑定项目弹窗不再有验收程序字段", all(page.locator(selector).count() == 0 for selector in fields))
                page.click("#modal-close")

            # 输入框从一行起、随内容长高（Hermes 那种），不再是固定三行的方框。
            one_line = page.eval_on_selector("#message-input", "el => el.clientHeight")
            check("输入框初始只有一行高（" + str(one_line) + "px）", one_line < 32)
            page.fill("#message-input", "\n".join("第 " + str(index) + " 行内容" for index in range(1, 9)))
            grown = page.eval_on_selector("#message-input", "el => el.clientHeight")
            check("输入多行后输入框变高（" + str(one_line) + " → " + str(grown) + "px）", grown > one_line + 60)
            page.fill("#message-input", "")

            backdrop = page.evaluate(
                """() => {
                    let background = '', blur = '';
                    for (const sheet of document.styleSheets) {
                        try {
                            for (const rule of sheet.cssRules) {
                                if (rule.selectorText && rule.selectorText.includes('::backdrop')) {
                                    background = rule.style.background || rule.style.backgroundColor;
                                    blur = rule.style.backdropFilter || rule.style.webkitBackdropFilter;
                                }
                            }
                        } catch (error) { /* 跨域样式表跳过 */ }
                    }
                    return { background: background, blur: blur };
                }"""
            )
            check(
                "弹窗遮罩是半透明而不是实心深灰（" + str(backdrop["background"]) + "）",
                bool(backdrop["background"])
                and ("rgba(" in backdrop["background"] or "rgb(" in backdrop["background"])
                and "0.22" in backdrop["background"].replace(" ", ""),
            )
            check("弹窗遮罩带模糊（" + str(backdrop["blur"]) + "）", "blur" in (backdrop["blur"] or ""))

            # 成员那一行：模型永远是下拉；未连接时给连接入口。
            if page.locator("#live-agent").count():
                members = page.eval_on_selector_all("#live-agent option", "els => els.map(e => e.value)")
                if "hermes-win" in members:
                    page.select_option("#live-agent", "hermes-win")
            tag = page.eval_on_selector("#live-model", "el => el.tagName")
            check("内联的模型控件是下拉而不是手填（当前 " + str(tag) + "）", tag == "SELECT")

            # 没有连接时不能是死路：要有「连接并读取模型」入口，连上之后列表要真出现。
            page.locator("#live-connect").wait_for(state="visible")
            before = page.eval_on_selector_all("#live-model option", "els => els.length")
            check("未连接该成员时给出「连接并读取模型」入口", page.locator("#live-connect").is_visible())
            page.click("#live-connect")
            page.wait_for_function("() => document.querySelectorAll('#live-model option').length > 1", timeout=120000)
            after = page.eval_on_selector_all("#live-model option", "els => els.length")
            check("连上之后模型列表真的出现（" + str(before) + " → " + str(after) + " 项）", after > 1)
            check("连上之后不再显示连接入口", page.locator("#live-connect").count() == 0)
            check("没有 JavaScript 运行错误", not errors)
    finally:
        if proc and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)
    print("Modal: " + str(len(checks)) + " passed", flush=True)


if __name__ == "__main__":
    run()
