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
  project_roles: Roles | null;
  project_coordinator?: RoleChoice | null;
  /** 调度倾向：save 省额度 / balanced 平衡 / quality 质量优先。 */
  project_preference?: string;
  project_grants?: string;
  workflows: Workflow[];
  project_agent_chats: ProjectAgentChat[];
  project_team_messages: ProjectTeamMessage[];
  project_agent_inbox?: ProjectRoomAgentMessage[];
  project_questions?: ProjectQuestion[];
  chat_questions?: ChatQuestion[];
  sessions: { agent_id: string; session_key: string; native_session_id: string | null; model: string | null; reasoning_effort: string | null }[];
}
export interface ProjectAgentChat { group_conversation_id: string; agent_id: string; conversation_id: string; label?: string | null; /** 后端算好的显示名：起过名就是它，否则成员名（多个会话时带编号）。 */ name?: string; /** 调度为协作新开的会话；和用户建的一样留在项目里。 */ opened_by_coordinator?: boolean; }
export interface ProjectRoomAgentMessage { id: string; sender_agent: string; recipient_agent: string; content: string; created_at: number; }
export interface ProjectTeamMessage { id: string; project_id: string; conversation_id: string; workflow_id: string | null; attempt_id: string | null; source_message_id: string | null; sender_agent: string | null; kind: 'agent' | 'system' | 'user'; content: string; created_at: number; }
export interface AppInfo {
  version: string; milestone: number; database_path: string; hud_shortcut: boolean; autoconnect?: boolean; resume_workflow?: string | null;
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
export interface ExecutionChoice { model: string; reasoning_effort: string | null; rationale: string; }
export type RoleChoice = { agent: string; model: string | null; effort: string | null };
export type Roles = { plan: RoleChoice; implement: RoleChoice; review: RoleChoice };
export type TaskChoice = { position: number; agent_id: string; model: string | null; effort: string | null };
export interface PlannedTask { title: string; agent_id: string; instructions: string; files: string[]; depends_on: number[]; execution?: ExecutionChoice | null; }
export interface ProjectTask extends PlannedTask { id: string; workflow_id: string; position: number; status: string; output: string; error: string | null; model?: string | null; effort?: string | null; worktree?: string | null; branch?: string | null; }
export interface ProjectAttempt { id: string; created_at?: number; activity?: string | null; workflow_id: string; task_id: string | null; agent_id: string; stage: string; status: string; native_thread_id: string | null; native_turn_id: string | null; model: string | null; reasoning_effort: string | null; thought: string; tool_events?: RunToolEvent[]; output: string; error: string | null; usage?: TokenUsage | null; }
/** 一次任务的 token 用量：input 含缓存读写，output 含推理；context_only 表示只知道上下文占用，实际消耗未知。cost_kind：api_estimate 为 API 等值估算，provider 为服务商报的额度消耗。 */
/** 历史表现画像的一行：成员+模型+强度+类别；比率是平滑后的估计，空值表示没有可用数据。 */
export interface CapabilityRow { agent: string; model: string; effort: string; category: string; samples: number; success_rate: number; first_pass_rate: number | null; avg_attempt_tokens: number | null; avg_task_tokens: number | null; avg_repairs: number | null; confidence: 'low' | 'medium' | 'high'; }
export interface TokenUsage { input: number; cached: number; cache_write?: number; output: number; reasoning: number; subagent?: number; cost_usd?: number; cost_kind?: 'api_estimate' | 'provider'; context_only?: boolean; context?: number; }
export interface WorkflowAgentMessage { id: string; workflow_id: string; attempt_id: string; sender_agent: string; recipient_agent: string; content: string; created_at: number; }
export interface Workflow { id: string; project_id: string; conversation_id: string; user_message_id: string; request: string; status: string; plan: { summary: string; tasks: PlannedTask[] } | null; roles?: Roles | string | null; coordination?: {coordinator:RoleChoice;status:string;ledger?:string|null;routes:{task_id:string;purpose:string;source_id:string;quote:string;context_ids:string[];instructions?:string|null;reason?:string|null;allow_subagents?:boolean}[]}; summary: string; error: string | null; created_at: number; updated_at: number; tasks: ProjectTask[]; attempts: ProjectAttempt[]; changes: { attempt_id: string; path: string; operation: string; before_hash: string | null; after_hash: string | null }[]; agent_messages?: WorkflowAgentMessage[]; team_messages?: ProjectTeamMessage[]; }

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

export interface ProjectQuestion { attempt_id?:string|null;id:string;workflow_id:string;conversation_id:string;agent_id:string;prompt:string;options:string[];status:string;answer:string|null; }
export interface ChatQuestion { id:string;conversation_id:string;run_id:string;message_id:string;agent_id:string;prompt:string;options:string[];status:string;mode:string;answer:string|null;delivery:string;error:string|null; }
export interface PendingChatQuestion { kind?:string;id:string;conversation_id:string;agent_id:string;group_id:string|null; }
