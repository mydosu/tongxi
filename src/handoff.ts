/// 成员交回给主脑的消息（后端 project_brain.rs 拼的固定格式）拆成卡片头和正文：
/// 第一行「【成员交回】名字（席位）· 任务 id · 状态」，可选第二行「工作树 路径，分支 名字（比主线多 N 个提交…）」。
export type Handoff = { task: string; state: string; tone: 'ok' | 'warn' | 'err'; branch?: string; branchNote?: string; worktree?: string; body: string };

export function parseHandoff(text: string): Handoff | null {
  const lines = text.split('\n');
  const head = /^【成员交回】.+?（.+?）\s*·\s*任务\s+(\S+)\s*·\s*(.+)$/.exec(lines[0]?.trim() || '');
  if (!head) return null;
  const [, task, state] = head;
  const tone = state.startsWith('失败') ? 'err' : state.includes('停') ? 'warn' : 'ok';
  let rest = lines.slice(1);
  const tree = /^工作树\s+(.+?)，分支\s+([^（\s]+)(?:（(.+)）)?\s*$/.exec(rest[0]?.trim() || '');
  if (tree) rest = rest.slice(1);
  return { task, state, tone, worktree: tree?.[1], branch: tree?.[2], branchNote: tree?.[3], body: rest.join('\n').trim() };
}

export function handoffHtml(handoff: Handoff, body: string, escape: (value: string) => string) {
  const branch = handoff.branch ? `<span class="handoff-branch" title="${escape(handoff.worktree || '')}">${escape(handoff.branch)}${handoff.branchNote ? ` · ${escape(handoff.branchNote)}` : ''}</span>` : '';
  return `<div class="handoff-head"><span class="handoff-state" data-tone="${handoff.tone}">${escape(handoff.state)}</span><span class="handoff-task">任务 ${escape(handoff.task)}</span>${branch}</div><div class="handoff-body">${body}</div>`;
}
