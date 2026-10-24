export interface Agent {
  id: string; name: string; subtitle: string; role: string;
  location: string; accent: string; status: 'not_connected';
  /** 陪伴头像（用户上传的 data URL）。 */
  avatar?: string;
}
export interface Conversation {
  id: string; title: string; kind: 'direct' | 'group'; archived: boolean;
  created_at: number; updated_at: number; members: string[];
  message_count: number; preview: string;
}
export interface Message {
  id: string; conversation_id: string; sender_id: string; content: string;
  /** 后端推理块；没有推理或老数据时是空串。 */
  thought?: string;
  tool_events?: RunToolEvent[];
  status: 'local_only' | 'pending' | 'delivered' | 'streaming' | 'completed' | 'interrupted' | 'failed'; created_at: number;
}
export interface RunToolEvent { event_id: string; kind: string; title: string; input: string | null; output: string | null; status: string; created_at: number; }
export interface ConversationDetail {
  conversation: Conversation; messages: Message[];
  discussions: Discussion[];
  project: Project | null;
  project_coordinator?: RoleChoice | null;
  project_grants?: string;
  project_agent_chats: ProjectAgentChat[];
  project_team_messages: ProjectTeamMessage[];
  chat_questions?: ChatQuestion[];
  sessions: { agent_id: string; session_key: string; native_session_id: string | null; model: string | null; reasoning_effort: string | null }[];
}
export interface ProjectAgentChat { group_conversation_id: string; agent_id: string; conversation_id: string; label?: string | null; /** 后端算好的显示名：起过名就是它，否则成员名（多个会话时带编号）。 */ name?: string; }
export interface ProjectTeamMessage { id: string; project_id: string; conversation_id: string; workflow_id: string | null; attempt_id: string | null; source_message_id: string | null; sender_agent: string | null; kind: 'agent' | 'system' | 'user'; content: string; created_at: number; }
export interface AppInfo {
  version: string; milestone: number; database_path: string; hud_shortcut: boolean; autoconnect?: boolean;
}

export interface RunRecord {
  discussion_id: string | null; round: number | null;
  agent_id: string;
  model: string | null; reasoning_effort: string | null;
  id: string; conversation_id: string; user_message_id: string; assistant_message_id: string;
  status: 'starting' | 'running' | 'cancelling' | 'completed' | 'interrupted' | 'failed';
  native_thread_id: string | null; native_turn_id: string | null; error: string | null;
}
export interface RuntimeSnapshot {
  models: ModelOption[]; default_model: string | null; default_effort: string | null;
  revision: number;
  connection: 'disconnected' | 'connecting' | 'connected'; executable: string | null;
  version: string | null; error: string | null; active: (RunRecord & { text: string; thought?: string; tool_events?: RunToolEvent[] }) | null;
  /** 各会话的运行（同一成员可同时在多个会话回复）。 */
  runs?: (RunRecord & { text: string; thought?: string; tool_events?: RunToolEvent[] })[];
}
export interface NativeSession {
  id: string; title?: string | null; cwd?: string | null; updated_at?: string | null;
  occupied_by?: string | null; current?: boolean;
}
export interface ModelOption { id: string; name: string; provider_id?: string | null; provider_name?: string | null; efforts: string[]; default_effort: string | null; }

export interface Discussion {
  id: string; conversation_id: string; user_message_id: string; participants: string[]; rounds: number;
  status: 'running' | 'cancelling' | 'completed' | 'interrupted' | 'failed'; error: string | null;
  created_at: number; updated_at: number; turns: RunRecord[];
}

export interface Project { id: string; name: string; root: string; summary_enabled: boolean; created_at: number; }
export type RoleChoice = { agent: string; model: string | null; effort: string | null };
/** 协作问答：主脑挂在项目问答面板上的问题。 */
/** 主脑派出去的一件活。status：pending 排队、running 在做、done 做完、failed 失败或被停下（run_status 为 interrupted）。worktree 为空而 branch 还在，表示工作树已合并或丢弃。 */
export interface BrainJob { id: string; seat: string; name: string; chat: string; content: string; model: string | null; effort: string | null; worktree: string | null; branch: string | null; status: 'pending' | 'running' | 'done' | 'failed'; error: string | null; run_status: string | null; result: string | null; created_at: number; finished_at: number | null; usage: JobUsage | null; }
/// runs.usage：cost 是按 API 价折的美元（没收录的型号为 null），brain_cost 是同样用量按主脑模型折的。
export interface JobUsage { usage: { inputTokens?: number; cachedReadTokens?: number; cachedWriteTokens?: number; outputTokens?: number }; cost: number | null; brain_cost: number | null; }
export interface UsageSum { turns: number; input: number; cache_read: number; cache_write: number; output: number; cost: number | null; }
/** 派活页：主脑席位和会话、还没送进主脑的消息数、全部派活（最新的在前）。 */
export interface PlanDoc { name: string; brief: boolean; text: string; modified: number; }
export interface PlanStep { id: number; title: string; status: 'todo' | 'doing' | 'review' | 'waiting' | 'done'; owner: string | null; name: string | null; depends: number[]; note: string | null; wait_key: string | null; job: string | null; }
export interface BrainOverview { seat: string; chat: string | null; waiting: number; jobs: BrainJob[]; plans: PlanDoc[]; steps: PlanStep[]; usage: { brain_today: UsageSum; brain_total: UsageSum; context: number | null }; ledger: LedgerRow[]; }
/// 项目总账一格：某天、某席位、某模型的回合合计；priced 少于 turns 说明有回合价格未知。
export interface LedgerRow { day: string; name: string; model: string; turns: number; input: number; output: number; cost: number; priced: number; }
export interface Inquiry { id: string; conversation_id: string; workflow_id: string | null; key: string; asked_by: string; question: string; background: string; options: string[]; default_choice: string | null; reversible: boolean; status: 'open' | 'answered' | 'withdrawn'; answer: string | null; seen: boolean; created_at: number; answered_at: number | null; }

export interface ServiceInfo {
  id: string; name: string; connection: 'disconnected' | 'connecting' | 'connected';
  executable: string | null; runtime_version: string | null; installed_version: string | null;
  version_source: string; busy: boolean; update_state: string;
}
export interface ServiceCheckResult {
  service_id: string; checked_at: string; runtime_version: string | null;
  installed_version: string | null; version_source: string; note: string;
  latest_version: string | null; update_available: boolean | null; check_source: string;
}
export interface ServicePlanResult {
  service_id: string; supported: boolean; reason: string; created_at: string;
  plan_id: string | null; candidate_version: string | null;
}
export interface ServiceUpdateStatus {
  service_id: string; managed: boolean;
  active_version: string | null; previous_version: string | null;
  ready_plan_id: string | null; candidate_version: string | null;
  rollback_available: boolean;
}

export interface ChatQuestion { id:string;conversation_id:string;run_id:string;message_id:string;agent_id:string;prompt:string;options:string[];status:string;mode:string;answer:string|null;delivery:string;error:string|null; }
export interface PendingChatQuestion { kind?:string;id:string;conversation_id:string;agent_id:string;group_id:string|null; }
