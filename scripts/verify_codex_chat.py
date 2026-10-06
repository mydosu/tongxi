"""Real Codex end-to-end verification via WebView2; NOT system input."""
import verify_native_input as verification


class BrowserControl:
    def __init__(self, process, page):
        self.page = page

    def click(self, selector):
        self.page.locator(selector).click()
        verification.actions.append({'type': 'cdp_click', 'target': selector})

    def type(self, text):
        self.page.locator(':focus').fill(text)
        verification.actions.append({'type': 'cdp_fill', 'characters': len(text)})

    def hotkey(self, *keys):
        names = {0x11: 'Control', 0x10: 'Shift', 0x0D: 'Enter', 0x41: 'a'}
        self.page.keyboard.press('+'.join(names[key] for key in keys))
        verification.actions.append({'type': 'cdp_keyboard', 'keys': list(keys)})

    def screenshot(self, name):
        self.page.screenshot(path=str(verification.desktop.ARTIFACTS / ('cdp-' + name)))


if __name__ == '__main__':
    verification.run(control_factory=BrowserControl, report_name='codex-chat-verification.json', native=False)
