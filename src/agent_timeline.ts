import { steerIds } from './thought_flow';
import type { ConversationDetail, RunToolEvent, RuntimeSnapshot } from './types';

/** Display data only: merging logs never changes the separate native sessions. */
export interface AgentEntry {
  id: string;
  at: number;
  kind: 'user' | 'agent';
  /** 'user' 是你；送进来的消息也可能是主脑派活（主脑席位）、成员交回（成员席位）、同席通知（hub）。 */
  author: string;
  body: string;
  thought: string;
  tools: RunToolEvent[];
  status: string;
  error: string | null;
  model?: string | null;
  effort?: string | null;
  messageId?: string;
  attachmentRoom?: string;
  protocol?: { label: string; raw: string };
  /** 回复中途插进来的话（按消息 id），显示在回复里被打断的位置。 */
  steers?: Record<string, { body: string; at: number; author: string }>;
}

export const entryBusy = (status: string) => ['starting', 'running', 'cancelling', 'streaming'].includes(status);

/** 两份条目都已落定（不在流式）且内容一样：画过的节点不用再 patch。字符串先比指针，相同的条目几乎是 O(1)。 */
export function settledSame(a: AgentEntry, b: AgentEntry): boolean {
  if (entryBusy(a.status) || entryBusy(b.status)) return false;
  for (const key of ['id', 'at', 'kind', 'author', 'body', 'thought', 'status', 'error', 'model', 'effort', 'messageId', 'attachmentRoom'] as const) if (a[key] !== b[key]) return false;
  return a.protocol?.raw === b.protocol?.raw && a.protocol?.label === b.protocol?.label
    && a.tools.length === b.tools.length
    && a.tools.every((tool, index) => { const other = b.tools[index]; return tool === other || (tool.event_id === other.event_id && tool.kind === other.kind && tool.title === other.title && tool.status === other.status && tool.input === other.input && tool.output === other.output); })
    && JSON.stringify(a.steers) === JSON.stringify(b.steers);
}
const toolActivity = (events: RunToolEvent[]) => events.filter(event => !['userMessage','hookPrompt','plan','functionCallOutput'].includes(event.kind));

export function agentTimeline(agent: string, chat: ConversationDetail, runtime: RuntimeSnapshot): AgentEntry[] {
  const rows = new Map<string, AgentEntry>();
  for (const message of chat.messages) {
    rows.set(`message:${message.id}`, {
      id: `message:${message.id}`, at: message.created_at, kind: message.sender_id === agent ? 'agent' : 'user',
      author: message.sender_id, body: message.content, thought: message.thought || '',
      tools: toolActivity(message.tool_events || []), status: message.status, error: null, messageId: message.id, attachmentRoom: chat.conversation.id,
    });
  }
  // 同一成员可能同时在多个会话回复，只取这个会话的运行。
  const own = (runtime.runs?.length ? runtime.runs : runtime.active ? [runtime.active] : []).filter(run => run.conversation_id === chat.conversation.id);
  const active = own.find(run => entryBusy(run.status)) || own[own.length - 1];
  if (active?.conversation_id === chat.conversation.id && (entryBusy(active.status) || rows.has(`message:${active.assistant_message_id}`))) {
    const key = `message:${active.assistant_message_id}`;
    const saved = rows.get(key);
    rows.set(key, {
      id: key, at: saved?.at || chat.messages.find(message => message.id === active.user_message_id)?.created_at || chat.conversation.updated_at,
      kind: 'agent', author: agent, body: active.text, thought: active.thought || '',
      tools: toolActivity(active.tool_events || []), status: active.status, error: active.error, model: active.model, effort: active.reasoning_effort,
      messageId: active.assistant_message_id,
    });
  }
  // 插话跟着被它打断的回复显示，不再按自己的时间排到整条回复后面。
  for (const row of [...rows.values()]) {
    if (row.kind !== 'agent' || row.body.includes('```tongxi-question')) continue;
    for (const id of steerIds(row.thought)) {
      const said = rows.get(`message:${id}`);
      if (said?.kind !== 'user') continue;
      (row.steers ??= {})[id] = { body: said.body, at: said.at, author: said.author };
      rows.delete(said.id);
    }
  }
  return [...rows.values()].sort((a, b) => a.at - b.at || Number(b.kind === 'user') - Number(a.kind === 'user') || a.id.localeCompare(b.id));
}
