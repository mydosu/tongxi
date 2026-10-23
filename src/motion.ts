/// 动效：换页时旧内容化雾淡出、新内容从雾里清晰；侧栏选中的底色滑到新位置；新到的消息浮上来。
/// 都只在主窗口里做（HUD 是透明浮窗），系统开了「减少动态效果」就全部跳过。

export const still = () => document.documentElement.dataset.hud === '1' || matchMedia('(prefers-reduced-motion: reduce)').matches;
const EASE = 'cubic-bezier(.2, .8, .2, 1)';

/// 换页前调用：把 main 里的旧内容整块挪进一层盖在原位的「雾」，它自己淡出后移除。
/// 雾层挂在 body 上而不是 main 里：新页面紧接着的几次重绘（innerHTML）冲不掉它。
export function fogOut(main: HTMLElement) {
  document.querySelectorAll('.view-ghost').forEach(ghost => ghost.remove());
  if (still() || !main.firstChild) return;
  const box = main.getBoundingClientRect();
  // 节点挪走会丢滚动位置，先记下来，挪完再还回去。
  const scrolled = [...main.querySelectorAll<HTMLElement>('*')].filter(node => node.scrollTop > 0).map(node => [node, node.scrollTop] as const);
  const ghost = document.createElement('div');
  ghost.className = 'view-ghost';
  ghost.inert = true;
  ghost.setAttribute('aria-hidden', 'true');
  Object.assign(ghost.style, { left: `${box.left + main.clientLeft}px`, top: `${box.top + main.clientTop}px`, width: `${main.clientWidth}px`, height: `${main.clientHeight}px` });
  ghost.append(...main.childNodes);
  document.body.append(ghost);
  for (const [node, top] of scrolled) node.scrollTop = top;
  // 里面的旧节点自己的动画结束也会冒泡上来，只认雾层自己的。
  ghost.addEventListener('animationend', event => { if (event.target === ghost) ghost.remove(); });
}

/// 侧栏选中项：两层各一块「滑块」——会话卡片一层、项目子页一层。重绘前 measure()，重绘后 glide()。
const LEVELS = ['.conversation.active', '.project-module-row.active'] as const;
const gliders = new Map<string, { glider: HTMLElement; animation: Animation; target: HTMLElement }>();
type Box = { x: number; y: number; w: number; h: number };

function boxIn(list: HTMLElement, node: Element): Box {
  const outer = list.getBoundingClientRect(), inner = node.getBoundingClientRect();
  return { x: inner.left - outer.left + list.scrollLeft, y: inner.top - outer.top + list.scrollTop, w: inner.width, h: inner.height };
}

/// 记下每层选中项此刻「看上去」在哪：正在滑的就取滑块当前的位置，好从半路接着滑。
export function measure(list: HTMLElement): (Box | null)[] {
  return LEVELS.map(level => {
    const running = gliders.get(level);
    if (running?.glider.isConnected) return boxIn(list, running.glider);
    const node = list.querySelector(level);
    return node ? boxIn(list, node) : null;
  });
}

export function glide(list: HTMLElement, before: (Box | null)[]) {
  LEVELS.forEach((level, index) => {
    const running = gliders.get(level);
    if (running) { gliders.delete(level); running.animation.cancel(); running.glider.remove(); running.target.classList.remove('gliding'); }
    const from = before[index], target = list.querySelector<HTMLElement>(level);
    if (!from || !target || still()) return;
    const to = boxIn(list, target);
    if (Math.abs(from.y - to.y) < 1 && Math.abs(from.x - to.x) < 1 && Math.abs(from.h - to.h) < 1) return;
    const glider = document.createElement('div');
    glider.className = `nav-glider ${index ? 'module' : 'card'}`;
    glider.setAttribute('aria-hidden', 'true');
    Object.assign(glider.style, { left: `${to.x}px`, top: `${to.y}px`, width: `${to.w}px`, height: `${to.h}px` });
    list.append(glider);
    target.classList.add('gliding');
    const animation = glider.animate([
      { transform: `translate(${from.x - to.x}px, ${from.y - to.y}px)`, width: `${from.w}px`, height: `${from.h}px` },
      { transform: 'none', width: `${to.w}px`, height: `${to.h}px` },
    ], { duration: Math.min(460, 260 + Math.abs(from.y - to.y) / 4), easing: EASE });
    gliders.set(level, { glider, animation, target });
    // 被半路接管时 cancel 会让 finished 失败：那边已经收拾过了，这里什么都不做。
    animation.finished.then(() => { if (gliders.get(level)?.glider !== glider) return; gliders.delete(level); glider.remove(); target.classList.remove('gliding'); }, () => {});
  });
}

/// 新到的消息（发出去的、刚开始的回复）浮上来；换到别的会话时已有的那一批不动。
/// 按消息 id 记：同一会话整页重画，旧消息 id 都见过，不会再播一遍。
export function watchArrivals(root: HTMLElement, view: () => string | undefined) {
  let shown: string | undefined, seen = new Set<string>();
  new MutationObserver(records => {
    const fresh = view() !== shown;
    if (fresh) { shown = view(); seen = new Set(); }
    for (const record of records) for (const node of record.addedNodes) {
      if (!(node instanceof HTMLElement)) continue;
      const items = node.matches('.messages > [data-message-id]') ? [node] : node.querySelectorAll<HTMLElement>('.messages > [data-message-id]');
      for (const item of items) {
        const id = item.dataset.messageId!;
        if (seen.has(id)) continue;
        seen.add(id);
        if (!fresh && !still()) item.classList.add('arrive');
      }
    }
  }).observe(root, { childList: true, subtree: true });
}
