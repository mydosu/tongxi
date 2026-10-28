import { invoke, isRemote } from './transport';
import { loadImage } from './image_file';

// 主题背景：内置一套不含任何角色的公用背景；个人图片由用户自己导入（压缩后存在本机库里，只有这台电脑看得到）。
// 竖图默认放在右侧当立绘（白底正片叠底融进底色），横图铺满窗口再罩一层薄纱；「浓淡」只调那层纱，免得背景抢了内容。
type Escape = (value: string | number) => string;
export type Veil = 'soft' | 'medium' | 'clear';
type Mode = 'figure' | 'cover' | 'plain';
interface Backdrop { id: string; name: string; mode: Mode; image?: string; thumb?: string; position?: string; pastel?: boolean; custom?: boolean; }

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
const VEILS: Record<Veil, { name: string; cover: number; figure: number; blur: number }> = {
  soft: { name: '淡雅', cover: .66, figure: .42, blur: 18 },
  medium: { name: '适中', cover: .34, figure: .16, blur: 9 },
  clear: { name: '清晰', cover: .24, figure: 0, blur: 5 },
};
const KEY = 'hub.theme';
interface Choice { id: string; veil: Veil; }
interface Stored { id: string; name: string; mode: 'cover' | 'figure'; thumb: string; }

let custom: Backdrop[] = [];
const images = new Map<string, string>();
const all = () => [...BUILTIN, ...custom];

function saved(): Choice {
  try {
    const value = JSON.parse(localStorage.getItem(KEY) || 'null') as Choice | null;
    if (value && typeof value.id === 'string' && value.veil in VEILS) return value;
  } catch { /* 读不到就用默认 */ }
  return { id: 'chenwu', veil: 'medium' };
}
let current = saved();
// 手机浏览器的 localStorage 是空的：第一次用桌面上导入的第一张自定义主题，没有再用默认。
let firstRemote = isRemote && localStorage.getItem(KEY) === null;
let front = 0;

/// 背景容器：两层图交替淡入，切换时不闪；底色渐变也在这里，立绘才能和它正片叠底。
export const backdropHtml = '<div class="theme-backdrop" aria-hidden="true"><div class="bg-layer"></div><div class="bg-layer"></div><div class="bg-veil"></div></div>';

async function loadCustom() {
  try {
    custom = (await invoke<Stored[]>('list_backgrounds')).map(item => ({ id: `custom:${item.id}`, name: item.name, mode: item.mode, thumb: item.thumb, custom: true }));
  } catch { custom = []; }
}
async function imageOf(theme: Backdrop) {
  if (!theme.custom) return theme.image;
  const id = theme.id.slice('custom:'.length);
  if (!images.has(id)) images.set(id, await invoke<string>('background_image', { id }));
  return images.get(id);
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
  const root = document.documentElement;
  root.dataset.bgMode = theme.mode;
  const veil = VEILS[choice.veil];
  const base = theme.mode === 'cover' ? veil.cover : theme.mode === 'figure' ? veil.figure : 0;
  root.style.setProperty('--veil', String(theme.pastel ? base / 2 : base));
  root.style.setProperty('--panel-blur', `${veil.blur}px`);
  const layers = [...document.querySelectorAll<HTMLElement>('.theme-backdrop .bg-layer')];
  if (layers.length < 2) return;
  const next = layers[animate ? 1 - front : front], previous = layers[animate ? front : 1 - front];
  if (next.dataset.theme !== `${theme.id}|${theme.mode}`) {
    next.className = `bg-layer ${theme.mode}`;
    next.dataset.theme = `${theme.id}|${theme.mode}`;
    next.style.backgroundImage = image ? `url("${image}")` : 'none';
    next.style.backgroundPosition = theme.mode === 'cover' ? theme.position || 'center' : '';
  }
  next.classList.add('visible');
  if (previous !== next) previous.classList.remove('visible');
  if (animate) front = 1 - front;
}

/// 读图片 → 压成 webp（长边 ≤ 2400）+ 360×240 缩略图。竖图默认当立绘，横图铺满。
async function compress(file: File) {
  const picture = await loadImage(file);
  {
    const scale = Math.min(1, 2400 / Math.max(picture.naturalWidth, picture.naturalHeight));
    const canvas = document.createElement('canvas');
    canvas.width = Math.round(picture.naturalWidth * scale); canvas.height = Math.round(picture.naturalHeight * scale);
    canvas.getContext('2d')!.drawImage(picture, 0, 0, canvas.width, canvas.height);
    const image = canvas.toDataURL('image/webp', .82);
    const thumb = document.createElement('canvas');
    thumb.width = 360; thumb.height = 240;
    const cover = Math.max(360 / picture.naturalWidth, 240 / picture.naturalHeight);
    const w = picture.naturalWidth * cover, h = picture.naturalHeight * cover;
    thumb.getContext('2d')!.drawImage(picture, (360 - w) / 2, (240 - h) * .3, w, h);
    const mode = picture.naturalHeight > picture.naturalWidth * 1.15 ? 'figure' : 'cover';
    return { image, thumb: thumb.toDataURL('image/webp', .75), mode };
  }
}

export async function themePicker(host: { escape: Escape; open: (title: string, note: string, body: string) => void; modal: HTMLDialogElement; toast: (text: string, error?: boolean) => void }) {
  const { escape } = host;
  await loadCustom();
  const card = (theme: Backdrop) => `<div class="theme-card${theme.id === current.id ? ' selected' : ''}" data-theme-id="${escape(theme.id)}" role="button" tabindex="0" aria-pressed="${theme.id === current.id}"><span class="theme-thumb${theme.mode === 'plain' ? ' plain' : ''}"${theme.thumb ? ` style="background-image:url('${theme.thumb}')"` : ''}></span><span class="theme-name">${escape(theme.name)}<small>${theme.custom && !isRemote ? `<button type="button" class="theme-mode" data-theme-mode="${escape(theme.id)}" title="切换显示方式">${theme.mode === 'figure' ? '立绘' : '铺满'}</button><button type="button" class="theme-remove" data-theme-remove="${escape(theme.id)}" title="删除这张背景" aria-label="删除这张背景">×</button>` : theme.custom ? '自定义' : theme.mode === 'plain' ? '纯色' : '内置'}</small></span></div>`;
  const grid = () => `${all().map(card).join('')}${isRemote ? '' : '<label class="theme-card theme-import" title="导入你自己的图片（可多选），只保存在这台电脑上"><input type="file" accept="image/*" multiple hidden data-theme-import><span class="theme-thumb plus">＋</span><span class="theme-name">导入图片<small>可多选</small></span></label>'}`;
  host.open('主题背景', '点一下立即生效。导入的图片只保存在这台电脑上，竖图默认当立绘、横图铺满，可以在卡片上切换。', `<div class="theme-picker"><div class="theme-grid">${grid()}</div><div class="theme-veil"><span>背景浓淡</span><div class="segmented" role="radiogroup" aria-label="背景浓淡">${(Object.keys(VEILS) as Veil[]).map(id => `<button type="button" role="radio" data-veil="${id}" aria-checked="${id === current.veil}" class="${id === current.veil ? 'selected' : ''}">${VEILS[id].name}</button>`).join('')}</div></div><div class="modal-footer"><span>内置背景不含任何人物，适合所有人</span><button type="button" class="primary" data-theme-done>完成</button></div></div>`);
  const root = host.modal.querySelector<HTMLElement>('.theme-picker')!;
  const repaint = () => { root.querySelector('.theme-grid')!.innerHTML = grid(); };
  root.addEventListener('change', async event => {
    const input = (event.target as HTMLElement).closest<HTMLInputElement>('[data-theme-import]');
    if (!input?.files?.length) return;
    const files = [...input.files];
    let last = '';
    for (const file of files) {
      try {
        const { image, thumb, mode } = await compress(file);
        last = await invoke<string>('add_background', { name: file.name.replace(/\.[^.]+$/, ''), mode, image, thumb });
      } catch (error) { host.toast(`${file.name}：${String(error)}`, true); }
    }
    await loadCustom(); repaint();
    if (last) { await applyTheme({ ...current, id: `custom:${last}` }); repaint(); host.toast(`已导入 ${files.length} 张图片`); }
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
    const veil = target.closest<HTMLElement>('[data-veil]');
    if (theme && theme.dataset.themeId !== current.id) {
      await applyTheme({ ...current, id: theme.dataset.themeId! });
      repaint();
    }
    if (veil) {
      await applyTheme({ ...current, veil: veil.dataset.veil as Veil }, false);
      root.querySelectorAll<HTMLElement>('[data-veil]').forEach(item => { item.classList.toggle('selected', item === veil); item.setAttribute('aria-checked', String(item === veil)); });
    }
    if (target.closest('[data-theme-done]')) host.modal.close();
  });
  root.addEventListener('keydown', event => {
    const theme = (event.target as HTMLElement).closest<HTMLElement>('[data-theme-id]');
    if (theme && (event.key === 'Enter' || event.key === ' ')) { event.preventDefault(); theme.click(); }
  });
}
