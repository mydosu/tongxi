import type { RunToolEvent } from './types';
type Escape = (value: string | number) => string;

/** A quiet latest-activity line; history is revealed only by the reader. */
export function toolActivity(events: RunToolEvent[], escape: Escape) {
  const last = events.at(-1);
  if (!last) return '';
  const status = (value: string) => value === 'running' ? '进行中' : value === 'failed' ? '失败' : '完成';
  return `<details class="tool-activity message-tool-trace agent-tools-summary" data-tool-key="all"><summary><span class="tool-chevron">›</span><span class="tool-latest">${escape(last.title || last.kind)}</span><small>${status(last.status)} · ${events.length} 项</small></summary><div class="tool-history">${events.map(event => `<details class="message-tool-event agent-tool-detail" data-tool-key="${escape(event.event_id)}"><summary><span>${escape(event.title || event.kind)}</span><small>${status(event.status)}</small></summary>${event.input ? `<pre>${escape(event.input)}</pre>` : ''}${event.output ? `<pre>${escape(event.output)}</pre>` : ''}</details>`).join('')}</div></details>`;
}

export function captureToolState(root: Element | null) {
  return [...(root?.querySelectorAll<HTMLDetailsElement>('[data-tool-key]') || [])].map(node => ({ key: node.dataset.toolKey!, open: node.open, scroll: node.querySelector('.tool-history')?.scrollTop || 0 }));
}
export function restoreToolState(root: Element, state: ReturnType<typeof captureToolState>) {
  for (const saved of state) {
    const node = [...root.querySelectorAll<HTMLDetailsElement>('[data-tool-key]')].find(node => node.dataset.toolKey === saved.key);
    if (node) { node.open = saved.open; const history = node.querySelector('.tool-history'); if (history) history.scrollTop = saved.scroll; }
  }
}
