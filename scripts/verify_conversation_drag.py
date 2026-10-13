"""侧栏会话卡片拖动排序与就地改名的发布 EXE 专项：真实鼠标拖动、落点前后、刷新后保持、新消息不打乱；
右键与双击标题改名、Esc 取消；成员描述不含固定分工。
不调用模型，使用隔离数据目录。"""
from __future__ import annotations

import shutil
import tempfile
import uuid
from pathlib import Path

from playwright.sync_api import sync_playwright

import smoke_desktop as desktop

passed = 0


def check(label: str, condition: bool) -> None:
    global passed
    if not condition:
        raise AssertionError(label)
    passed += 1
    print("PASS " + label, flush=True)


def order(page) -> list[str]:
    return page.eval_on_selector_all("[data-order-id]", "items => items.map(item => item.dataset.orderId)")


def drag(page, source: str, target: str, after: bool) -> None:
    """逐步移动的真实鼠标按下-移动-松开（不是合成的 HTML5 拖放事件）。"""
    start = page.locator(f'[data-order-id="{source}"] .conversation').bounding_box()
    end = page.locator(f'[data-order-id="{target}"] .conversation').bounding_box()
    page.mouse.move(start["x"] + start["width"] / 2, start["y"] + start["height"] / 2)
    page.mouse.down()
    page.mouse.move(end["x"] + end["width"] / 2, end["y"] + end["height"] * (0.8 if after else 0.2), steps=12)
    page.mouse.up()


def main() -> None:
    desktop.DATA = Path(tempfile.gettempdir()) / ("tongxi-drag-data-" + uuid.uuid4().hex[:10])
    desktop.ISOLATED_SESSION_DB = True
    proc = browser = None
    try:
        with sync_playwright() as playwright:
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.locator("[data-order-id]").first.wait_for()
            before = order(page)
            check("sidebar lists at least three cards", len(before) >= 3)
            selected = page.evaluate("localStorage.getItem('hub.selected')")

            drag(page, before[-1], before[0], after=False)
            expected = [before[-1]] + before[:-1]
            page.wait_for_function("ids => [...document.querySelectorAll('[data-order-id]')].map(i => i.dataset.orderId).join() === ids.join()", arg=expected)
            check("dropping on the upper half moves the card before the target", order(page) == expected)

            drag(page, expected[0], expected[1], after=True)
            expected = [expected[1], expected[0]] + expected[2:]
            page.wait_for_function("ids => [...document.querySelectorAll('[data-order-id]')].map(i => i.dataset.orderId).join() === ids.join()", arg=expected)
            check("dropping on the lower half moves the card after the target", order(page) == expected)

            stored = [room["id"] for room in desktop.ipc(page, "list_conversations", search="", archived=False)]
            check("backend list returns the dragged order", stored == expected)
            check("finishing a drag does not open the dragged conversation", page.evaluate("localStorage.getItem('hub.selected')") == selected)
            page.locator(f'[data-order-id="{expected[2]}"] .conversation').click()
            page.wait_for_function("id => localStorage.getItem('hub.selected') === id", arg=expected[2])
            check("a plain click still opens a conversation", True)
            desktop.ipc(page, "save_local_message", conversationId=expected[-1], messageId=str(uuid.uuid4()), content="新的消息")
            page.reload()
            page.locator("[data-order-id]").first.wait_for()
            check("order survives reload and new messages", order(page) == expected)

            target = expected[1]
            page.locator(f'[data-conversation="{target}"]').click(button="right")
            page.locator("[data-conversation-rename]").click()
            field = page.locator(f'[data-conversation="{target}"] .inline-rename')
            field.fill("右键改的名字")
            field.press("Enter")
            page.wait_for_function("id => document.querySelector(`[data-conversation=\"${id}\"] .conversation-title`)?.textContent === '右键改的名字'", arg=target)
            check("sidebar right-click renames in place", desktop.ipc(page, "get_conversation", id=target)["conversation"]["title"] == "右键改的名字")
            check("renaming keeps the dragged order", order(page) == expected)

            desktop.choose(page, target)
            page.locator(".chat-heading h1").dblclick()
            page.locator(".chat-heading h1 .inline-rename").fill("不保存")
            page.locator(".chat-heading h1 .inline-rename").press("Escape")
            check("Escape cancels a header rename", page.locator(".chat-heading h1").inner_text() == "右键改的名字")
            page.locator(".chat-heading h1").dblclick()
            page.locator(".chat-heading h1 .inline-rename").fill("双击改的名字")
            page.locator(".chat-heading h1 .inline-rename").press("Enter")
            page.wait_for_function("id => document.querySelector(`[data-conversation=\"${id}\"] .conversation-title`)?.textContent === '双击改的名字'", arg=target)
            check("double-clicking the header renames and updates the sidebar", page.locator(".chat-heading h1").inner_text() == "双击改的名字")

            roles = desktop.ipc(page, "list_agents")
            check("member descriptions no longer assign fixed roles", not any(word in agent["subtitle"] + agent["role"] for agent in roles for word in ("管家", "方案", "按 Codex", "全部编码")))
            check("no JavaScript errors", not errors)
    finally:
        if browser is not None:
            try:
                browser.close()
            except Exception:
                pass
        if proc is not None:
            desktop.subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"], capture_output=True)
        shutil.rmtree(desktop.DATA, ignore_errors=True)
    print(f"Sidebar drag and rename: {passed} passed")


if __name__ == "__main__":
    main()
