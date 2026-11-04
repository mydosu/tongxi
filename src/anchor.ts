/// 列表上方插入内容（往上补旧消息、重画）后让屏上那条留在原处：按它的实际位移补偿。
/// scrollTop 只能落在整设备像素上（125% 缩放是 0.8px 一格），插进来的高度却带小数，每批会剩零点几像素的零头，
/// 屏上的字跟着跳一个像素。零头用列表顶上多出的一点 padding（不到一格）垫回去：padding 不按像素取整。
export function holdAnchor(box: HTMLElement, anchor: Element | null | undefined, before: number) {
  if (!anchor?.isConnected) return;
  const step = 1 / devicePixelRatio, old = Number(box.dataset.anchorNudge) || 0;
  box.scrollTop += anchor.getBoundingClientRect().top - before;
  // 剩下的零头 r：留白加 -r 正好抵掉；留白要保持在 [0, step)，多出或不够的整格交给 scrollTop。
  const want = old - (anchor.getBoundingClientRect().top - before), whole = Math.floor(want / step);
  const nudge = want - whole * step;
  box.style.paddingTop = `${parseFloat(getComputedStyle(box).paddingTop) - old + nudge}px`;
  box.dataset.anchorNudge = String(nudge);
  box.scrollTop -= whole * step;
}
