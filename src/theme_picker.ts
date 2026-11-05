import { invoke, isRemote } from './transport';
import { loadImage } from './image_file';
import { buttonColors, DEFAULT_HUE, hueOf } from './accent';

// 主题背景：内置一套不含任何角色的公用背景；个人图片由用户自己导入（压缩后存在本机库里，只有这台电脑看得到）。
// 竖图默认放在右侧当立绘（白底正片叠底融进底色），横图铺满窗口再罩一层薄纱；「浓淡」只调那层纱，免得背景抢了内容。
type Escape = (value: string | number) => string;
type Veil = 'soft' | 'medium' | 'clear';
type Mode = 'figure' | 'cover' | 'plain';
interface Backdrop { id: string; name: string; mode: Mode; image?: string; thumb?: string; focus?: number | null; pastel?: boolean; custom?: boolean; }

const builtin = (id: string, name: string, image: string, thumb: string): Backdrop => ({ id, name, mode: 'cover', pastel: true, image, thumb });
const BUILTIN: Backdrop[] = [
  builtin('chenwu', '晨雾', new URL('./assets/themes/chenwu.webp', import.meta.url).href, new URL('./assets/themes/chenwu-thumb.webp', import.meta.url).href),
  builtin('bohe', '薄荷', new URL('./assets/themes/bohe.webp', import.meta.url).href, new URL('./assets/themes/bohe-thumb.webp', import.meta.url).href),
  builtin('xingguang', '杏光', new URL('./assets/themes/xingguang.webp', import.meta.url).href, new URL('./assets/themes/xingguang-thumb.webp', import.meta.url).href),
  builtin('haiyan', '海盐', new URL('./assets/themes/haiyan.webp', import.meta.url).href, new URL('./assets/themes/haiyan-thumb.webp', import.meta.url).href),
  builtin('muzi', '暮紫', new URL('./assets/themes/muzi.webp', import.meta.url).href, new URL('./assets/themes/muzi-thumb.webp', import.meta.url).href),
  { id: 'plain', name: '素净', mode: 'plain' },
];
// cover/figure：薄纱透明度；blur：场景主题下主面板的磨砂强度（越清晰越薄）。内置柔光背景本身就淡，纱减半。
// panel：桌面侧栏和主面板的白底（桌面和手机清晰档同值）。photo：手机上背景图本身的模糊（px）。fill：主图四周用同图放大补位那层的模糊，越清晰越清楚，但留一点虚免得和主图抢。手机没有实时磨砂，这一档就是背景的虚实；清晰档不糊。
const VEILS: Record<Veil, { name: string; cover: number; figure: number; blur: number; panel: number; photo: number; fill: number }> = {
  soft: { name: '淡雅', cover: .66, figure: .42, blur: 20, panel: .56, photo: 4, fill: 28 },
  medium: { name: '适中', cover: .34, figure: .16, blur: 12, panel: .44, photo: 2, fill: 22 },
  clear: { name: '清晰', cover: .12, figure: 0, blur: 0, panel: .12, photo: 0, fill: 10 },
};
/// 浓淡是 0（淡雅）～100（清晰）的滑条，50 是适中；中间各项参数在相邻两档间线性插值。
const STOPS: Record<Veil, number> = { soft: 0, medium: 50, clear: 100 };
function veilAt(value: number) {
  const [a, b, t] = value <= 50 ? [VEILS.soft, VEILS.medium, value / 50] : [VEILS.medium, VEILS.clear, (value - 50) / 50];
  const mix = (key: 'cover' | 'figure' | 'blur' | 'panel' | 'photo' | 'fill') => a[key] + (b[key] - a[key]) * t;
  return { cover: mix('cover'), figure: mix('figure'), blur: mix('blur'), panel: mix('panel'), photo: mix('photo'), fill: mix('fill') };
}
const KEY = 'hub.theme';
interface Choice { id: string; veil: number; }
interface Stored { id: string; name: string; mode: 'cover' | 'figure'; thumb: string; focus: number | null; }

let custom: Backdrop[] = [];
const images = new Map<string, string>();
const all = () => [...BUILTIN, ...custom];

function saved(): Choice {
  try {
    const value = JSON.parse(localStorage.getItem(KEY) || 'null') as { id?: unknown; veil?: unknown } | null;
    // 旧版存的是档位名，按档位换成滑条位置。
    const veil = typeof value?.veil === 'number' ? value.veil : STOPS[value?.veil as Veil];
    if (value && typeof value.id === 'string' && Number.isFinite(veil)) return { id: value.id, veil: Math.min(100, Math.max(0, veil)) };
  } catch { /* 读不到就用默认 */ }
  return { id: 'chenwu', veil: 50 };
}
let current = saved();
// 手机浏览器的 localStorage 是空的：第一次用桌面上导入的第一张自定义主题，没有再用默认。
let firstRemote = isRemote && localStorage.getItem(KEY) === null;
let front = 0;

/// 背景容器：两层图交替淡入，切换时不闪；底色渐变也在这里，立绘才能和它正片叠底。
export const backdropHtml = '<div class="theme-backdrop" aria-hidden="true"><div class="bg-layer"></div><div class="bg-layer"></div><div class="bg-veil"></div></div>';

async function loadCustom() {
  try {
    custom = (await invoke<Stored[]>('list_backgrounds')).map(item => ({ id: `custom:${item.id}`, name: item.name, mode: item.mode, thumb: item.thumb, focus: item.focus, custom: true }));
    pruneImages();
  } catch { custom = []; }
}
async function imageOf(theme: Backdrop) {
  if (!theme.custom) return theme.image;
  const id = theme.id.slice('custom:'.length);
  if (!images.has(id)) images.set(id, await cachedImage(id));
  return images.get(id);
}

/// 手机每次打开页面都要重取这张背景（几百 KB）；同一个 id 的图不会变（换图是新 id），按 id 存在本机。桌面窗口走本地调用，不缓存。
const imageKey = (id: string) => `hub.bg-image.${id}`;
async function cachedImage(id: string) {
  let saved: string | null = null;
  if (isRemote) try { saved = localStorage.getItem(imageKey(id)); } catch { /* 读不了就照常取 */ }
  if (saved) return saved;
  const image = await invoke<string>('background_image', { id });
  if (isRemote) try { localStorage.setItem(imageKey(id), image); } catch { /* 存不下（配额）就每次取 */ }
  return image;
}
/// 桌面上删掉的背景，手机本机存的那份也清掉。
function pruneImages() {
  if (!isRemote) return;
  try {
    const keep = new Set(custom.map(item => imageKey(item.id.slice('custom:'.length))));
    for (const key of Object.keys(localStorage)) if (key.startsWith('hub.bg-image.') && !keep.has(key)) localStorage.removeItem(key);
  } catch { /* 清不掉不影响显示 */ }
}

export async function applyTheme(choice: Choice = current, animate = true) {
  if (firstRemote) {
    firstRemote = false;
    await loadCustom();
    if (custom.length) choice = { ...choice, id: custom[0].id };
  }
  current = choice;
  try { localStorage.setItem(KEY, JSON.stringify(choice)); } catch { /* 只是偏好，存不下也照常显示 */ }
  if (choice.id.startsWith('custom:') && !custom.length) await loadCustom();
  const theme = all().find(item => item.id === choice.id) || BUILTIN[0];
  const image = await imageOf(theme).catch(() => undefined);
  if (current !== choice) return; // 读图期间又换了主题
  const look = image ? await inspect(image) : null;
  if (current !== choice) return;
  const root = document.documentElement;
  root.dataset.bgMode = theme.mode;
  paintAccent(look?.hue ?? DEFAULT_HUE);
  const veil = veilAt(choice.veil);
  // 暗图上深色字读不清：图越暗，面板白底越厚、磨砂越重，滑条拉到最清晰也保住可读性。
  const dark = theme.pastel ? 0 : Math.min(1, Math.max(0, (.62 - (look?.light ?? 1)) / .37));
  veil.panel = Math.max(veil.panel, .12 + .48 * dark); veil.blur = Math.max(veil.blur, 12 * dark);
  const base = theme.mode === 'cover' ? veil.cover : theme.mode === 'figure' ? veil.figure : 0;
  root.style.setProperty('--veil', String(theme.pastel ? base / 2 : base));
  root.dataset.veil = choice.veil >= 75 ? 'clear' : choice.veil < 25 ? 'soft' : 'medium'; // 手机清晰档的面板白底按最近的档位切
  root.style.setProperty('--panel-blur', `${veil.blur}px`);
  root.style.setProperty('--panel-alpha', String(veil.panel));
  root.style.setProperty('--fill-blur', `${veil.fill}px`);
  root.style.setProperty('--bg-filter', veil.photo ? `blur(${veil.photo}px)` : 'none');
  const fill = image && !theme.pastel && theme.mode !== 'plain' ? await fillOf(image, theme.mode) : null;
  if (current !== choice) return;
  const layers = [...document.querySelectorAll<HTMLElement>('.theme-backdrop .bg-layer')];
  if (layers.length < 2) return;
  const next = layers[animate ? 1 - front : front], previous = layers[animate ? front : 1 - front];
  if (next.dataset.theme !== `${theme.id}|${theme.mode}`) {
    // tinted：立绘的底不是白的，正片叠底会压成一块暗色方框，改成普通叠放、上沿也渐隐。
    next.className = `bg-layer ${theme.mode}${theme.pastel ? ' pastel' : ''}${theme.mode === 'figure' && look && !look.whiteEdges ? ' tinted' : ''}`; next.style.removeProperty('--bg-ar'); // pastel：内置柔光图，手机上照常铺满，不走「整张居中+模糊衬底」
    next.dataset.theme = `${theme.id}|${theme.mode}`;
    next.style.backgroundImage = image ? `url("${image}")` : 'none';
    next.style.setProperty('--bg-img', image ? `url("${image}")` : 'none'); // 手机上横图的底衬/前景用伪元素画，同一个 url 不会多解码一份
    // 手机上横图按宽度居中完整显示，前景那块的高度要靠宽高比定，边缘才能渐隐进模糊衬底。
    if (theme.mode !== 'plain' && !theme.pastel && image) { const probe = new Image(); const ready = () => next.classList.add('measured'); probe.onload = () => { if (probe.naturalWidth && probe.naturalHeight) next.style.setProperty('--bg-ar', `${probe.naturalWidth} / ${probe.naturalHeight}`); next.classList.toggle('soft', theme.mode === 'cover' && probe.naturalWidth < innerWidth * devicePixelRatio * .6); ready(); }; probe.onerror = ready; probe.src = image; }
  }
  next.style.backgroundPosition = theme.mode === 'cover' ? focusOf(theme) : '';
  next.classList.toggle('filled', !!fill);
  if (fill) next.style.setProperty('--fill-src', `url("${fill}")`); else next.style.removeProperty('--fill-src');
  next.classList.add('visible');
  if (previous !== next) previous.classList.remove('visible');
  if (animate) front = 1 - front;
}

/// 铺满时竖直方向露哪一段：用户拖过就用他的；没拖过偏上 30%（人物的脸多在上半部分）。
const focusOf = (theme: Backdrop) => theme.pastel ? 'center' : `center ${theme.focus ?? 30}%`;

/// 缩到 48×48 看一眼：色相（按钮色）、整体明暗、左右上三边是不是白底（立绘能不能正片叠底）。
interface Look { hue: number; light: number; whiteEdges: boolean; }
function look(picture: CanvasImageSource): Look {
  const canvas = document.createElement('canvas'); canvas.width = canvas.height = 48;
  const context = canvas.getContext('2d', { willReadFrequently: true })!;
  context.drawImage(picture, 0, 0, 48, 48);
  const data = context.getImageData(0, 0, 48, 48).data;
  let light = 0, edge = 0, edges = 0;
  for (let i = 0; i < data.length; i += 4) {
    const alpha = data[i + 3] / 255, x = (i / 4) % 48, y = Math.floor(i / 4 / 48);
    // 透明的地方露出的是浅色底，按白算。
    light += (0.2126 * data[i] + 0.7152 * data[i + 1] + 0.0722 * data[i + 2]) / 255 * alpha + (1 - alpha);
    if (x === 0 || x === 47 || y === 0) { edge += Math.min(data[i], data[i + 1], data[i + 2]) / 255 * alpha + (1 - alpha); edges++; }
  }
  return { hue: hueOf(data), light: light / (48 * 48), whiteEdges: edge / edges > .88 };
}
const looks = new Map<string, Promise<Look | null>>();
function inspect(image: string) {
  if (!looks.has(image)) looks.set(image, new Promise(resolve => {
    const picture = new Image();
    picture.onload = () => { try { resolve(look(picture)); } catch { resolve(null); } };
    picture.onerror = () => resolve(null);
    picture.src = image;
  }));
  return looks.get(image)!;
}

/// 主图四周空出来的地方：不再拿整张图放大模糊去垫（主图一清晰，后面那个放大的她就像重影），
/// 改成把主图最外一圈的颜色往外延伸（横图拉上下沿、立绘拉左右沿），边缘太杂时用整张图的取色渐变。只是颜色，不出现第二遍画面。
const fills = new Map<string, Promise<string | null>>();
function fillOf(image: string, mode: 'cover' | 'figure') {
  const key = `${mode}|${image}`;
  if (!fills.has(key)) fills.set(key, new Promise(resolve => {
    const picture = new Image();
    picture.onload = () => { try { resolve(edgeFill(picture, mode)); } catch { resolve(null); } };
    picture.onerror = () => resolve(null);
    picture.src = image;
  }));
  return fills.get(key)!;
}
function paletteFill(picture: HTMLImageElement) {
  const canvas = document.createElement('canvas'); canvas.width = canvas.height = 3;
  canvas.getContext('2d')!.drawImage(picture, 0, 0, 3, 3);
  return canvas.toDataURL();
}
/// 沿边取 16 段颜色（每段是边缘内侧 3% 那条带子的平均，避开描边和签名；段少一点，边上一个小物件不会被拉成一道长条），
/// 叠在取色渐变上：贴着主图的地方是纯边缘色，越往窗口外沿越淡、过渡成取色渐变。画外看着像画布延伸出去，人物不会出现第二次。
/// 立绘在会话区中间（整窗约 40%～80%），横图上下各一条带。边缘颜色起伏太大（拉出来是一道道杂纹）就只用取色渐变。
function edgeFill(picture: HTMLImageElement, mode: 'cover' | 'figure') {
  const w = picture.naturalWidth, h = picture.naturalHeight, n = 16, band = .03, cover = mode === 'cover';
  const strip = (sx: number, sy: number, sw: number, sh: number) => {
    const canvas = document.createElement('canvas'); canvas.width = cover ? n : 1; canvas.height = cover ? 1 : n;
    canvas.getContext('2d', { willReadFrequently: true })!.drawImage(picture, sx, sy, sw, sh, 0, 0, canvas.width, canvas.height);
    return canvas;
  };
  const [a, b] = cover
    ? [strip(0, 0, w, h * band), strip(0, h * (1 - band), w, h * band)]
    : [strip(0, 0, w * band, h), strip(w * (1 - band), 0, w * band, h)];
  if (Math.max(roughness(a), roughness(b)) > EDGE_BUSY) return paletteFill(picture);
  const out = document.createElement('canvas'); out.width = cover ? n : 8; out.height = cover ? 8 : n;
  const context = out.getContext('2d')!;
  const palette = document.createElement('canvas'); palette.width = palette.height = 3;
  palette.getContext('2d')!.drawImage(picture, 0, 0, 3, 3);
  context.drawImage(palette, 0, 0, out.width, out.height);
  // 8 格里每格边缘色的浓度：立绘左沿色铺在 1～3 格、右沿色在 4～7 格；横图上沿色在 0～3 行、下沿色在 4～7 行，最外一格淡一半。
  const weights = cover ? [.5, 1, 1, 1, 1, 1, 1, .5] : [0, .5, 1, 1, 1, 1, 1, .5];
  weights.forEach((alpha, i) => {
    if (!alpha) return;
    context.globalAlpha = alpha;
    const source = i < 4 ? a : b;
    if (cover) context.drawImage(source, 0, i, n, 1); else context.drawImage(source, i, 0, 1, n);
  });
  context.globalAlpha = 1;
  return out.toDataURL();
}
/// 相邻两段的平均亮度差（0～1）：纯色边 ≈0，渐变边很小，花纹、窗格、杂物边就大。
const EDGE_BUSY = .08;
function roughness(canvas: HTMLCanvasElement) {
  const data = canvas.getContext('2d', { willReadFrequently: true })!.getImageData(0, 0, canvas.width, canvas.height).data;
  const luma = (i: number) => (0.2126 * data[i] + 0.7152 * data[i + 1] + 0.0722 * data[i + 2]) / 255;
  let sum = 0;
  for (let i = 4; i < data.length; i += 4) sum += Math.abs(luma(i) - luma(i - 4));
  return sum / (data.length / 4 - 1);
}

/// 按钮色跟着背景图走（色相取自图，深浅固定，见 accent.ts）。图读不出像素就用默认淡紫。
function paintAccent(hue: number) {
  const { base, hover } = buttonColors(hue);
  document.documentElement.style.setProperty('--btn', base); document.documentElement.style.setProperty('--btn-hover', hover);
}

/// 读图片 → 压成 webp（长边 ≤ 3840，4K 屏铺满不发虚；太大再降到 2560）+ 360×240 缩略图。
/// 白底竖图默认当立绘，其余铺满（彩底竖图叠在右边会是一块方框）。
async function compress(file: File) {
  const picture = await loadImage(file).catch(() => { throw new Error(/hei[cf]/i.test(file.type + file.name) ? 'HEIC 格式读不了，请先转成 JPG / PNG / WebP' : '这张图片无法解码，请换成 JPG / PNG / WebP'); });
  {
    const encode = (edge: number) => {
      const scale = Math.min(1, edge / Math.max(picture.naturalWidth, picture.naturalHeight));
      const canvas = document.createElement('canvas');
      canvas.width = Math.round(picture.naturalWidth * scale); canvas.height = Math.round(picture.naturalHeight * scale);
      canvas.getContext('2d')!.drawImage(picture, 0, 0, canvas.width, canvas.height);
      return canvas.toDataURL('image/webp', .82);
    };
    let image = encode(3840);
    if (image.length > 7_500_000) image = encode(2560); // 库里单张上限 8MB
    const thumb = document.createElement('canvas');
    thumb.width = 360; thumb.height = 240;
    const cover = Math.max(360 / picture.naturalWidth, 240 / picture.naturalHeight);
    const w = picture.naturalWidth * cover, h = picture.naturalHeight * cover;
    thumb.getContext('2d')!.drawImage(picture, (360 - w) / 2, (240 - h) * .3, w, h);
    const mode = picture.naturalHeight > picture.naturalWidth * 1.15 && look(picture).whiteEdges ? 'figure' : 'cover';
    return { image, thumb: thumb.toDataURL('image/webp', .75), mode };
  }
}

export async function themePicker(host: { escape: Escape; open: (title: string, note: string, body: string) => void; modal: HTMLDialogElement; toast: (text: string, error?: boolean) => void }) {
  const { escape } = host;
  await loadCustom();
  const card = (theme: Backdrop) => `<div class="theme-card${theme.id === current.id ? ' selected' : ''}" data-theme-id="${escape(theme.id)}" role="button" tabindex="0" aria-pressed="${theme.id === current.id}"><span class="theme-thumb${theme.mode === 'plain' ? ' plain' : ''}"${theme.thumb ? ` style="background-image:url('${theme.thumb}')"` : ''}></span><span class="theme-name">${escape(theme.name)}<small>${theme.custom && !isRemote ? `<button type="button" class="theme-mode" data-theme-mode="${escape(theme.id)}" title="切换显示方式">${theme.mode === 'figure' ? '立绘' : '铺满'}</button><button type="button" class="theme-remove" data-theme-remove="${escape(theme.id)}" title="删除这张背景" aria-label="删除这张背景">×</button>` : theme.custom ? '自定义' : theme.mode === 'plain' ? '纯色' : '内置'}</small></span></div>`;
  const grid = () => `${all().map(card).join('')}${isRemote ? '' : '<label class="theme-card theme-import" title="导入你自己的图片（可多选），只保存在这台设备上"><input type="file" accept="image/*" multiple hidden data-theme-import><span class="theme-thumb plus">＋</span><span class="theme-name">导入图片<small>可多选</small></span></label>'}`;
  host.open('主题背景', '点一下立即生效。导入的图片只保存在这台设备上，竖图默认当立绘、横图铺满，可以在卡片上切换。', `<div class="theme-picker"><div class="theme-grid">${grid()}</div><div class="theme-veil"><span>背景浓淡</span><div class="veil-slider"><small>${VEILS.soft.name}</small><input type="range" min="0" max="100" step="1" value="${Math.round(current.veil)}" style="--fill:${Math.round(current.veil)}%" data-veil-range aria-label="背景浓淡，0 淡雅、100 清晰"><small>${VEILS.clear.name}</small></div></div><div class="theme-veil" data-focus-row hidden><span>画面位置</span><div class="veil-slider"><small>偏上</small><input type="range" min="0" max="100" step="1" data-focus-range aria-label="铺满时画面露出的位置，0 最上、100 最下"><small>偏下</small></div></div><div class="modal-footer"><span>内置背景不含任何人物，适合所有人</span><button type="button" class="primary" data-theme-done>完成</button></div></div>`);
  const root = host.modal.querySelector<HTMLElement>('.theme-picker')!;
  // 只有自定义的铺满图才有「画面位置」：内置柔光图没有主体，立绘贴着右下角。
  const focusRow = root.querySelector<HTMLElement>('[data-focus-row]')!, focusInput = root.querySelector<HTMLInputElement>('[data-focus-range]')!;
  const syncFocus = () => {
    const theme = custom.find(item => item.id === current.id);
    focusRow.hidden = isRemote || theme?.mode !== 'cover';
    const value = String(Math.round(theme?.focus ?? 30));
    focusInput.value = value; focusInput.style.setProperty('--fill', `${value}%`);
  };
  const repaint = () => { root.querySelector('.theme-grid')!.innerHTML = grid(); syncFocus(); };
  syncFocus();
  focusInput.addEventListener('input', () => {
    const theme = custom.find(item => item.id === current.id);
    if (!theme) return;
    theme.focus = Number(focusInput.value); focusInput.style.setProperty('--fill', `${focusInput.value}%`);
    void applyTheme(current, false);
  });
  focusInput.addEventListener('change', () => {
    const theme = custom.find(item => item.id === current.id);
    if (theme) invoke('set_background_focus', { id: theme.id.slice('custom:'.length), focus: theme.focus }).catch(error => host.toast(String(error), true));
  });
  root.addEventListener('change', async event => {
    const input = (event.target as HTMLElement).closest<HTMLInputElement>('[data-theme-import]');
    if (!input?.files?.length) return;
    const files = [...input.files];
    let last = '', ok = 0, still = 0;
    for (const file of files) {
      try {
        const { image, thumb, mode } = await compress(file);
        last = await invoke<string>('add_background', { name: file.name.replace(/\.[^.]+$/, ''), mode, image, thumb });
        ok++; if (file.type === 'image/gif') still++;
      } catch (error) { host.toast(`${file.name}：${error instanceof Error ? error.message : String(error)}`, true); }
    }
    await loadCustom(); repaint();
    if (last) { await applyTheme({ ...current, id: `custom:${last}` }); repaint(); if (ok === files.length) host.toast(`已导入 ${ok} 张图片${still ? `（动图只保留第一帧）` : ''}`); }
  });
  root.addEventListener('click', async event => {
    const target = event.target as HTMLElement;
    const remove = target.closest<HTMLElement>('[data-theme-remove]');
    const mode = target.closest<HTMLElement>('[data-theme-mode]');
    if (remove) {
      event.stopPropagation();
      const id = remove.dataset.themeRemove!;
      await invoke('remove_background', { id: id.slice('custom:'.length) });
      images.delete(id.slice('custom:'.length));
      try { localStorage.removeItem(imageKey(id.slice('custom:'.length))); } catch { /* 没存过就算了 */ }
      await loadCustom();
      if (current.id === id) await applyTheme({ ...current, id: 'chenwu' });
      repaint(); return;
    }
    if (mode) {
      event.stopPropagation();
      const theme = custom.find(item => item.id === mode.dataset.themeMode)!;
      await invoke('set_background_mode', { id: theme.id.slice('custom:'.length), mode: theme.mode === 'figure' ? 'cover' : 'figure' });
      await loadCustom();
      if (current.id === theme.id) await applyTheme(current);
      repaint(); return;
    }
    const theme = target.closest<HTMLElement>('[data-theme-id]');
    if (theme && theme.dataset.themeId !== current.id) {
      await applyTheme({ ...current, id: theme.dataset.themeId! });
      repaint();
    }
    if (target.closest('[data-theme-done]')) host.modal.close();
  });
  root.querySelector<HTMLInputElement>('[data-veil-range]')!.addEventListener('input', event => {
    const input = event.target as HTMLInputElement;
    input.style.setProperty('--fill', `${input.value}%`);
    void applyTheme({ ...current, veil: Number(input.value) }, false);
  });
  root.addEventListener('keydown', event => {
    const theme = (event.target as HTMLElement).closest<HTMLElement>('[data-theme-id]');
    if (theme && (event.key === 'Enter' || event.key === ' ')) { event.preventDefault(); theme.click(); }
  });
}
