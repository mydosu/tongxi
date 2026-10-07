export interface Agent {
  id: string; name: string; subtitle: string; role: string;
  location: string; accent: string; status: 'not_connected';
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
  status: 'local_only' | 'pending' | 'delivered' | 'streaming' | 'completed' | 'interrupted' | 'failed'; created_at: number;
}
export interface ConversationDetail {
  conversation: Conversation; messages: Message[];
  discussions: Discussion[];
  project: Project | null;
  workflows: Workflow[];
  sessions: { agent_id: string; session_key: string; native_session_id: string | null; model: string | null; reasoning_effort: string | null }[];
}
export interface AppInfo {
  version: string; milestone: number; database_path: string; hud_shortcut: boolean;
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
  version: string | null; error: string | null; active: (RunRecord & { text: string; thought?: string }) | null;
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

export interface CheckCommand { name: string; program: string; args: string[]; timeout_seconds: number; }
export interface Project { id: string; name: string; root: string; checks: CheckCommand[]; summary_enabled: boolean; created_at: number; }
export interface ExecutionChoice { model: string; reasoning_effort: string | null; rationale: string; }
export type RoleChoice = { agent: string; model: string | null; effort: string | null };
export type Roles = { plan: RoleChoice; implement: RoleChoice; review: RoleChoice };
export type TaskChoice = { position: number; agent_id: string; model: string | null; effort: string | null };
export interface PlannedTask { title: string; agent_id: string; instructions: string; files: string[]; depends_on: number[]; execution?: ExecutionChoice | null; }
export interface ProjectTask extends PlannedTask { id: string; workflow_id: string; position: number; status: string; output: string; error: string | null; model?: string | null; effort?: string | null; worktree?: string | null; branch?: string | null; }
export interface ProjectCheck extends Omit<CheckCommand, 'timeout_seconds'> { exit_code: number | null; timed_out: boolean; duration_ms: number; output: string; }
export interface ProjectAttempt { id: string; workflow_id: string; task_id: string | null; agent_id: string; stage: string; status: string; native_thread_id: string | null; native_turn_id: string | null; model: string | null; reasoning_effort: string | null; output: string; checks: ProjectCheck[]; error: string | null; }
export interface Workflow { id: string; project_id: string; conversation_id: string; user_message_id: string; request: string; status: string; plan: { summary: string; tasks: PlannedTask[] } | null; roles?: Roles | null; summary: string; error: string | null; created_at: number; updated_at: number; tasks: ProjectTask[]; attempts: ProjectAttempt[]; changes: { attempt_id: string; path: string; operation: string; before_hash: string | null; after_hash: string | null }[]; }

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
