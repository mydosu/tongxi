// 系统原生 title 提示是黑字方框，跟玻璃主题不搭：悬停时把 title 暂收走，换成自绘的提示框，离开再还回去。
// 全局委托，动态设的 title（最大化/还原、展开/收起）也一并接管，不用逐个按钮改。
const tip = document.createElement('div');
tip.className = 'hover-tip';
tip.setAttribute('role', 'tooltip');
let owner: HTMLElement | null = null;
let timer = 0;

function hide() {
  clearTimeout(timer);
  tip.classList.remove('visible');
  if (owner && !owner.hasAttribute('title') && owner.dataset.tip) owner.setAttribute('title', owner.dataset.tip);
  owner = null;
}

function place() {
  if (!owner) return;
  const r = owner.getBoundingClientRect(), t = tip.getBoundingClientRect(), gap = 8;
  let top = r.bottom + gap;
  if (top + t.height > innerHeight - 4) top = r.top - t.height - gap;
  const left = Math.min(Math.max(4, r.left + r.width / 2 - t.width / 2), innerWidth - t.width - 4);
  tip.style.transform = `translate(${Math.round(left)}px, ${Math.round(Math.max(4, top))}px)`;
}

document.addEventListener('pointerover', event => {
  const target = event.target as Element | null;
  if (owner?.contains(target)) return;
  const el = target?.closest?.<HTMLElement>('[title]');
  hide();
  if (!el) return;
  const text = el.getAttribute('title')?.trim();
  if (!text) return;
  owner = el; el.dataset.tip = text; el.removeAttribute('title');
  // 打开的 <dialog> 在顶层，提示框得挂进去才不会被盖住。
  (el.closest('dialog[open]') || document.body).append(tip);
  tip.textContent = text;
  timer = window.setTimeout(() => { place(); tip.classList.add('visible'); }, 350);
});
document.addEventListener('pointerout', event => {
  if (owner && !owner.contains(event.relatedTarget as Node | null)) hide();
});
// 点下只收起提示，title 仍暂扣着，免得原生提示接着冒出来。
document.addEventListener('pointerdown', () => { clearTimeout(timer); tip.classList.remove('visible'); }, true);
addEventListener('blur', hide);
addEventListener('scroll', hide, true);
