export interface MessageTarget { conversationId: string; kind: 'message' | 'team_message'; id: string; }

/// 非安全上下文（手机经 http 打开）没有 navigator.clipboard：退回 execCommand，再不行就算了，不抛错。
function copyText(text: string) {
  if (navigator.clipboard) { void navigator.clipboard.writeText(text).catch(() => {}); return; }
  const area = document.createElement('textarea');
  area.value = text; area.style.cssText = 'position:fixed;opacity:0';
  document.body.append(area); area.select();
  try { document.execCommand('copy'); } catch { /* 复制不了就算了 */ }
  area.remove();
}

export function installMessageMenu(root: HTMLElement, conversation: () => string, remove: (target: MessageTarget) => Promise<void>) {
  let menu: HTMLElement | null = null;
  const close = () => { menu?.remove(); menu = null; };
  root.addEventListener('contextmenu', event => {
    const origin = event.target instanceof Element ? event.target : null;
    const row = origin?.closest<HTMLElement>('[data-team-message-id], [data-message-id]');
    if (!row) return;
    event.preventDefault(); close();
    const target: MessageTarget = row.dataset.teamMessageId ? { conversationId: conversation(), kind: 'team_message', id: row.dataset.teamMessageId }
      : { conversationId: row.closest<HTMLElement>('[data-project-chat-id]')?.dataset.projectChatId || conversation(), kind: 'message', id: row.dataset.messageId! };
    menu = document.createElement('div');
    menu.className = 'message-context-menu'; menu.setAttribute('role', 'menu');
    menu.innerHTML = `<button type="button" role="menuitem" data-message-copy>复制</button><button type="button" role="menuitem" data-message-delete>删除消息</button><small>删除本机的这条消息</small>`;
    document.body.append(menu);
    menu.style.left = `${Math.min(event.clientX, innerWidth - menu.offsetWidth - 12)}px`;
    menu.style.top = `${Math.min(event.clientY, innerHeight - menu.offsetHeight - 12)}px`;
    menu.querySelector('[data-message-copy]')!.addEventListener('click', () => {
      const text = getSelection()?.toString() || row.querySelector<HTMLElement>('.agent-turn-text,.bubble')?.innerText || row.innerText;
      close(); copyText(text);
    });
    const button = menu.querySelector<HTMLButtonElement>('[data-message-delete]')!;
    button.focus();
    button.addEventListener('click', () => { close(); void remove(target); });
  });
  document.addEventListener('pointerdown', event => { if (!menu?.contains(event.target as Node)) close(); });
  document.addEventListener('keydown', event => { if (event.key === 'Escape') close(); });
  window.addEventListener('blur', close);
}
