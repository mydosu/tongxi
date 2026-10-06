"""Generate the application's own geometric icon using only Python stdlib."""
from pathlib import Path
import math
import struct
import zlib

root = Path(__file__).resolve().parents[1]
size = 64
pixels = bytearray()
nodes = [(21, 21, (231, 202, 135)), (43, 21, (226, 239, 229)),
         (43, 43, (151, 188, 224)), (21, 43, (229, 184, 181))]
# 银白主题：底板用银灰竖向渐变，十字用近白，四个成员点保持原色。
top = (124, 133, 144)
bottom = (74, 81, 88)
for y in range(size):
    pixels.append(0)
    ratio = y / (size - 1)
    tile = tuple(round(top[i] + (bottom[i] - top[i]) * ratio) for i in range(3))
    for x in range(size):
        inset_x = max(12 - x, 0, x - 51)
        inset_y = max(12 - y, 0, y - 51)
        inside = inset_x * inset_x + inset_y * inset_y <= 144
        color = (*tile, 255 if inside else 0)
        if inside and (((20 <= x <= 22 or 42 <= x <= 44) and 21 <= y <= 43) or
                       ((20 <= y <= 22 or 42 <= y <= 44) and 21 <= x <= 43)):
            color = (240, 243, 246, 255)
        for cx, cy, fill in nodes:
            if math.hypot(x - cx, y - cy) <= 4.5:
                color = (*fill, 255)
        pixels.extend(color)

def chunk(kind, data):
    return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data) & 0xffffffff)

png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!IIBBBBB', size, size, 8, 6, 0, 0, 0))
png += chunk(b'IDAT', zlib.compress(bytes(pixels))) + chunk(b'IEND', b'')
directory = root / 'src-tauri' / 'icons'
directory.mkdir(parents=True, exist_ok=True)
(directory / 'icon.png').write_bytes(png)
ico = struct.pack('<HHH', 0, 1, 1) + struct.pack('<BBBBHHII', size, size, 0, 0, 1, 32, len(png), 22) + png
(directory / 'icon.ico').write_bytes(ico)
print(f'Generated icon: {len(ico)} bytes')
