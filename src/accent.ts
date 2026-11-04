/// 从背景图取按钮色：只取色相，饱和度、明暗压在固定窄范围里，换哪张图都不刺眼、白字都读得清。
/// 按饱和度给每个像素的色相投票；接近白/灰的不算。立绘里最艳的往往是脸颊和皮肤（红到桃），这一段只算一成，
/// 不然每张图的按钮都成了肉粉色；头发、眼睛、饰品的颜色才是一张图的气质。
export const DEFAULT_HUE = 248; // 同席原来的淡紫
export function hueOf(rgba: ArrayLike<number>): number {
  const bins = new Float64Array(36);
  let total = 0;
  for (let i = 0; i + 3 < rgba.length; i += 4) {
    const r = rgba[i] / 255, g = rgba[i + 1] / 255, b = rgba[i + 2] / 255;
    const max = Math.max(r, g, b), min = Math.min(r, g, b), chroma = max - min;
    if (chroma < .08 || rgba[i + 3] < 128) continue;
    let hue = max === r ? ((g - b) / chroma) % 6 : max === g ? (b - r) / chroma + 2 : (r - g) / chroma + 4;
    hue = (hue * 60 + 360) % 360;
    const weight = chroma * (hue >= 340 || hue < 45 ? .1 : 1);
    bins[Math.floor(hue / 10)] += weight; total += weight;
  }
  if (total < rgba.length / 4 * .01) return DEFAULT_HUE; // 几乎全是白和灰：用默认
  let best = 0;
  for (let i = 0; i < 36; i++) if (bins[i] + bins[(i + 35) % 36] * .5 + bins[(i + 1) % 36] * .5 > bins[best] + bins[(best + 35) % 36] * .5 + bins[(best + 1) % 36] * .5) best = i;
  return best * 10 + 5;
}
export const buttonColors = (hue: number) => ({ base: `hsl(${hue} 34% 64%)`, hover: `hsl(${hue} 34% 58%)` });
