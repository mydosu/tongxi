const FOLLOW_THRESHOLD = 28;
const bindings = new WeakMap<HTMLElement, () => void>();

export function bindProjectThoughtFollow(card: HTMLElement): void {
  bindings.get(card)?.();
  const cleanup: (() => void)[] = [];
  for (const thought of card.querySelectorAll<HTMLDetailsElement>('.attempt-thought[data-active="true"]')) {
    const viewport = thought.querySelector<HTMLElement>('.attempt-thought-output');
    const content = thought.querySelector<HTMLElement>('.attempt-thought-content');
    if (!viewport || !content) continue;

    let following = true;
    let lastHeight = -1;
    const trackScroll = () => {
      following = viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight < FOLLOW_THRESHOLD;
    };
    const resize = (entries: ResizeObserverEntry[]) => {
      const height = entries.at(-1)?.borderBoxSize?.[0]?.blockSize ?? content.offsetHeight;
      const grew = height > lastHeight;
      lastHeight = height;
      if (grew && following) viewport.scrollTop = viewport.scrollHeight;
    };

    const observer = new ResizeObserver(resize);
    observer.observe(content);
    viewport.addEventListener('scroll', trackScroll, { passive: true });
    cleanup.push(() => {
      observer.disconnect();
      viewport.removeEventListener('scroll', trackScroll);
    });
  }
  bindings.set(card, () => cleanup.forEach(dispose => dispose()));
}

export function unbindProjectThoughtFollow(card: HTMLElement): void {
  bindings.get(card)?.();
  bindings.delete(card);
}
