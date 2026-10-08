export interface ThoughtScrollState {
  open: boolean;
  follow: boolean;
  top: number;
}

export function captureThoughtScroll(message: HTMLElement | null): ThoughtScrollState | null {
  const details = message?.querySelector<HTMLDetailsElement>('.thought');
  const body = details?.querySelector<HTMLElement>('.thought-body');
  if (!details || !body) return null;
  return {
    open: details.open,
    follow: details.open && body.scrollHeight - body.scrollTop - body.clientHeight <= 24,
    top: body.scrollTop,
  };
}

export function restoreThoughtScroll(message: HTMLElement, state: ThoughtScrollState | null): void {
  const details = message.querySelector<HTMLDetailsElement>('.thought');
  const body = details?.querySelector<HTMLElement>('.thought-body');
  if (!details || !body) return;
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
