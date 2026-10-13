export interface ThoughtScrollState {
  open: boolean;
  follow: boolean;
  top: number;
  /** 穿插显示时消息里的思考段数；段数变了说明进入了新一段，旧状态不再套用。 */
  count: number;
  /** 各段思考是否展开（用户手动展开的早先段落在重绘后保持展开）。 */
  opened: boolean[];
  /** 最后一段是否仍在思考；从“想”转到“说”时它回到默认收起。 */
  live: boolean;
}

/** 正在增长的总是最后一段思考。 */
function lastThought(message: HTMLElement | null) {
  const all = message?.querySelectorAll<HTMLDetailsElement>('.thought');
  return { details: all?.[all.length - 1], count: all?.length || 0 };
}

export function captureThoughtScroll(message: HTMLElement | null): ThoughtScrollState | null {
  const { details, count } = lastThought(message);
  const body = details?.querySelector<HTMLElement>('.thought-body');
  if (!details || !body) return null;
  return {
    open: details.open,
    follow: details.open && body.scrollHeight - body.scrollTop - body.clientHeight <= 24,
    top: body.scrollTop,
    count,
    opened: [...message!.querySelectorAll<HTMLDetailsElement>('.thought')].map(item => item.open),
    live: details.dataset.live === 'true',
  };
}

export function restoreThoughtScroll(message: HTMLElement, previous: ThoughtScrollState | null): void {
  const { details, count } = lastThought(message);
  const body = details?.querySelector<HTMLElement>('.thought-body');
  if (!details || !body) return;
  const state = previous?.count === count && previous.live === (details.dataset.live === 'true') ? previous : null;
  // 早先已经结束的段落沿用展开状态；刚结束的那段（之前的最后一段）回到默认收起。
  const all = message.querySelectorAll<HTMLDetailsElement>('.thought');
  previous?.opened.slice(0, previous.count - 1).forEach((open, index) => { if (all[index]) all[index].open = open; });
  if (state) details.open = state.open;
  if (!details.open) return;

  const restore = () => {
    if (!body.isConnected) return;
    const max = Math.max(0, body.scrollHeight - body.clientHeight);
    body.scrollTop = state?.follow ?? true ? max : Math.min(state?.top || 0, max);
  };
  restore();
  requestAnimationFrame(() => {
    restore();
    requestAnimationFrame(restore);
  });
}
