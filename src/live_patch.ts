import { thoughtFollowing } from './thought_scroll';

/** 流式重画时就地更新一条消息：思考框留着原节点（展开状态、滚动位置、正在进行的滚轮动画都不被打断），
 *  其他块内容变了才替换。最外层结构对不上（比如多了插话、变成整轮）返回 false，由调用方整条替换。 */
export function patchMessage(old: HTMLElement, html: string): boolean {
  const template = document.createElement('template');
  template.innerHTML = html.trim();
  const next = template.content.firstElementChild as HTMLElement | null;
  if (!next || template.content.childElementCount !== 1 || next.tagName !== old.tagName || next.className !== old.className) return false;
  for (const { name, value } of [...next.attributes]) if (old.getAttribute(name) !== value) old.setAttribute(name, value);
  const fresh = [...next.children];
  fresh.forEach((child, index) => {
    const current = old.children[index];
    if (!current) old.append(child);
    else if (current instanceof HTMLDetailsElement && child instanceof HTMLDetailsElement && current.classList.contains('thought') && child.classList.contains('thought')) patchThought(current, child);
    else if (!current.isEqualNode(child)) current.replaceWith(child);
  });
  while (old.children.length > fresh.length) old.lastElementChild!.remove();
  return true;
}

function patchThought(current: HTMLDetailsElement, next: HTMLDetailsElement) {
  // 从「想」转到「说」：回到默认收起（用户亲手展开过的由调用方再恢复）。
  if (current.dataset.live !== next.dataset.live) { current.dataset.live = next.dataset.live || 'false'; current.open = next.open; }
  const summary = current.querySelector('summary');
  const label = next.querySelector('summary')?.textContent ?? '';
  if (summary && summary.textContent !== label) summary.textContent = label;
  const body = current.querySelector<HTMLElement>('.thought-body');
  const text = next.querySelector('.thought-body')?.textContent ?? '';
  if (!body || body.textContent === text) return;
  const follow = current.open && thoughtFollowing(body) && body.scrollHeight - body.scrollTop - body.clientHeight <= 24;
  body.textContent = text;
  if (follow) body.scrollTop = body.scrollHeight;
}

/** 把 next 的子节点就地合进 parent：一样的节点原样留着（图片不重载、动画不重播、悬停不丢），只改变了的属性、文字，增删多出少掉的节点。
 *  keep 匹配的子元素（比如滑动的高亮块）不参与比较、原样保留。新出现的节点不带监听，调用方要用事件委托。 */
export function morphChildren(parent: Element, next: ParentNode, keep = '') {
  const current = [...parent.childNodes].filter(node => !(keep && node instanceof Element && node.matches(keep)));
  const fresh = [...next.childNodes];
  const anchor = keep ? parent.querySelector(`:scope > ${keep}`) : null;
  fresh.forEach((node, index) => {
    const have = current[index];
    if (!have) parent.insertBefore(node, anchor);
    else morph(have, node);
  });
  current.slice(fresh.length).forEach(node => node.remove());
}

function morph(old: ChildNode, next: ChildNode) {
  if (old.isEqualNode(next)) return;
  if (old.nodeType !== next.nodeType || old.nodeName !== next.nodeName) { old.replaceWith(next); return; }
  if (!(old instanceof Element)) { old.nodeValue = next.nodeValue; return; }
  const to = next as Element;
  for (const { name } of [...old.attributes]) if (!to.hasAttribute(name)) old.removeAttribute(name);
  for (const { name, value } of [...to.attributes]) if (old.getAttribute(name) !== value) old.setAttribute(name, value);
  morphChildren(old, to);
}
