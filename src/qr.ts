import qrcode from 'qrcode-generator';

/// 把文本编成二维码 SVG：深色模块、白底、四格静区，深色主题背景上也能扫。编码交给 qrcode-generator。
export function qrSvg(text: string, size = 180): string {
  const qr = qrcode(0, 'M');
  qr.addData(text);
  qr.make();
  const count = qr.getModuleCount(), quiet = 4;
  let path = '';
  for (let row = 0; row < count; row++) for (let col = 0; col < count; col++) if (qr.isDark(row, col)) path += `M${col + quiet} ${row + quiet}h1v1h-1z`;
  const side = count + quiet * 2;
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 ${side} ${side}" shape-rendering="crispEdges" role="img" aria-label="配对二维码"><rect width="${side}" height="${side}" fill="#fff"/><path d="${path}" fill="#1f2433"/></svg>`;
}
