import type { ConversationDetail, RunToolEvent, RuntimeSnapshot, Workflow } from './types';

/** Display data only: merging logs never changes the separate native sessions. */
export interface AgentEntry {
  id: string;
  at: number;
  kind: 'user' | 'agent' | 'peer';
  source: 'private' | 'project' | 'peer';
  author: string;
  body: string;
  thought: string;
  tools: RunToolEvent[];
  status: string;
  error: string | null;
  model?: string | null;
  effort?: string | null;
  messageId?: string;
  attemptId?: string;
  workflowId?: string;
}

export const entryBusy = (status: string) => ['starting', 'running', 'cancelling', 'streaming'].includes(status);

export function agentTimeline(agent: string, chat: ConversationDetail, jobs: Workflow[], runtime: RuntimeSnapshot): AgentEntry[] {
  const rows = new Map<string, AgentEntry>();
  for (const message of chat.messages) {
    rows.set(`message:${message.id}`, {
      id: `message:${message.id}`, at: message.created_at, kind: message.sender_id === 'user' ? 'user' : 'agent',
      source: 'private', author: message.sender_id, body: message.content, thought: message.thought || '',
      tools: message.tool_events || [], status: message.status, error: null, messageId: message.id,
    });
  }
  const active = runtime.active;
  if (active?.conversation_id === chat.conversation.id) {
    const key = `message:${active.assistant_message_id}`;
    const saved = rows.get(key);
    rows.set(key, {
      id: key, at: saved?.at || chat.messages.find(message => message.id === active.user_message_id)?.created_at || chat.conversation.updated_at,
      kind: 'agent', source: 'private', author: agent, body: active.text, thought: active.thought || '',
      tools: active.tool_events || [], status: active.status, error: active.error, model: active.model, effort: active.reasoning_effort,
      messageId: active.assistant_message_id,
    });
  }
  for (const job of jobs) {
    const attempts = job.attempts.filter(attempt => attempt.agent_id === agent && attempt.stage !== 'verify');
    if (attempts.length) rows.set(`request:${job.id}`, {
      id: `request:${job.id}`, at: job.created_at, kind: 'user', source: 'project', author: 'user', body: job.request,
      thought: '', tools: [], status: 'delivered', error: null,
    });
    attempts.forEach((attempt, index) => rows.set(`attempt:${attempt.id}`, {
      id: `attempt:${attempt.id}`, at: attempt.created_at ?? job.created_at + index + 1,
      kind: 'agent', source: 'project', author: agent, body: attempt.output, thought: attempt.thought,
      tools: attempt.tool_events || [], status: attempt.status, error: attempt.error, model: attempt.model,
      effort: attempt.reasoning_effort, attemptId: attempt.id, workflowId: job.id,
    }));
    for (const message of job.agent_messages || []) {
      if (message.sender_agent !== agent && message.recipient_agent !== agent) continue;
      rows.set(`peer:${message.id}`, { id: `peer:${message.id}`, at: message.created_at, kind: 'peer', source: 'peer',
        author: message.sender_agent, body: message.content, thought: '', tools: [], status: 'completed', error: null });
    }
  }
  for (const message of chat.project_agent_inbox || []) rows.set(`peer:${message.id}`, {
    id: `peer:${message.id}`, at: message.created_at, kind: 'peer', source: 'peer', author: message.sender_agent,
    body: message.content, thought: '', tools: [], status: 'completed', error: null,
  });
  return [...rows.values()].sort((a, b) => a.at - b.at || Number(b.kind === 'user') - Number(a.kind === 'user') || a.id.localeCompare(b.id));
}
