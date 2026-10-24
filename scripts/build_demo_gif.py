"""把 docs/demo/demo.html 逐帧截图拼成 README 顶上的演示动图（中英各一张）。

用法：python scripts/build_demo_gif.py    需要 playwright（chromium）和 PATH 上的 ffmpeg。
"""
import pathlib
import subprocess
import tempfile

from playwright.sync_api import sync_playwright

ROOT = pathlib.Path(__file__).resolve().parent.parent
PAGE = (ROOT / 'docs' / 'demo' / 'demo.html').as_uri()
FPS = 12

with sync_playwright() as p:
    browser = p.chromium.launch()
    page = browser.new_page(viewport={'width': 960, 'height': 540})
    for lang, out in (('zh', 'demo.gif'), ('en', 'demo-en.gif')):
        page.goto(f'{PAGE}?lang={lang}')
        page.wait_for_timeout(300)
        frames = int(page.evaluate('DURATION') * FPS)
        with tempfile.TemporaryDirectory() as tmp:
            for i in range(frames):
                page.evaluate(f'render({i / FPS})')
                page.screenshot(path=f'{tmp}/{i:04d}.png')
            target = ROOT / 'docs' / 'images' / out
            # 两遍：先按整段画面算调色板，再用它抖动出 GIF，颜色不发灰。
            subprocess.run(['ffmpeg', '-y', '-loglevel', 'error', '-framerate', str(FPS), '-i', f'{tmp}/%04d.png',
                            '-vf', 'split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle',
                            '-loop', '0', str(target)], check=True)
            print(target, target.stat().st_size // 1024, 'KB')
    browser.close()
