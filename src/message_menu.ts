export interface MessageTarget { conversationId: string; kind: 'message' | 'team_message' | 'workflow' | 'peer_message'; id: string; }

export function installMessageMenu(root: HTMLElement, conversation: () => string, remove: (target: MessageTarget) => Promise<void>) {
  let menu: HTMLElement | null = null;
  const close = () => { menu?.remove(); menu = null; };
  root.addEventListener('contextmenu', event => {
    const origin = event.target instanceof Element ? event.target : null;
    const row = origin?.closest<HTMLElement>('[data-source-workflow], [data-workflow-id], [data-team-message-id], [data-message-id], [data-agent-direct-message-id]');
    if (!row) return;
    event.preventDefault(); close();
    const workflow = row.dataset.sourceWorkflow || row.dataset.workflowId;
    const target: MessageTarget = workflow ? { conversationId: conversation(), kind: 'workflow', id: workflow }
      : row.dataset.teamMessageId ? { conversationId: conversation(), kind: 'team_message', id: row.dataset.teamMessageId }
      : row.dataset.agentDirectMessageId ? { conversationId: conversation(), kind: 'peer_message', id: row.dataset.agentDirectMessageId }
      : { conversationId: row.closest<HTMLElement>('[data-project-chat-id]')?.dataset.projectChatId || conversation(), kind: 'message', id: row.dataset.messageId! };
    menu = document.createElement('div');
    menu.className = 'message-context-menu'; menu.setAttribute('role', 'menu');
    menu.innerHTML = `<button type="button" role="menuitem" data-message-copy>复制</button><button type="button" role="menuitem" data-message-delete>${workflow ? '删除本轮协作' : '删除消息'}</button><small>${workflow ? '消息、方案和执行记录一并删除' : '删除本机的这条消息'}</small>`;
    document.body.append(menu);
    menu.style.left = `${Math.min(event.clientX, innerWidth - menu.offsetWidth - 12)}px`;
    menu.style.top = `${Math.min(event.clientY, innerHeight - menu.offsetHeight - 12)}px`;
    menu.querySelector('[data-message-copy]')!.addEventListener('click', () => {
      const text = getSelection()?.toString() || row.querySelector<HTMLElement>('.agent-turn-text,.bubble,.workflow-result')?.innerText || row.innerText;
      close(); void navigator.clipboard.writeText(text).catch(() => {});
    });
    const button = menu.querySelector<HTMLButtonElement>('[data-message-delete]')!;
    button.focus();
    button.addEventListener('click', () => { close(); void remove(target); });
  });
  document.addEventListener('pointerdown', event => { if (!menu?.contains(event.target as Node)) close(); });
  document.addEventListener('keydown', event => { if (event.key === 'Escape') close(); });
  window.addEventListener('blur', close);
}
