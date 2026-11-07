"""生成安卓自适应图标的前景：只画 brand.svg 里那两只交织的菱形，透明底。

为什么要单独出这张图：`src-tauri/icons/icon.png` 是带圆角底板和描边的整张图，
直接塞进自适应图标会被启动器再套一层形状，看起来像「框中框」。前景只留菱形，
底板由 res/drawable/ic_launcher_background.xml 的渐变补上。

不碰的文件：mipmap-*/ic_launcher.png、ic_launcher_round.png（旧版安卓的方形/圆形
图标，仍用 scripts/build_icon.py 出的带底板图）、src-tauri/icons/ 下的桌面图标。

用法（换图标后重跑）：py mobile/scripts/build_android_icon.py
"""
from pathlib import Path
import re

from PIL import Image
from playwright.sync_api import sync_playwright

root = Path(__file__).resolve().parents[1]          # mobile/
res = root / 'src-tauri' / 'gen' / 'android' / 'app' / 'src' / 'main' / 'res'
svg = (root.parent / 'src' / 'assets' / 'brand.svg').read_text(encoding='utf-8')

# 自适应图标画布 108dp，安全区是中间 66dp；菱形含 21 宽描边的可见外框在 svg 里是
# x 31.5~224.5（宽 193），缩到画布的 60% 居中，正好落在安全区里。
CANVAS = 108
INK_RATIO = 0.60
INK_WIDTH = 193          # 含描边，见上面的说明
SCALE = CANVAS * INK_RATIO / INK_WIDTH
DENSITIES = {'mdpi': 108, 'hdpi': 162, 'xhdpi': 216, 'xxhdpi': 324, 'xxxhdpi': 432}

# defs 里是两只菱形的渐变和交织用的 mask；mask 自己的 <rect> 没有 x=，下面只删带 x= 的底板/描边。
defs = re.search(r'<defs>.*?</defs>', svg, re.S).group(0)
art = re.search(r'<g fill="none".*?</g>', svg, re.S).group(0)
shapes = [r for r in re.findall(r'<rect[^>]*/>', svg) if re.search(r'\bx="\d+"', r)]
assert len(shapes) == 2, f'底板 rect 数量不对：{len(shapes)}'
foreground = (
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 108 108">'
    + defs
    + f'<g transform="translate({CANVAS / 2} {CANVAS / 2}) scale({SCALE:.6f}) translate(-128 -128)">'
    + art
    + '</g></svg>'
)

pngs = {}
with sync_playwright() as playwright:
    browser = playwright.chromium.launch(headless=True)
    for density, size in DENSITIES.items():
        page = browser.new_page(viewport={'width': size, 'height': size}, device_scale_factor=1)
        page.set_content(
            '<style>html,body{margin:0;background:transparent;overflow:hidden}svg{display:block}</style>'
            + foreground
        )
        page.locator('svg').evaluate(
            '(svg, size) => { svg.setAttribute("width", size); svg.setAttribute("height", size); }', size
        )
        pngs[density] = page.screenshot(omit_background=True)
        page.close()
    browser.close()

# 自检：尺寸对、四角透明、菱形在中间且占比符合预期、中心区域确实有东西。
def check(density: str, size: int, data: bytes) -> None:
    path = res / f'mipmap-{density}' / 'ic_launcher_foreground.png'
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    image = Image.open(path).convert('RGBA')
    assert image.size == (size, size), f'{density}: 尺寸 {image.size} != {(size, size)}'
    for corner in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)]:
        assert image.getpixel(corner)[3] == 0, f'{density}: 角落 {corner} 不透明'
    alpha = image.getchannel('A')
    box = alpha.getbbox()
    assert box, f'{density}: 整张都是空的'
    width, height = box[2] - box[0], box[3] - box[1]
    assert 0.56 <= width / size <= 0.64, f'{density}: 菱形宽 {width / size:.1%}，应在 56%~64%'
    assert 0.40 <= height / size <= 0.48, f'{density}: 菱形高 {height / size:.1%}，应在 40%~48%'
    for axis, (low, high) in {'x': (box[0], box[2]), 'y': (box[1], box[3])}.items():
        center = (low + high) / 2
        assert abs(center - size / 2) <= size * 0.03, f'{density}: {axis} 方向没居中（{center:.1f}/{size / 2}）'
    middle = size * 0.1
    inner = alpha.crop((int(size / 2 - middle), int(size / 2 - middle), int(size / 2 + middle), int(size / 2 + middle)))
    assert inner.getextrema()[1] > 0, f'{density}: 中心区域没有不透明像素'
    ink = sum(alpha.histogram()[33:]) / (size * size)   # alpha > 32 的像素占比
    print(f'  mipmap-{density}/ic_launcher_foreground.png {size}x{size}  宽 {width / size:.1%}  高 {height / size:.1%}  墨迹 {ink:.1%}')


print(f'缩放 {SCALE:.4f}（菱形含描边宽 {INK_WIDTH} → 画布 {CANVAS} 的 {INK_RATIO:.0%}）')
for density, size in DENSITIES.items():
    check(density, size, pngs[density])
print('前景图自检通过')
