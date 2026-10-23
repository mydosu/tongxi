"""将品牌 SVG 渲染为透明 PNG 和多尺寸 ICO。"""
from pathlib import Path
import struct

from playwright.sync_api import sync_playwright


root = Path(__file__).resolve().parents[1]
sizes = (16, 24, 32, 48, 64, 128, 256)
svg = (root / 'src' / 'assets' / 'brand.svg').read_text(encoding='utf-8')
pngs = {}

with sync_playwright() as playwright:
    browser = playwright.chromium.launch(headless=True)
    for size in sizes:
        # 各尺寸独立栅格化，保留小图的抗锯齿细节。
        page = browser.new_page(
            viewport={'width': size, 'height': size}, device_scale_factor=1,
        )
        # 去掉默认边距，避免视口裁切图标。
        page.set_content('<style>html,body{margin:0;overflow:hidden}svg{display:block}</style>' + svg)
        page.locator('svg').evaluate('''(svg, size) => {
            svg.setAttribute('width', size);
            svg.setAttribute('height', size);
        }''', size)
        pngs[size] = page.screenshot(omit_background=True)
        page.close()
    browser.close()

directory = root / 'src-tauri' / 'icons'
directory.mkdir(parents=True, exist_ok=True)
(directory / 'icon.png').write_bytes(pngs[256])

entries = []
offset = 6 + 16 * len(sizes)
for size in sizes:
    png = pngs[size]
    # ICO 用零表示 256 像素，PNG 原样嵌入以保留透明度。
    dimension = 0 if size == 256 else size
    entries.append(struct.pack('<BBBBHHII', dimension, dimension, 0, 0, 1, 32, len(png), offset))
    offset += len(png)
ico = struct.pack('<HHH', 0, 1, len(sizes)) + b''.join(entries) + b''.join(pngs.values())
icon_path = directory / 'icon.ico'
icon_path.write_bytes(ico)

# 读回文件，确认目录和 PNG 偏移实际写入正确。
saved = icon_path.read_bytes()
assert struct.unpack_from('<HHH', saved) == (0, 1, 7)
for index in range(7):
    entry = struct.unpack_from('<BBBBHHII', saved, 6 + 16 * index)
    assert saved[entry[7]:entry[7] + 8] == b'\x89PNG\r\n\x1a\n'

print(f"Generated icons: {', '.join(f'{size}px={len(pngs[size])} bytes' for size in sizes)}; ICO={len(ico)} bytes")
