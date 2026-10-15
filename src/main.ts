import { invoke, isTauri } from '@tauri-apps/api/core';
import { emit, listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import type { Agent, ChatQuestion, PendingChatQuestion, AppInfo, Conversation, ConversationDetail, Discussion, Message, NativeSession, Project, RunRecord, RuntimeSnapshot, ServiceCheckResult, ServiceInfo, ServicePlanResult, ServiceUpdateStatus, Workflow } from './types';
import { bindingDialog, projectTeamMessageHtml, projectTeamMessages, workflowBusy } from './project_ui';
import { modelOptionsHtml, modelProviders, providerOptionsHtml, selectedProvider } from './model_select';
import { captureThoughtScroll, restoreThoughtScroll } from './thought_scroll';
import { plainThought, steerIds, thoughtFlow, type FlowPart } from './thought_flow';
import { agentMarkdown } from './agent_markdown';
import { agentTimeline } from './agent_timeline';
import { AgentTranscript } from './agent_transcript';
import { agentChatChrome, agentEmpty, projectAgentPage } from './project_agent_view';
import { installMessageMenu } from './message_menu';
import { projectProgressPage, progressContent } from './project_progress';
import { projectSettings } from './project_settings';
import { projectSessionsDialog } from './project_sessions';
import { applyTheme, backdropHtml, themePicker } from './theme_picker';
import { baseAgent, isSeat, seatSlot } from './seats';
import { autoConnect, companionDialog, confirmInstall, runSetup, scanAgents, scanned, setAutoConnect, setupBanner, setupLine } from './agent_setup';
import { answerDialog, questionCard, visibleQuestionReply } from './chat_questions';
import { toolActivity, toolLine, captureToolState, restoreToolState } from './tool_activity';
import { mountAttachments, hydrateAttachments, composeMessage, submittedAttachments, attachmentText, attachmentHtml, attachmentsBusy } from './attachments';
import './styles.css';
import './agent_transcript.css';
import './theme.css';

const app = document.querySelector<HTMLDivElement>('#app')!;
const icons: Record<string, string> = {
  plus: '<path d="M12 5v14M5 12h14"/>',
  search: '<circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 4 4"/>',
  chat: '<path d="M21 11.5a8.5 8.5 0 0 1-8.5 8.5H4l-2 2v-9.5A8.5 8.5 0 0 1 10.5 4h2A8.5 8.5 0 0 1 21 11.5Z"/>',
  group: '<circle cx="9" cy="8" r="3"/><path d="M3 21v-2a6 6 0 0 1 12 0v2M16 5a3 3 0 0 1 0 6m2 4a5 5 0 0 1 3 5"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  archive: '<path d="M4 8v12h16V8M3 4h18v4H3zM9 12h6"/>',
  arrow: '<path d="M5 12h14m-6-6 6 6-6 6"/>',
  settings: '<rect x="4" y="4" width="16" height="16" rx="4"/><path d="M8 8h8M8 12h8M8 16h5"/>',
  check: '<path d="m5 12 4 4L19 6"/>',
  lock: '<rect x="5" y="10" width="14" height="11" rx="3"/><path d="M8 10V7a4 4 0 0 1 8 0v3M12 14v3"/>',
  close: '<path d="m6 6 12 12M6 18 18 6"/>',
  edit: '<path d="m15 5 4 4M4 20l5-1L20 8l-4-4L5 15z"/>',
  palette: '<path d="M12 3a9 9 0 1 0 0 18c1.2 0 1.8-.8 1.8-1.7 0-1.2-1-1.6-1-2.6 0-.9.7-1.6 1.6-1.6H17a4 4 0 0 0 4-4C21 6.6 17 3 12 3Z"/><circle cx="7.5" cy="11" r="1"/><circle cx="10" cy="7" r="1"/><circle cx="15" cy="7.5" r="1"/>',
  panel: '<rect x="3" y="4" width="18" height="16" rx="3"/><path d="M15 4v16"/>',
};
const icon = (name: string, className = '') => `<svg class="icon ${className}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${icons[name] || icons.chat}</svg>`;
const escape = (value: string | number) => String(value).replace(/[&<>"']/g, char => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[char]!));
// 伴侣上传了头像就用头像（data URL 只含 base64 字符，可以直接放进 style）。
const portrait = (agent: Agent) => agent.avatar && /^data:image\/[a-z+]+;base64,[A-Za-z0-9+/=]+$/.test(agent.avatar) ? agent.avatar : '';
const avatar = (agent: Agent, size = '') => portrait(agent) ? `<span class="avatar albion-portrait ${size}" style="background-image:url('${portrait(agent)}')" aria-hidden="true"></span>` : isSeat(agent.id) ? `<span class="avatar ${escape(agent.accent)} ${size} seat" aria-hidden="true">${member(baseAgent(agent.id)).name[0]}<i class="seat-badge">${seatSlot(agent.id)}</i></span>` : `<span class="avatar ${escape(agent.accent)} ${size}" aria-hidden="true">${agent.name[0]}</span>`;
const brand = `<svg class="brand-mark" viewBox="0 0 40 40" aria-hidden="true"><defs><linearGradient id="brand-silver" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#7c8590"/><stop offset="1" stop-color="#4a5158"/></linearGradient></defs><rect width="40" height="40" rx="12" fill="url(#brand-silver)"/><path d="M13 13h14v14H13z" stroke="#f0f3f6" stroke-width="1.2" fill="none"/><circle cx="13" cy="13" r="3" fill="#e7ca87"/><circle cx="27" cy="13" r="3" fill="#e2efe5"/><circle cx="27" cy="27" r="3" fill="#97bce0"/><circle cx="13" cy="27" r="3" fill="#e5b8b5"/></svg>`;

let agents: Agent[] = [];
let info: AppInfo | null = null;
let conversations: Conversation[] = [];
let detail: ConversationDetail | null = null;
let selectedId = localStorage.getItem('hub.selected') || '';
let hudActive = false;
let filter: 'all' | 'direct' | 'group' = 'all';
let archived = false;
let search = '';
let screen: 'chat' | 'services' = 'chat';
let listGeneration = 0;
let detailGeneration = 0;
let sending = false;
let settingsAgentId = '';
const liveProviderSelection: Record<string, string> = {};
let settingsPending = 0;
let settingsQueue: Promise<void> = Promise.resolve();
const effortLabels: Record<string, string> = { none: '关闭思考', off: '关闭思考', minimal: '最少', low: '低', medium: '中', high: '高', xhigh: '很高', max: '最大', ultra: 'Ultra' };
let searchTimer: ReturnType<typeof setTimeout>;
let toastTimer: ReturnType<typeof setTimeout>;
let pendingSave: { conversationId: string; messageId: string; content: string } | null = null;
const emptyRuntime = (): RuntimeSnapshot => ({ revision: 0, connection: 'disconnected', executable: null, version: null, error: null, active: null, models: [], default_model: null, default_effort: null });
// 席位（codex-win-2 …）和底层成员共用同一个原生连接，读写都落到底层成员上。
const runtimes: Record<string, RuntimeSnapshot> = new Proxy<Record<string, RuntimeSnapshot>>({ 'codex-win': emptyRuntime(), 'hermes-win': emptyRuntime(), 'dsh-win': emptyRuntime(), 'claude-win': emptyRuntime(), 'albion-wsl': emptyRuntime() }, {
  get: (target, key) => typeof key === 'string' ? target[baseAgent(key)] : undefined,
  set: (target, key, value) => { target[baseAgent(String(key))] = value; return true; },
});
const seatLabels = new Map<string, string>();
const currentAgentId = () => detail?.conversation.kind === 'direct' ? detail.conversation.members[0] : detail?.conversation.kind === 'group' && activeProjectModule !== 'team' ? activeProjectModule : 'codex-win';
const activeProjectAgent = () => detail?.conversation.kind === 'group' && detail.project && activeProjectModule !== 'team' && detail.project_agent_chats.some(chat => chat.agent_id === activeProjectModule) ? activeProjectModule : null;
const currentRuntime = () => runtimes[currentAgentId()] || emptyRuntime();
const harnessKey = (agent = currentAgentId(), id = baseAgent(agent)) => id === 'hermes-win' ? 'hermes' : id === 'dsh-win' ? 'dsh' : id === 'claude-win' ? 'claude' : id === 'albion-wsl' ? 'albion' : 'codex';
const busySnapshot = (snapshot: RuntimeSnapshot) => runsOf(snapshot).some(liveRun);
type NativeRun = NonNullable<RuntimeSnapshot['active']>;
/// 一个成员可以同时在多个会话里回复；旧快照只有 `active`。
const runsOf = (snapshot?: RuntimeSnapshot): NativeRun[] => !snapshot ? [] : snapshot.runs?.length ? snapshot.runs : snapshot.active ? [snapshot.active] : [];
const liveRun = (run?: NativeRun | null) => !!run && ['starting', 'running', 'cancelling'].includes(run.status);
/// 这个会话最近一次的运行（正在进行的优先）。
const runIn = (snapshot: RuntimeSnapshot | undefined, conversationId: string | null) => { const runs = runsOf(snapshot).filter(run => run.conversation_id === conversationId); return runs.find(liveRun) || runs[runs.length - 1] || null; };
const busyIn = (snapshot: RuntimeSnapshot | undefined, conversationId: string | null) => liveRun(runIn(snapshot, conversationId));

let pendingSend: { conversationId: string; messageId: string; content: string } | null = null;
const lastFinalRuns: Record<string, string> = {};
const lastFinalSeeded = new Set<string>();
const projectChatRefreshStamp: Record<string, string> = {};
let eventSubscribed = false;
let latestDiscussion: Discussion | null = null;
const projectJobs = new Map<string, Workflow>();
const removedProjectJobs = new Set<string>();
const projectAgentChatDetails = new Map<string, ConversationDetail>();
const projectRoomsByGroup = new Map<string, ConversationDetail['project_agent_chats']>();
let activeProjectModule = 'team';
let expandedProjectGroup = '';
let projectRevision = 0;
let pendingProject: { conversationId: string; messageId: string; content: string } | null = null;
let pendingSummary: { projectId: string; enabled: boolean } | null = null;
const roomProjects = () => [...new Map([...(detail?.workflows || []), ...projectJobs.values()].filter(job => job.conversation_id === selectedId).map(job => [job.id, job])).values()].sort((a, b) => b.created_at - a.created_at);
const currentWorkflow = () => roomProjects()[0] || null;
const projectAgentBusy = (id: string) => [...projectJobs.values()].some(job => workflowBusy(job) && job.attempts.some(attempt => attempt.agent_id === id && attempt.activity !== 'coordinate' && ['starting', 'running', 'cancelling'].includes(attempt.status)));
function adoptProject(job: Workflow, event = false) {
  if (removedProjectJobs.has(job.id)) return;
  const old = projectJobs.get(job.id);
  if (!event && old && (old.updated_at > job.updated_at || old.attempts.length > job.attempts.length || (!workflowBusy(old) && workflowBusy(job)) || (old.status === 'cancelling' && workflowBusy(job) && job.status !== 'cancelling'))) return;
  projectJobs.set(job.id, job);
  updateProjectControls();
}
let discussionRevision = 0;
let groupConnecting = false;
let pendingDiscussion: { conversationId: string; messageId: string; content: string; participants: string[]; rounds: number } | null = null;
const discussionBusy = (job: Discussion | null | undefined) => !!job && ['running', 'cancelling'].includes(job.status);
const currentDiscussion = () => latestDiscussion?.conversation_id === selectedId ? latestDiscussion : detail?.discussions[0] || null;
function groupOptions() {
  const room = detail!.conversation;
  let stored: { participants?: string[]; rounds?: number } = {};
  try { const parsed = JSON.parse(localStorage.getItem(`hub.discussion.${room.id}`) || '{}'); if (parsed && typeof parsed === 'object') stored = parsed; } catch { /* use defaults */ }
  return { participants: Array.isArray(stored.participants) ? room.members.filter(id => stored.participants!.includes(id)) : room.members.filter(id => id !== 'albion-wsl'), rounds: [1, 2, 3].includes(stored.rounds || 0) ? stored.rounds! : 1 };
}
function adoptDiscussion(job: Discussion) {
  if (latestDiscussion?.id === job.id && (job.updated_at < latestDiscussion.updated_at || job.turns.length < latestDiscussion.turns.length || (!discussionBusy(latestDiscussion) && discussionBusy(job)) || (latestDiscussion.status === 'cancelling' && job.status === 'running'))) return;
  if (latestDiscussion && job.id !== latestDiscussion.id && job.created_at < latestDiscussion.created_at) return;
  latestDiscussion = job;
  if (detail?.conversation.id === job.conversation_id) {
    detail.discussions = [job, ...detail.discussions.filter(item => item.id !== job.id)];
  }
  updateRuntimeControls();
}
const runBusy = () => busyIn(currentRuntime(), selectedId);
const nativeChat = () => detail?.conversation.kind === 'direct' && ['codex-win', 'hermes-win', 'dsh-win', 'claude-win', 'albion-wsl'].includes(currentAgentId());
const connectionLabel = () => currentRuntime().connection === 'connected' ? '接口已连接' : currentRuntime().connection === 'connecting' ? '正在连接' : '未连接';
function adoptRuntime(snapshot: RuntimeSnapshot, agentId = currentAgentId()) {
  const old = runtimes[agentId];
  if (old && snapshot.revision < old.revision) return false;
  runtimes[agentId] = snapshot;
  return true;
}

app.innerHTML = `
  ${backdropHtml}
  <div class="shell">
    <aside class="sidebar">
      <div class="brand" id="window-drag-region" title="拖动窗口，双击最大化">${brand}<div><strong>同席</strong><span>AGENT HUB</span></div><span class="version" id="app-version">…</span></div>
      <div class="sidebar-top"><span class="eyebrow">你的协作空间</span><button id="new-conversation" class="icon-button filled" aria-label="新建会话" title="新建会话">${icon('plus')}</button></div>
      <label class="search">${icon('search')}<input id="search" placeholder="搜索会话或消息" aria-label="搜索会话或消息" maxlength="120"><kbd>Ctrl K</kbd></label>
      <div class="filters" aria-label="会话类型"><button data-filter="all" class="selected">全部</button><button data-filter="direct">私聊</button><button data-filter="group">群聊</button></div>
      <div class="list-label"><span id="list-label">最近会话</span><button id="archive-filter" title="查看归档">${icon('archive')}<span>归档</span></button></div>
      <div id="conversation-list" class="conversation-list" aria-label="会话列表"></div>
      <div class="sidebar-bottom"><div class="sidebar-actions"><button id="service-link">${icon('settings')}<span>成员与服务</span><span class="count-pill" id="member-count">${agents.length}</span></button><button id="theme-link" class="icon-button" aria-label="主题背景" title="主题背景">${icon('palette')}</button></div><div class="local-indicator"><span class="status-dot"></span>本地工作空间<span>桌面版</span></div></div>
    </aside>
    <main id="main" class="main"><div class="loading">正在打开你的工作空间…</div></main>
  </div>
  <div class="window-controls" aria-label="窗口操作"><button id="hud-toggle" aria-label="HUD 模式" title="HUD 模式">▣</button><button id="window-minimize" aria-label="最小化到托盘" title="最小化到托盘">−</button><button id="window-maximize" aria-label="最大化" title="最大化">□</button><button id="window-close" aria-label="退出同席" title="退出同席">×</button></div>
  ${(['North', 'South', 'East', 'West', 'NorthEast', 'NorthWest', 'SouthEast', 'SouthWest'] as const).map(direction => `<div class="resize-edge resize-${direction}" data-resize="${direction}" aria-hidden="true"></div>`).join('')}
  <div id="toast" class="toast" role="status" aria-live="polite"></div>
  <dialog id="modal" class="modal"></dialog>
`;

void applyTheme(undefined, false);
const main = document.querySelector<HTMLDivElement>('#main')!;
document.querySelector('#theme-link')!.addEventListener('click', () => void themePicker({ escape, open: openModal, modal, toast }).catch(error => toast(errorText(error), true)));
/// 摘要胶囊：点一下展开对应的浮层，再点、点外面或按 Esc 收起。设置改动后摘要跟着变。
function wireToolChips() {
  const tools = main.querySelector<HTMLElement>('#chat-tools');
  if (!tools) return;
  const close = () => tools.querySelectorAll<HTMLElement>('.tool-pop.open').forEach(pop => { pop.classList.remove('open'); tools.querySelector(`[data-tool-toggle="${pop.id}"]`)?.setAttribute('aria-expanded', 'false'); });
  tools.querySelectorAll<HTMLButtonElement>('[data-tool-toggle]').forEach(chip => chip.addEventListener('click', () => {
    const pop = tools.querySelector<HTMLElement>(`#${chip.dataset.toolToggle}`)!;
    const opening = !pop.classList.contains('open');
    close();
    if (opening) { pop.classList.add('open'); chip.setAttribute('aria-expanded', 'true'); }
  }));
  tools.addEventListener('change', () => setTimeout(paintToolChips, 0));
  const outside = (event: Event) => {
    if (!tools.isConnected) { document.removeEventListener('pointerdown', outside, true); document.removeEventListener('keydown', escape_, true); return; }
    if (!tools.contains(event.target as Node)) close();
  };
  const escape_ = (event: KeyboardEvent) => { if (event.key === 'Escape' && tools.querySelector('.tool-pop.open')) { close(); event.stopPropagation(); } };
  document.addEventListener('pointerdown', outside, true);
  document.addEventListener('keydown', escape_, true);
  paintToolChips();
}

/// 换会话、换页面时整块轻轻滑入；同一页面里的重绘不再播放，免得刷新时闪。
let enterTimer: ReturnType<typeof setTimeout>;
function enterView(view: string) {
  if (main.dataset.view === view) return;
  main.dataset.view = view;
  main.classList.remove('view-enter'); void main.offsetWidth; main.classList.add('view-enter');
  clearTimeout(enterTimer); enterTimer = setTimeout(() => main.classList.remove('view-enter'), 520);
}
let membersOpen = localStorage.getItem('hub.membersPanel') === '1';
new MutationObserver(()=>hydrateAttachments(main)).observe(main,{childList:true,subtree:true});
let pendingQuestions:PendingChatQuestion[]=[];
const noticedQuestions=new Set<string>();
main.addEventListener('focusout', () => { setTimeout(() => renderLiveSettings(), 0); });
const modal = document.querySelector<HTMLDialogElement>('#modal')!;
installMessageMenu(main, () => selectedId, async target => {
  try {
    const removed = await invoke<{ workflow_id: string | null }>('remove_conversation_item', { conversationId: target.conversationId, kind: target.kind, id: target.id });
    if (removed.workflow_id) { removedProjectJobs.add(removed.workflow_id); projectJobs.delete(removed.workflow_id); }
    projectAgentChatDetails.delete(target.conversationId);
    await refreshList();
    if (selectedId) await selectConversation(selectedId);
    toast('本机记录已删除');
  } catch (error) { toast(errorText(error), true); }
});

if (isTauri()) {
  const window = getCurrentWindow();
  // HUD 视图 = 同一个前端 + 一条紧凑皮肤。身份靠 `?win=hud`（查询串在 # 之前，参考 Hermes 的
  // hud-url 契约：放进 hash 会被前端路由吃掉），窗口 label 作为兜底。
  if (new URLSearchParams(location.search).get('win') === 'hud' || window.label === 'hud') {
    document.documentElement.dataset.hud = '1';
    // HUD 有独立的 WebView2 数据目录，读不到主窗口的「当前会话」，靠事件同步。
    void listen<{ id: string }>('hud:select', event => { if (event.payload?.id) void selectConversation(event.payload.id); });
    void emit('hud:ready');
    // 窗口创建时先不显示（参考 Hermes 的 show:false + reveal）：皮肤套上之后再露脸，避免白闪。
    void window.show();
    // 「光标在输入框里」才亮起正文画布（Hermes 的 data-hud-typing 同理）。要查询 :focus 而不是
    // 读 activeElement——切到别的 App 时窗口失焦但 activeElement 不动，查询才不会把画布一直点亮。
    const applyTyping = () => document.documentElement.toggleAttribute('data-hud-typing', !!(document.querySelector('#message-input:focus,[data-project-agent-chat-input]:focus') || (modal.open && modal.contains(document.activeElement))));
    applyTyping();
    document.addEventListener('focusin', applyTyping);
    document.addEventListener('focusout', applyTyping);
    // 注意：这里的 `window` 是 Tauri 的窗口对象，DOM 的焦点事件得走 globalThis。
    // 浮窗被拉大/缩小时把最新一条重新贴到底：尺寸变了布局就变了，不然最后一行会飘到视窗外。
    void window.onResized(() => {
      if (document.documentElement.dataset.hud !== '1') return;
      pinToBottom(document.querySelector('.messages'));
    });
    globalThis.addEventListener('focus', applyTyping);
    globalThis.addEventListener('blur', () => requestAnimationFrame(applyTyping));
  }
  // 两个窗口的输入框都随内容长高（主窗口最多 200px、HUD 里 96px），和 Hermes 的输入栏一样。
  // 原来这条只挂在 HUD 分支里，主窗口固定三行看不出来；输入框改成一行起就必须两个窗口都挂。
  document.addEventListener('input', event => {
    const input = event.target as HTMLTextAreaElement | null;
    if (!(input?.id === 'message-input' || input?.matches('[data-project-agent-chat-input]'))) return;
    resizeComposer(input);
  });
  const action = (work: () => Promise<unknown>) => void work().catch(error => toast(errorText(error), true));
  document.querySelector('#window-minimize')!.addEventListener('click', () => action(() => window.minimize()));
  document.querySelector('#window-maximize')!.addEventListener('click', () => action(() => window.toggleMaximize()));
  document.querySelector('#window-close')!.addEventListener('click', () => action(() => window.close()));
  const hudButton = document.querySelector<HTMLButtonElement>('#hud-toggle')!;
  if (document.documentElement.dataset.hud === '1') {
    hudButton.hidden = true;
    // 退出按钮嵌在输入栏里，跟着会话一起重渲染，所以走委托。
    document.addEventListener('click', event => {
      if (!(event.target as Element | null)?.closest('[data-hud-exit]')) return;
      void action(async () => { await invoke<boolean>('set_hud_enabled', { enabled: false }); });
    });
  }
  else hudButton.addEventListener('click', () => action(async () => {
    const open = await invoke<boolean>('set_hud_enabled', { enabled: !hudButton.classList.contains('active') });
    hudButton.classList.toggle('active', open);
    hudActive = open;
    if (open && selectedId) void emit('hud:select', { id: selectedId });
  }));
  void listen('hud:ready', () => { if (hudActive && selectedId) void emit('hud:select', { id: selectedId }); });
  // HUD 也可能从它自己那边、托盘或快捷键关掉；以广播为准，按钮不能和真实状态不一致。
  void listen<boolean>('hud:active', event => {
    const open = event.payload === true;
    hudActive = open;
    hudButton.classList.toggle('active', open);
  });
  const directions = ['North', 'South', 'East', 'West', 'NorthEast', 'NorthWest', 'SouthEast', 'SouthWest'] as const;
  // 屏幕坐标 → 绝对 bounds：保持对边不动，宽高夹到 HUD 的最小尺寸（与建窗的 min_inner_size 一致）。
  const hudResizeBounds = (origin: { x: number; y: number; width: number; height: number }, direction: string, dx: number, dy: number) => {
    const west = direction.includes('West');
    const east = direction.includes('East');
    const north = direction.includes('North');
    const south = direction.includes('South');
    const width = west ? Math.max(380, origin.width - dx) : east ? Math.max(380, origin.width + dx) : origin.width;
    const height = north ? Math.max(160, origin.height - dy) : south ? Math.max(160, origin.height + dy) : origin.height;
    return { x: west ? origin.x + origin.width - width : origin.x, y: north ? origin.y + origin.height - height : origin.y, width, height };
  };
  for (const direction of directions) {
    document.querySelector(`[data-resize="${direction}"]`)!.addEventListener('mousedown', event => {
      if ((event as MouseEvent).button !== 0 || modal.open) return;
      event.preventDefault();
      if (document.documentElement.dataset.hud !== '1') {
        action(() => window.startResizeDragging(direction));
        return;
      }
      // HUD 平时不可缩放（透明无边框 + resizable 会给系统留边缘热区，一拖就把窗口拖长），
      // 所以缩放只能程序化做：这里用指针的屏幕坐标算绝对 bounds，交给 set_hud_bounds
      // （它只在尺寸真的变了那一下临时开一下 resizable）。用屏幕坐标而不是 client 坐标——
      // 窗口在拖的过程中位置和尺寸同时在变，client 坐标不可信。
      // 注意本文件里 `window` 被 getCurrentWindow() 遮住了，DOM 的根要显式取。
      const root = globalThis as unknown as Window & typeof globalThis;
      const origin = { x: root.screenX, y: root.screenY, width: root.outerWidth, height: root.outerHeight };
      const start = { x: (event as MouseEvent).screenX, y: (event as MouseEvent).screenY };
      const move = (moveEvent: MouseEvent) => {
        const bounds = hudResizeBounds(origin, direction, moveEvent.screenX - start.x, moveEvent.screenY - start.y);
        void invoke('set_hud_bounds', bounds).catch(() => {});
      };
      const done = () => {
        root.removeEventListener('mousemove', move, true);
        root.removeEventListener('mouseup', done, true);
      };
      root.addEventListener('mousemove', move, true);
      root.addEventListener('mouseup', done, true);
    });
  }
  app.addEventListener('mousedown', event => {
    const target = event.target as Element;
    if (event.button !== 0 || modal.open || target.closest('button, input, textarea, select, a')) return;
    if (target.closest('#window-drag-region, .chat-header, [data-hud-grip], html[data-hud] .composer, html[data-hud] .project-agent-chat-composer')) {
      event.preventDefault();
      action(() => event.detail === 2 ? window.toggleMaximize() : window.startDragging());
    }
  });
  const updateWindowControl = async () => {
    const maximized = await window.isMaximized();
    document.body.classList.toggle('maximized', maximized);
    const button = document.querySelector<HTMLButtonElement>('#window-maximize')!;
    button.textContent = maximized ? '❐' : '□';
    button.title = maximized ? '还原窗口' : '最大化';
    button.setAttribute('aria-label', button.title);
  };
  void window.onResized(() => void updateWindowControl().catch(() => {}));
  void updateWindowControl().catch(() => {});
}

function toast(text: string, error = false) {
  const element = document.querySelector<HTMLDivElement>('#toast')!;
  clearTimeout(toastTimer);
  element.textContent = text;
  element.className = `toast visible ${error ? 'error' : ''}`;
  toastTimer = setTimeout(() => element.classList.remove('visible'), 4200);
}

function errorText(error: unknown) { return typeof error === 'string' ? error : error instanceof Error ? error.message : '操作未完成，请重试'; }
// agents 要等 boot 里那次 list_agents 回来才有；在这之前事件回调（runtime state）也会
// 走到这里，取不到成员就用 id 兜底——不能让它抛 "reading 'name'"，那会在 HUD 打开时弹错。
function member(id: string): Agent {
  const owner = isSeat(id) ? agents.find(agent => agent.id === baseAgent(id)) : null;
  if (owner) return { ...owner, id, name: seatLabels.get(id) || `${owner.name} ${seatSlot(id)}号` };
  return agents.find(agent => agent.id === id) || { id, name: id, subtitle: '', role: '', location: '', accent: '', status: 'not_connected' };
}
function time(timestamp: number) { return new Intl.DateTimeFormat('zh-CN', { hour: '2-digit', minute: '2-digit' }).format(timestamp); }
function date(timestamp: number) { return new Intl.DateTimeFormat('zh-CN', { month: 'short', day: 'numeric' }).format(timestamp); }
function draftKey(id: string) { return `hub.draft.${id}`; }
function readDraft(id: string) { return localStorage.getItem(draftKey(id)) || ''; }

function renderList() {
  // 侧栏卡片正在就地改名时先不重绘，免得把输入框冲掉；结束后补一次。
  if (renamingConversation) { listRenderPending = true; return; }
  document.querySelectorAll<HTMLButtonElement>('[data-filter]').forEach(button => button.classList.toggle('selected', button.dataset.filter === filter));
  document.querySelector('#list-label')!.textContent = archived ? '归档会话' : '最近会话';
  document.querySelector('#archive-filter')!.classList.toggle('active', archived);
  const visible = conversations.filter(conversation => filter === 'all' || conversation.kind === filter);
  const list = document.querySelector<HTMLDivElement>('#conversation-list')!;
  list.innerHTML = visible.length ? visible.map(conversation => {
    const agent = conversation.kind === 'direct' ? member(conversation.members[0]) : null;
    const rooms = conversation.kind === 'group' ? projectRoomsByGroup.get(conversation.id) || [] : [];
    const expanded = conversation.id === expandedProjectGroup && rooms.length > 0;
    const active = screen === 'chat' && conversation.id === selectedId;
    const row = `<button class="conversation ${active ? 'active' : ''}" data-conversation="${escape(conversation.id)}">
      ${agent ? avatar(agent) : `<span class="avatar group-avatar">${icon('group')}</span>`}
      <span class="conversation-text"><span class="conversation-title">${escape(conversation.title)}</span><span class="conversation-preview">${escape(attachmentText(visibleQuestionReply(conversation.preview,true)) || (conversation.kind === 'group' ? `${conversation.members.length} 位成员 · 新的讨论` : agent!.subtitle))}</span></span>
      ${rooms.length ? `<span class="project-chevron" aria-hidden="true">${expanded ? '⌄' : '›'}</span>` : ''}<span class="conversation-meta">${pendingQuestions.some(q=>q.conversation_id===conversation.id||q.group_id===conversation.id)?'<span class="question-sidebar-badge">待回答</span>':''}${conversation.message_count ? `<span>${time(conversation.updated_at)}</span><small>${conversation.message_count}</small>` : '<span class="new-dot"></span>'}</span>
    </button>`;
    if (!rooms.length) return `<div class="conversation-item" data-order-id="${escape(conversation.id)}">${row}</div>`;
    const children = `<button class="project-module-row ${active && activeProjectModule === 'team' ? 'active' : ''}" data-project-module="team" data-group-id="${escape(conversation.id)}"><span class="project-module-icon">☷</span><span>团队对话</span></button><button class="project-module-row ${active && activeProjectModule === 'progress' ? 'active' : ''}" data-project-module="progress" data-group-id="${escape(conversation.id)}"><span class="project-module-icon">◴</span><span>项目协作进度</span></button>${rooms.map(chat => `<button class="project-module-row ${active && activeProjectModule === chat.agent_id ? 'active' : ''}" data-project-module="${escape(chat.agent_id)}" data-group-id="${escape(conversation.id)}">${avatar(member(chat.agent_id), 'project-module-avatar')}<span>${escape(member(chat.agent_id).name)} 专属会话</span>${pendingQuestions.some(q=>q.kind!=='coordination'&&q.group_id===conversation.id&&q.agent_id===chat.agent_id)?'<span class="question-sidebar-badge">待答</span>':projectAgentBusy(chat.agent_id) ? '<i class="project-active-dot"></i>' : ''}</button>`).join('')}`;
    return `<div class="conversation-item conversation-branch ${expanded ? 'expanded' : ''}" data-order-id="${escape(conversation.id)}">${row}<div class="project-subnav" id="project-subnav-${escape(conversation.id)}" ${expanded ? '' : 'hidden'}>${children}</div></div>`;
  }).join('') : `<div class="list-empty">这里还没有会话</div>`;
  list.querySelectorAll<HTMLButtonElement>('[data-conversation]').forEach(button => button.addEventListener('click', () => {
    const id = button.dataset.conversation!;
    if ((projectRoomsByGroup.get(id)?.length || 0) > 0 && selectedId === id && screen === 'chat') {
      expandedProjectGroup = expandedProjectGroup === id ? '' : id;
      renderList();
    } else void selectConversation(id);
  }));
  list.querySelectorAll<HTMLButtonElement>('[data-project-module]').forEach(button => button.addEventListener('click', () => void selectProjectModule(button.dataset.groupId || '', button.dataset.projectModule || 'team')));
  bindConversationDrag(list, visible.map(conversation => conversation.id));
}

/// 就地改名：把标题元素换成输入框，Enter 或失焦保存，Esc 取消。侧栏卡片右键与聊天页标题双击共用。
let renamingConversation: string | null = null;
let listRenderPending = false;
function startInlineRename(id: string, title: HTMLElement) {
  const conversation = conversations.find(item => item.id === id) || (detail?.conversation.id === id ? detail.conversation : null);
  if (!conversation || renamingConversation) return;
  renamingConversation = id;
  const original = conversation.title;
  const input = document.createElement('input');
  input.className = 'inline-rename';
  input.maxLength = 80;
  input.value = original;
  input.setAttribute('aria-label', '会话名称');
  title.replaceChildren(input);
  // 输入框可能在侧栏的按钮里：点击、按下不能冒泡成“打开会话”或“开始拖动”。
  for (const type of ['click', 'pointerdown', 'dblclick', 'contextmenu'] as const) input.addEventListener(type, event => event.stopPropagation());
  let done = false;
  const finish = async (save: boolean) => {
    if (done) return;
    done = true;
    const value = input.value.trim();
    renamingConversation = null;
    title.textContent = original;
    if (save && value && value !== original) {
      try {
        const updated = await invoke<Conversation>('rename_conversation', { id, title: value });
        title.textContent = updated.title;
        if (detail?.conversation.id === id) {
          detail.conversation.title = updated.title;
          const heading = document.querySelector<HTMLElement>('.chat-heading h1');
          if (heading) heading.textContent = updated.title;
        }
        toast('会话名称已更新');
        await refreshList();
        return;
      } catch (error) { toast(errorText(error), true); }
    }
    if (listRenderPending) { listRenderPending = false; renderList(); }
  };
  input.addEventListener('keydown', event => {
    event.stopPropagation();
    if (event.key === 'Enter' && !event.isComposing) { event.preventDefault(); void finish(true); }
    else if (event.key === 'Escape') { event.preventDefault(); void finish(false); }
  });
  input.addEventListener('blur', () => void finish(true));
  input.focus();
  input.select();
}

/// 侧栏卡片右键菜单：重命名、归档/恢复、删除，样式沿用消息右键菜单。
function installConversationMenu(list: HTMLElement) {
  let menu: HTMLElement | null = null;
  const close = () => { menu?.remove(); menu = null; };
  list.addEventListener('contextmenu', event => {
    const row = (event.target as Element).closest<HTMLElement>('[data-conversation]');
    if (!row) return;
    event.preventDefault(); close();
    const id = row.dataset.conversation!;
    menu = document.createElement('div');
    menu.className = 'message-context-menu conversation-context-menu'; menu.setAttribute('role', 'menu');
    const conversation = conversations.find(item => item.id === id);
    menu.innerHTML = `<button type="button" role="menuitem" data-conversation-rename>重命名</button>`
      + (conversation ? `<button type="button" role="menuitem" data-conversation-archive>${conversation.archived ? '恢复会话' : '归档'}</button><button type="button" role="menuitem" class="danger" data-conversation-delete>删除</button>` : '');
    document.body.append(menu);
    menu.style.left = `${Math.min(event.clientX, innerWidth - menu.offsetWidth - 12)}px`;
    menu.style.top = `${Math.min(event.clientY, innerHeight - menu.offsetHeight - 12)}px`;
    const button = menu.querySelector<HTMLButtonElement>('[data-conversation-rename]')!;
    button.focus();
    button.addEventListener('click', () => {
      close();
      const title = list.querySelector<HTMLElement>(`[data-conversation="${CSS.escape(id)}"] .conversation-title`);
      if (title) startInlineRename(id, title);
    });
    menu.querySelector('[data-conversation-archive]')?.addEventListener('click', () => { close(); if (conversation) void toggleArchive(conversation); });
    menu.querySelector('[data-conversation-delete]')?.addEventListener('click', () => { close(); if (conversation) confirmDeleteConversation(conversation); });
  });
  document.addEventListener('pointerdown', event => { if (!menu?.contains(event.target as Node)) close(); });
  document.addEventListener('keydown', event => { if (event.key === 'Escape') close(); });
  window.addEventListener('blur', close);
}

/// 侧栏会话卡片拖动排序：按住移动超过 5px 才算拖动（普通点击不受影响）；落点上半部插到前面、
/// 下半部插到后面，松手立即重排并保存到本机数据库。用 pointer 事件实现：Windows 的 WebView2
/// 会把 HTML5 拖放交给系统处理，显示禁止图标。
let conversationDrag: { id: string; startY: number; pointer: number; active: boolean; target?: string; after?: boolean } | null = null;
let suppressConversationClick = false;
function bindConversationDrag(list: HTMLElement, visibleIds: string[]) {
  const clear = () => list.querySelectorAll('.drop-before, .drop-after, .dragging').forEach(item => item.classList.remove('drop-before', 'drop-after', 'dragging'));
  const finish = () => { conversationDrag = null; document.body.classList.remove('conversation-dragging'); clear(); };
  list.querySelectorAll<HTMLElement>('[data-order-id]').forEach(item => {
    item.addEventListener('pointerdown', event => {
      if (event.button !== 0 || !(event.target as HTMLElement).closest('.conversation')) return;
      conversationDrag = { id: item.dataset.orderId!, startY: event.clientY, pointer: event.pointerId, active: false };
    });
  });
  list.onpointermove = event => {
    const drag = conversationDrag;
    if (!drag || event.pointerId !== drag.pointer) return;
    if (!drag.active) {
      if (Math.abs(event.clientY - drag.startY) < 5) return;
      drag.active = true;
      list.setPointerCapture(event.pointerId);
      document.body.classList.add('conversation-dragging');
      list.querySelector(`[data-order-id="${CSS.escape(drag.id)}"]`)?.classList.add('dragging');
    }
    event.preventDefault();
    const box = list.getBoundingClientRect();
    if (event.clientY < box.top + 28) list.scrollTop -= 10;
    else if (event.clientY > box.bottom - 28) list.scrollTop += 10;
    list.querySelectorAll('.drop-before, .drop-after').forEach(item => item.classList.remove('drop-before', 'drop-after'));
    const target = [...list.querySelectorAll<HTMLElement>('[data-order-id]')].find(item => {
      const rect = item.getBoundingClientRect();
      return event.clientY >= rect.top && event.clientY < rect.bottom;
    });
    drag.target = undefined;
    if (!target || target.dataset.orderId === drag.id) return;
    const rect = target.getBoundingClientRect();
    drag.target = target.dataset.orderId;
    drag.after = event.clientY > rect.top + Math.min(rect.height, 72) / 2;
    target.classList.add(drag.after ? 'drop-after' : 'drop-before');
  };
  list.onpointerup = event => {
    const drag = conversationDrag;
    if (!drag || event.pointerId !== drag.pointer) return;
    finish();
    if (!drag.active) return;
    suppressConversationClick = true;
    setTimeout(() => { suppressConversationClick = false; }, 0);
    if (drag.target) void moveConversation(visibleIds, drag.id, drag.target, Boolean(drag.after));
  };
  list.onpointercancel = finish;
  // 拖动结束时紧跟着的那次 click 不当作打开会话（列表元素常驻，只绑定一次）。
  if (!list.dataset.dragBound) {
    list.dataset.dragBound = '1';
    list.addEventListener('click', event => { if (suppressConversationClick) { event.stopPropagation(); event.preventDefault(); } }, true);
  }
}

async function moveConversation(visibleIds: string[], moved: string, target: string, after: boolean) {
  const order = visibleIds.filter(id => id !== moved);
  order.splice(order.indexOf(target) + (after ? 1 : 0), 0, moved);
  // 本地先按新顺序换位（只动可见会话占据的位置），与后端的保存规则一致。
  const slots = new Set(order);
  const byId = new Map(conversations.map(conversation => [conversation.id, conversation]));
  let next = 0;
  conversations = conversations.map(conversation => slots.has(conversation.id) ? byId.get(order[next++])! : conversation);
  renderList();
  try {
    await invoke('set_conversation_order', { ids: order });
  } catch (error) {
    toast(errorText(error), true);
    await refreshList();
  }
}

async function refreshList() {
  const generation = ++listGeneration;
  const result = await invoke<Conversation[]>('list_conversations', { search, archived });
  if (generation !== listGeneration) return;
  conversations = result;
  renderList();
}

async function selectConversation(id: string) {
  const generation = ++detailGeneration;
  selectedId = id;
  screen = 'chat';
  activeProjectModule = 'team';
  localStorage.setItem('hub.selected', id);
  renderList();
  if (hudActive) void emit('hud:select', { id });
  try {
    const result = await invoke<ConversationDetail>('get_conversation', { id });
    if (generation !== detailGeneration || screen !== 'chat') return;
    detail = result;
    projectRoomsByGroup.set(id, result.project_agent_chats || []);
    for (const chat of result.project_agent_chats || []) if (chat.label) seatLabels.set(chat.agent_id, chat.label);
    const savedModule = result.project ? localStorage.getItem(`hub.project-module.${id}`) || 'team' : 'team';
    activeProjectModule = (savedModule === 'team' || savedModule === 'progress' || result.project_agent_chats.some(chat => chat.agent_id === savedModule)) ? savedModule : 'team';
    expandedProjectGroup = result.project ? id : '';
    for (const job of result.workflows || []) adoptProject(job);
    renderList();
    renderChat();
  } catch (error) {
    if (generation !== detailGeneration) return;
    // 会话已在别处被删除（如另一窗口或后台）：刷新侧栏去掉失效卡片，改选第一个会话。
    if (errorText(error).includes('会话不存在')) {
      toast('这个会话已被删除，已刷新列表', true);
      detail = null; selectedId = ''; localStorage.removeItem('hub.selected');
      await refreshList();
      if (conversations[0]) await selectConversation(conversations[0].id); else renderChat();
      return;
    }
    toast(errorText(error), true);
  }
}

async function selectProjectModule(groupId: string, moduleId: string) {
  if (selectedId !== groupId) await selectConversation(groupId);
  if (!detail || detail.conversation.id !== groupId || !detail.project) return;
  if (moduleId !== 'team' && moduleId !== 'progress' && !detail.project_agent_chats.some(chat => chat.agent_id === moduleId)) return;
  activeProjectModule = moduleId;
  expandedProjectGroup = groupId;
  localStorage.setItem(`hub.project-module.${groupId}`, moduleId);
  renderList();
  renderChat();
}

/// 滚到底。布局稳定前算出来的 scrollHeight 会偏小（打开会话时实测差 52px），
/// 所以这一帧滚一次、下一帧再补一次。
function pinToBottom(box: Element | null) {
  if (!box) return;
  box.scrollTop = box.scrollHeight;
  requestAnimationFrame(() => { box.scrollTop = box.scrollHeight; });
}

/// 输入框从一行起随内容长高（主窗口最多 200px、HUD 里 96px），和 Hermes 的输入栏一样。
function resizeComposer(input: HTMLTextAreaElement) {
  const cap = document.documentElement.dataset.hud === '1' ? 96 : 200;
  // 空输入框回到样式表的默认高度（HUD 里是单行），只在有内容时随内容长高。
  if (!input.value) { input.style.height = ''; return; }
  input.style.height = 'auto';
  input.style.height = `${Math.min(input.scrollHeight, cap)}px`;
}

function renderMessage(message: Message, includeDetails = true) {
  const own = message.sender_id === 'user';
  const labels: Record<Message['status'], string> = { local_only: '已保存到本机 · 尚未发送', pending: '正在提交原生会话', delivered: '已提交原生会话', streaming: '正在等待或接收回复', completed: '回复完成 · 已保存', interrupted: '已停止 · 保留已有内容', failed: '运行失败 · 可以发送新消息重试' };
  // 后端推理块，放在正文之前；没有就不占位。
  // 思考中保持展开（Hermes 那种边想边看，内容随流式增长）；回复结束自动收成一行。
  const streaming = message.status === 'streaming';
  const thoughtBlock = (text: string, live: boolean) => `<details class="thought" data-live="${live}"${live ? ' open' : ''}><summary>${live ? '思考中…' : '已思考'}</summary><div class="thought-body">${escape(text)}</div></details>`;
  // 思考和正文交替出现时按顺序穿插（一段想、一段说）；带提问卡片的回复仍用老布局。
  const flow = includeDetails && !own && !message.content.includes('```tongxi-question') ? thoughtFlow(message.thought || '', message.content) : null;
  const thought = includeDetails ? plainThought(message.thought || '') : '';
  const thoughtHtml = !flow && thought ? thoughtBlock(thought, streaming) : '';
  const toolEvents = includeDetails ? message.tool_events || [] : [];
  // 带位置标记的工具调用穿插在回复里显示；没有标记的（旧消息）仍汇总在回复顶部。
  const placed = new Set(flow?.flatMap(part => part.kind === 'tool' ? [part.text] : []));
  const toolsHtml = toolActivity(toolEvents.filter(event => !placed.has(event.event_id)), escape);
  const shown=attachmentText(own?message.content:visibleQuestionReply(message.content,message.status==='streaming'));
  const questionOnly=!own&&message.content.includes('```tongxi-question')&&!shown;
  const portrait = own ? '<span class="system-avatar">你</span>' : avatar(member(message.sender_id));
  // 正文每一段各占一个气泡（像 CLI 那样一条一条地出），思考穿插其间。
  const lastPart = flow?.[flow.length - 1];
  const toolHtml = (id: string) => { const event = toolEvents.find(item => item.event_id === id); return event ? toolLine(event, escape) : ''; };
  const partsHtml = (parts: FlowPart[]) => parts.map(part => part.kind === 'thought' ? thoughtBlock(part.text, streaming && part === lastPart) : part.kind === 'tool' ? toolHtml(part.text) : `<div class="bubble markdown">${agentMarkdown(attachmentText(part.text), escape)}</div>`).join('');
  const bodyHtml = flow?.length
    ? partsHtml(flow)
    : `<div class="bubble${own ? '' : ' markdown'}">${(own ? escape : (value: string) => agentMarkdown(value, escape))(questionOnly?'':shown || (message.status === 'streaming' ? '正在等待回复…' : message.status === 'interrupted' ? '已停止，尚无回复内容。' : '本次运行未产生回复内容。'))}</div>`;
  const delivery = `${attachmentHtml(message.content,message.conversation_id,escape)}<div class="message-delivery">${icon('check')}${labels[message.status]}</div>`;
  if (flow?.some(part => part.kind === 'steer')) {
    // 回复中途插过话：插话之前的回复留在原处，插话消息跟在后面，之后的回复在插话下面另起一条。
    const turns: FlowPart[][] = [[]];
    const steered: string[] = [];
    for (const part of flow) if (part.kind === 'steer') { steered.push(part.text); turns.push([]); } else turns[turns.length - 1].push(part);
    const author = (at?: number) => `<div class="message-author"><strong>${escape(member(message.sender_id)?.name || '成员')}</strong>${at ? `<time>${time(at)}</time>` : ''}</div>`;
    const reply = (parts: FlowPart[], at: number | undefined, extra: string, last: boolean) => `<article class="message agent-message">${author(at)}${extra}${parts.length ? partsHtml(parts) : last && streaming ? '<div class="bubble markdown">正在等待回复…</div>' : ''}${last ? delivery : ''}</article>`;
    let html = reply(turns[0], message.created_at, toolsHtml, false);
    steered.forEach((id, index) => {
      const said = detail?.messages.find(item => item.id === id);
      if (said) html += renderMessage(said, includeDetails);
      html += reply(turns[index + 1], said?.created_at, '', index === steered.length - 1);
    });
    return `<div class="message-turn" data-message-id="${escape(message.id)}" data-timeline-at="${message.created_at}" data-timeline-order="0">${html}</div>`;
  }
  return `<article class="message ${own ? 'own' : 'agent-message'} ${includeDetails ? '' : 'project-team-message'}" data-message-id="${escape(message.id)}" data-timeline-at="${message.created_at}" data-timeline-order="0">${includeDetails ? '' : `${portrait}<div class="project-team-message-body">`}<div class="message-author"><strong>${own ? '你' : escape(member(message.sender_id)?.name || '成员')}</strong><time>${time(message.created_at)}</time></div>${thoughtHtml}${toolsHtml}${bodyHtml}${delivery}${includeDetails ? '' : '</div>'}</article>`;
}

function projectChatFor(agentId: string) {
  return detail?.project_agent_chats?.find(chat => chat.agent_id === agentId);
}

function bindProjectChatHost(host: HTMLElement, agentId: string, chatId: string) {
  const input=host.querySelector<HTMLTextAreaElement>('[data-project-agent-chat-input]');
  if(input)mountAttachments(input,chatId,escape,message=>toast(message,true));
  if (host.dataset.bound === 'true') return;
  host.dataset.bound = 'true';
  host.addEventListener('input', event => {
    const target = event.target;
    if (target instanceof HTMLTextAreaElement && target.matches('[data-project-agent-chat-input]')) localStorage.setItem(draftKey(chatId), target.value);
  });
  host.addEventListener('submit', event => {
    if (!(event.target instanceof HTMLFormElement) || !event.target.matches('[data-project-agent-chat-form]')) return;
    event.preventDefault();
    void sendProjectAgentChat(host, agentId, chatId);
  });
  host.addEventListener('keydown', event => {
    const target = event.target;
    if (event.key === 'Escape' && target instanceof HTMLTextAreaElement && target.matches('[data-project-agent-chat-input]')) {
      const stop = host.querySelector<HTMLButtonElement>('[data-stop-project-chat-run]');
      if (stop && !stop.disabled) { event.preventDefault(); stop.click(); return; }
    }
    if (target instanceof HTMLTextAreaElement && target.matches('[data-project-agent-chat-input]') && event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      void sendProjectAgentChat(host, agentId, chatId);
    }
  });
  host.addEventListener('click', event => {
    const target = (event.target as HTMLElement).closest<HTMLButtonElement>('[data-project-agent-chat-share]');
    if (!target) return;
    target.disabled = true;
    void invoke('share_project_agent_message', { groupId: detail?.conversation.id, agentChatId: chatId, messageId: target.dataset.projectAgentChatShare })
      .then(async () => {
        if (detail) detail = await invoke<ConversationDetail>('get_conversation', { id: detail.conversation.id });
        target.textContent = '已分享到团队';
        updateRuntimeControls();
      })
      .catch(error => { target.disabled = false; toast(errorText(error), true); });
  });
  host.addEventListener('click', event => {
    const target = (event.target as HTMLElement).closest<HTMLButtonElement>('[data-stop-project-agent]');
    const workflowId = target?.dataset.stopProjectWorkflow;
    if (!target || !workflowId) return;
    target.disabled = true;
    void invoke<Workflow>('cancel_project_agent', { workflowId, agentId })
      .then(adoptProject)
      .catch(error => { target.disabled = false; toast(errorText(error), true); });
  });
  host.addEventListener('click', event => {
    if ((event.target as HTMLElement).closest('[data-open-cli]')) void openInCli(agentId, chatId);
  });
  host.addEventListener('click', event => {
    const target = (event.target as HTMLElement).closest<HTMLButtonElement>('[data-stop-project-chat-run]');
    if (!target) return;
    target.disabled = true;
    void invoke<RuntimeSnapshot>(`cancel_${harnessKey(agentId)}_run`, { conversationId: chatId })
      .then(snapshot => adoptRuntime(snapshot, agentId))
      .catch(error => { target.disabled = false; toast(errorText(error), true); });
  });
}

let projectTranscript: AgentTranscript | null = null;

function disposeProjectTranscript() {
  projectTranscript?.dispose();
  projectTranscript = null;
}

function syncProjectAgentChat(host: HTMLElement, agentId: string, chat: ConversationDetail) {
  const viewport = host.querySelector<HTMLElement>('.project-agent-chat-messages');
  if (!viewport || !host.isConnected) return;
  if (projectTranscript?.viewport !== viewport) {
    disposeProjectTranscript();
    projectTranscript = new AgentTranscript(viewport, { escape, name: id => member(id).name, empty: agentEmpty(member(agentId).name, escape) });
  }
  projectTranscript!.update(agentTimeline(agentId, chat, roomProjects(), runtimes[agentId]));
  syncChatQuestions();
  const stop = host.querySelector<HTMLButtonElement>('[data-stop-project-chat-run]');
  if (busyIn(runtimes[agentId], chat.conversation.id)) {
    if (!stop) host.querySelector('.project-agent-chat-heading')?.insertAdjacentHTML('beforeend', '<button type="button" class="text-button stop-reply" data-stop-project-chat-run title="停止当前回复（Esc）"><span class="stop-glyph" aria-hidden="true"></span>停止</button>');
  } else stop?.remove();
  // 本会话自己的回复进行中时不锁输入框，Enter 即插话；其他会话或项目任务占用成员时才锁。
  const steering = liveReply(agentId, chat.conversation.id);
  const cli = handedOff(agentId, chat.conversation.id);
  const blocked = cli || (!steering && busyIn(runtimes[agentId], chat.conversation.id)) || runtimes[agentId]?.connection !== 'connected';
  const cliButton = host.querySelector<HTMLButtonElement>('[data-open-cli]');
  if (cliButton) cliButton.textContent = cli ? '收回到同席' : '在 CLI 中打开';
  const input = host.querySelector<HTMLTextAreaElement>('[data-project-agent-chat-input]');
  const send = host.querySelector<HTMLButtonElement>('[data-project-agent-chat-send]');
  if (input) input.disabled = blocked;
  if (send) send.disabled = blocked;
}

async function loadProjectAgentChat(host: HTMLElement, agentId: string) {
  const chat = projectChatFor(agentId);
  if (!chat) return;
  bindProjectChatHost(host, agentId, chat.conversation_id);
  try {
    const saved = await invoke<ConversationDetail>('get_conversation', { id: chat.conversation_id });
    projectAgentChatDetails.set(chat.conversation_id, saved);
    if (!host.isConnected || activeProjectAgent() !== agentId) return;
    const initialLoad = !host.querySelector('.project-agent-chat-heading');
    if (initialLoad) {
      host.innerHTML = agentChatChrome(member(agentId).name, escape, icon('arrow'));
      bindProjectChatHost(host, agentId, chat.conversation_id);
      const input = host.querySelector<HTMLTextAreaElement>('[data-project-agent-chat-input]')!;
      input.value = readDraft(chat.conversation_id);
      resizeComposer(input);
    }
    syncProjectAgentChat(host, agentId, saved);
  } catch (error) { host.innerHTML = `<p class="workflow-error">${escape(errorText(error))}</p>`; }
}

async function refreshProjectAgentChat(chatId: string) {
  try {
    const saved = await invoke<ConversationDetail>('get_conversation', { id: chatId });
    projectAgentChatDetails.set(chatId, saved);
    if (!detail || saved.conversation.kind !== 'direct') return;
    const groupId = detail.conversation.id;
    const refreshedGroup = await invoke<ConversationDetail>('get_conversation', { id: groupId });
    if (selectedId === groupId) { detail = refreshedGroup; projectRoomsByGroup.set(groupId, refreshedGroup.project_agent_chats || []); renderList(); updateRuntimeControls(); }
    const mapping = detail.project_agent_chats?.find(chat => chat.conversation_id === chatId);
    if (mapping && activeProjectAgent() === mapping.agent_id) {
      const host = main.querySelector<HTMLElement>(`.project-agent-chat[data-project-chat-id="${chatId}"]`);
      if (host) {
        if (!host.querySelector('.project-agent-chat-heading')) await loadProjectAgentChat(host, mapping.agent_id);
        else syncProjectAgentChat(host, mapping.agent_id, saved);
      }
    } else if (activeProjectAgent()) {
      const viewing = projectChatFor(activeProjectAgent()!);
      const host = viewing && main.querySelector<HTMLElement>(`.project-agent-chat[data-project-chat-id="${viewing.conversation_id}"]`);
      if (viewing && host) await loadProjectAgentChat(host, viewing.agent_id);
    }
  } catch { /* keep the persisted transcript visible on transient refresh errors */ }
}

async function sendProjectAgentChat(host: HTMLElement, agentId: string, chatId: string) {
  const input = host.querySelector<HTMLTextAreaElement>('[data-project-agent-chat-input]');
  if(attachmentsBusy(chatId)){toast('附件正在保存，请稍后发送',true);return;}
  const text = composeMessage(chatId,input?.value||'');
  if (!text || !detail) return;
  if (runtimes[agentId]?.connection !== 'connected') { toast(`请先连接${member(agentId).name}`, true); return; }
  if (input && liveReply(agentId, chatId)) {
    const outcome = await steerReply(agentId, chatId, input);
    if (outcome === 'resend') return sendProjectAgentChat(host, agentId, chatId);
    if (outcome) {
      const saved = await invoke<ConversationDetail>('get_conversation', { id: chatId });
      projectAgentChatDetails.set(chatId, saved);
      syncProjectAgentChat(host, agentId, saved);
      projectTranscript?.latest();
    }
    return;
  }
  if (busyIn(runtimes[agentId], chatId)) { toast(`${member(agentId).name} 正在私聊回复`, true); return; }
  const button = host.querySelector<HTMLButtonElement>('[data-project-agent-chat-send]');
  if (button) { button.disabled = true; button.textContent = '发送中…'; }
  try {
    await invoke<RunRecord>(`send_${harnessKey(agentId)}_message`, { conversationId: chatId, messageId: crypto.randomUUID(), content: text });
    submittedAttachments(chatId,text);
    if (input?.value.trim() === attachmentText(text)) input.value = '';
    if (readDraft(chatId).trim() === attachmentText(text)) localStorage.removeItem(draftKey(chatId));
    const saved = await invoke<ConversationDetail>('get_conversation', { id: chatId });
    projectAgentChatDetails.set(chatId, saved);
    syncProjectAgentChat(host, agentId, saved);
    projectTranscript?.latest();
  } catch (error) { toast(errorText(error), true); }
  finally { if (button) { button.disabled = false; button.innerHTML = `发送${icon('arrow')}`; } }
}

/// 侧栏“成员与服务”的数字跟着已连接的成员数变，悬停能看到总数。
function updateMemberCount() {
  const pill = document.querySelector<HTMLElement>('#member-count');
  if (!pill) return;
  const connected = agents.filter(agent => runtimes[agent.id]?.connection === 'connected').length;
  pill.textContent = String(connected);
  pill.title = `已连接 ${connected} 位 · 共 ${agents.length} 位成员`;
}

function updateRuntimeControls() {
  updateMemberCount();
  if (detail?.project && activeProjectModule === 'progress') { updateProjectProgress(); return; }
  if (screen !== 'chat' || !detail || detail.conversation.id !== selectedId) return;
  const projectAgentId = activeProjectAgent();
  if (projectAgentId) {
    const snapshot = runtimes[projectAgentId];
    const badge = main.querySelector('#project-agent-connection');
    if (badge) badge.innerHTML = `<span class="status-dot ${snapshot.connection === 'connected' ? '' : 'muted'}"></span>${snapshot.connection === 'connected' ? '已连接' : snapshot.connection === 'connecting' ? '正在连接' : '未连接'}`;
    const connect = main.querySelector<HTMLButtonElement>('#connect-project-agent');
    if (connect) { connect.hidden = snapshot.connection === 'connected'; connect.disabled = snapshot.connection === 'connecting'; connect.textContent = snapshot.connection === 'connecting' ? '正在连接…' : `连接 ${member(projectAgentId).name}`; }
    const chat = projectChatFor(projectAgentId);
    const host = chat && main.querySelector<HTMLElement>(`.project-agent-chat[data-project-chat-id="${chat.conversation_id}"]`);
    const saved = chat && projectAgentChatDetails.get(chat.conversation_id);
    if (host && saved) syncProjectAgentChat(host, projectAgentId, saved);
    return;
  }
  const isNative = nativeChat();
  const selectedRun = runIn(currentRuntime(), selectedId);
  const currentRun = liveRun(selectedRun);
  const badge = main.querySelector('.connection-badge');
  if (badge) badge.innerHTML = `<span class="status-dot ${isNative && currentRuntime().connection === 'connected' ? '' : 'muted'}"></span>${isNative ? connectionLabel() : '待接入'}`;
  main.querySelectorAll('.member-card').forEach((card, index) => {
    const snapshot = runtimes[detail!.conversation.members[index]];
    if (snapshot) card.querySelector('small')!.innerHTML = `<span class="status-dot ${snapshot.connection === 'connected' ? '' : 'muted'}"></span>${snapshot.connection === 'connected' ? '接口已连接' : snapshot.connection === 'connecting' ? '正在连接' : '未连接'} · ${escape(member(detail!.conversation.members[index]).location)}`;
  });
  main.querySelectorAll<HTMLElement>('[data-stack-agent]').forEach(item => item.classList.toggle('online', runtimes[item.dataset.stackAgent!]?.connection === 'connected'));
  const connect = main.querySelector<HTMLButtonElement>(`#connect-${harnessKey()}`);
  if (connect) { connect.hidden = currentRuntime().connection === 'connected'; connect.disabled = currentRuntime().connection === 'connecting'; connect.textContent = currentRuntime().connection === 'connecting' ? '正在连接…' : `连接 ${member(currentAgentId()).name}`; }
  const send = main.querySelector<HTMLButtonElement>(`#send-${harnessKey()}`);
  const steering = liveReply(currentAgentId(), selectedId);
  if (send) {
    send.disabled = detail.conversation.archived || sending || handedOff(currentAgentId(), selectedId) || (!steering && (settingsPending > 0 || runBusy())) || currentRuntime().connection !== 'connected';
    const label = steering ? (NATIVE_STEER.has(currentAgentId()) ? '插话引导' : '停止并发送') : `发送给 ${escape(member(currentAgentId()).name)}`;
    if (send.dataset.label !== label) { send.dataset.label = label; send.innerHTML = `${label}${icon('arrow')}`; }
  }
  const cancel = main.querySelector<HTMLButtonElement>(`#cancel-${harnessKey()}`);
  if (cancel) { cancel.hidden = !currentRun; cancel.disabled = selectedRun?.status === 'cancelling'; cancel.innerHTML = selectedRun?.status === 'cancelling' ? '正在停止…' : '<span class="stop-glyph" aria-hidden="true"></span>停止'; cancel.title = '停止当前回复（Esc）'; }
  const cliButton = main.querySelector<HTMLButtonElement>('#open-cli');
  const cli = handedOff(currentAgentId(), selectedId);
  if (cliButton) cliButton.textContent = cli ? '收回到同席' : '在 CLI 中打开';
  const note = main.querySelector('#runtime-note');
  if (note && isNative && cli) note.textContent = `这条会话已交给 ${member(currentAgentId()).name} 的 CLI，同席暂停使用；关闭 CLI 窗口或点“收回到同席”后继续。`;
  else if (note && isNative) note.textContent = currentRuntime().error || (currentRun ? (selectedRun?.status === 'starting' ? '正在准备原生会话…' : selectedRun?.status === 'cancelling' ? '等待原生停止确认…' : `正在接收 ${member(currentAgentId()).name} 回复… 可直接输入插话${NATIVE_STEER.has(currentAgentId()) ? '引导，不会打断' : '，发送时会先停止当前回复'} · Esc 停止`) : currentRuntime().connection === 'connected' ? (currentAgentId() === 'albion-wsl' ? `${member(currentAgentId()).name} 已连接。` : `${member(currentAgentId()).name} 已连接；原生工具和问答可用。`) : currentRuntime().connection === 'connecting' ? `正在连接 ${member(currentAgentId()).name}…` : `${member(currentAgentId()).name} 未连接；连接后才能发送。`);
  renderLiveSettings();
  updateDiscussionControls();
  updateProjectControls();
  for (const snapshot of Object.values(runtimes)) for (const active of runsOf(snapshot)) {
    if (active.conversation_id !== selectedId || (!liveRun(active) && !detail.messages.some(message => message.id === active.assistant_message_id))) continue;
    const messages = main.querySelector('#messages')!;
    const old = messages.querySelector(`[data-message-id="${active.assistant_message_id}"]`);
    const thoughtScroll = captureThoughtScroll(old as HTMLElement | null);
    const toolState = captureToolState(old);
    const html = renderMessage({ id: active.assistant_message_id, conversation_id: selectedId, sender_id: active.agent_id, content: active.text, thought: active.thought, tool_events: active.tool_events, status: liveRun(active) ? 'streaming' : active.status as Message['status'], created_at: detail.messages.find(message => message.id === active.assistant_message_id)?.created_at || Date.now() }, detail.conversation.kind !== 'group');
    const nearBottom = messages.scrollHeight - messages.scrollTop - messages.clientHeight < 120;
    if (old) old.outerHTML = html;
    else { messages.querySelector('.welcome')?.remove(); messages.insertAdjacentHTML('beforeend', html); }
    const updated = messages.querySelector<HTMLElement>(`[data-message-id="${active.assistant_message_id}"]`);
    // 插话已经挪进这条回复里显示，去掉按时间单独排的那一份。
    for (const id of steerIds(active.thought)) for (const node of messages.querySelectorAll(`[data-message-id="${CSS.escape(id)}"]`)) if (!updated?.contains(node)) node.remove();
    if (updated) restoreThoughtScroll(updated, thoughtScroll);
    if (updated) restoreToolState(updated, toolState);
    if (nearBottom) pinToBottom(messages);
  }
  syncChatQuestions();
}

async function connectAgent(agentId = currentAgentId()) {
  try { adoptRuntime(await invoke<RuntimeSnapshot>(`connect_${harnessKey(agentId)}`), agentId); updateRuntimeControls(); if (screen === 'services') renderServices(); toast(`${member(agentId).name} 原生接口已连接`); }
  catch (error) { toast(errorText(error), true); }
}
async function disconnectAgent(agentId = currentAgentId()) {
  try { adoptRuntime(await invoke<RuntimeSnapshot>(`disconnect_${harnessKey(agentId)}`), agentId); updateRuntimeControls(); if (screen === 'services') renderServices(); toast(`${member(agentId).name} 已断开`); }
  catch (error) { toast(errorText(error), true); }
}

/// 原生支持中途插话的成员：Codex（turn/steer）与 Claude（写入运行中会话）。其他成员先停止当前回复再发送。
const NATIVE_STEER = new Set(['codex-win', 'claude-win']);
const liveReply = (agentId: string, conversationId: string) => {
  const run = runIn(runtimes[agentId], conversationId);
  return liveRun(run) && run!.status !== 'cancelling';
};

/// 回复进行中插话。原生支持的成员不打断、把引导追加进当前回复，返回存好的消息；
/// 其他成员先停止当前回复，等确认停止后返回 'resend'，由调用方按普通消息发送。
async function steerReply(agentId: string, conversationId: string, input: HTMLTextAreaElement): Promise<Message | 'resend' | null> {
  if (attachmentsBusy(conversationId)) { toast('附件正在保存，请稍后发送', true); return null; }
  const content = composeMessage(conversationId, input.value);
  if (!content) { input.focus(); return null; }
  if (NATIVE_STEER.has(agentId)) {
    try {
      const message = await invoke<Message>('steer_agent_message', { agentId: baseAgent(agentId), conversationId, messageId: crypto.randomUUID(), content });
      submittedAttachments(conversationId, content);
      if (input.value.trim() === attachmentText(content)) { input.value = ''; resizeComposer(input); }
      if (readDraft(conversationId).trim() === attachmentText(content)) localStorage.removeItem(draftKey(conversationId));
      toast(`已插话，${member(agentId).name} 会在下一步采纳`);
      return message;
    } catch (error) { toast(errorText(error), true); return null; }
  }
  try { adoptRuntime(await invoke<RuntimeSnapshot>(`cancel_${harnessKey(agentId)}_run`, { conversationId }), agentId); updateRuntimeControls(); }
  catch (error) { toast(errorText(error), true); return null; }
  for (let waited = 0; waited < 30_000 && busyIn(runtimes[agentId], conversationId); waited += 250) await new Promise(resolve => setTimeout(resolve, 250));
  if (busyIn(runtimes[agentId], conversationId)) { toast(`${member(agentId).name} 还没确认停止，稍后再发`, true); return null; }
  return 'resend';
}

async function sendNative() {
  if (detail && nativeChat() && !sending && !detail.conversation.archived && liveReply(currentAgentId(), detail.conversation.id)) {
    const input = main.querySelector<HTMLTextAreaElement>('#message-input')!;
    const conversationId = detail.conversation.id;
    sending = true; updateRuntimeControls();
    let outcome: Message | 'resend' | null = null;
    try { outcome = await steerReply(currentAgentId(), conversationId, input); }
    finally { sending = false; updateRuntimeControls(); }
    if (outcome === 'resend') return sendNative();
    if (outcome && selectedId === conversationId && detail) {
      detail.messages.push(outcome);
      const messages = main.querySelector<HTMLElement>('#messages');
      if (messages && !messages.querySelector(`[data-message-id="${CSS.escape(outcome.id)}"]`)) { messages.insertAdjacentHTML('beforeend', renderMessage(outcome, detail.conversation.kind !== 'group')); pinToBottom(messages); }
    }
    return;
  }
  if (!detail || !nativeChat() || sending || settingsPending > 0 || runBusy() || handedOff(currentAgentId(), selectedId) || detail.conversation.archived || currentRuntime().connection !== 'connected') return;
  const input = main.querySelector<HTMLTextAreaElement>('#message-input')!;
  if(attachmentsBusy(detail.conversation.id)){toast('附件正在保存，请稍后发送',true);return;}
  const content = composeMessage(detail.conversation.id,input.value);
  if (!content) { input.focus(); return; }
  const id = detail.conversation.id;
  if (!pendingSend || pendingSend.conversationId !== id || pendingSend.content !== content) pendingSend = { conversationId: id, messageId: crypto.randomUUID(), content };
  const request = pendingSend;
  sending = true; updateRuntimeControls();
  try {
    await invoke<RunRecord>(`send_${harnessKey()}_message`, request);
    pendingSend = null;
    submittedAttachments(id,content);
    if (readDraft(id).trim() === attachmentText(content)) localStorage.removeItem(draftKey(id));
    await refreshList();
    if (selectedId === id && screen === 'chat') await selectConversation(id);
  } catch (error) { toast(errorText(error), true); }
  finally { sending = false; updateRuntimeControls(); }
}

/// 在新终端里用该成员自己的 CLI 打开这条原生会话（同一个会话库，回到同席也能接着聊）。
/// 交给 CLI 使用中的（会话|成员）。期间同席暂停发送，CLI 窗口关闭后后端自动收回。
const cliHandoffs = new Set<string>();
const handedOff = (agentId: string, conversationId: string | null) => cliHandoffs.has(`${conversationId}|${agentId}`);
function adoptCliHandoffs(list: { conversation_id: string; agent_id: string }[]) {
  const next = new Set(list.map(item => `${item.conversation_id}|${item.agent_id}`));
  cliHandoffs.clear();
  next.forEach(key => cliHandoffs.add(key));
  updateRuntimeControls();
}

/// CLI 关闭、会话回到同席：CLI 里新增的对话已由后端补进聊天记录，重新载入正在看的会话。
function cliReturned(conversationId: string, agentId: string, imported: number) {
  cliHandoffs.delete(`${conversationId}|${agentId}`);
  toast(`${member(agentId).name} 的 CLI 已关闭，会话回到同席${imported ? `，已补上 CLI 里的 ${imported} 条对话` : ''}`);
  void refreshList();
  if (selectedId === conversationId) void selectConversation(conversationId);
  if (projectAgentChatDetails.has(conversationId)) void refreshProjectAgentChat(conversationId);
  updateRuntimeControls();
}

/// 把会话移交给该成员自己的 CLI（同一条原生会话），同席暂停使用；已移交时改为收回。
async function openInCli(agentId: string, conversationId: string) {
  if (handedOff(agentId, conversationId)) {
    if (!confirm(`收回到同席会结束 ${member(agentId).name} 的 CLI 窗口（CLI 里已说的内容都保存在会话里）。继续？`)) return;
    try { await invoke('reclaim_cli_session', { conversationId, agentId: baseAgent(agentId) }); }
    catch (error) { toast(errorText(error), true); }
    return;
  }
  try { await invoke('open_in_cli', { conversationId, agentId: baseAgent(agentId) }); toast(`已交给 ${member(agentId).name} 的 CLI；同席暂停这条会话，关闭 CLI 窗口后自动收回`); }
  catch (error) { toast(errorText(error), true); }
}

async function cancelNative() {
  const agentId = currentAgentId();
  const conversationId = selectedId;
  try { adoptRuntime(await invoke<RuntimeSnapshot>(`cancel_${harnessKey(agentId)}_run`, { conversationId }), agentId); updateRuntimeControls(); }
  catch (error) { toast(errorText(error), true); }
}

function renderDiscussionControls() {
  if (!detail || detail.conversation.kind !== 'group') return;
  const host = main.querySelector('#discussion-controls')!;
  if (detail.project) { host.innerHTML = ''; (host as HTMLElement).hidden = true; return; }
  (host as HTMLElement).hidden = false;
  const options = groupOptions();
  host.innerHTML = `<div class="discussion-options"><span>参与讨论</span>${detail.conversation.members.map(id => `<label><input type="checkbox" data-discussion-member="${escape(id)}" ${options.participants.includes(id) ? 'checked' : ''}>${escape(member(id).name)}</label>`).join('')}<label class="rounds-label">轮次<select id="discussion-rounds" aria-label="讨论轮次">${[1, 2, 3].map(round => `<option value="${round}" ${round === options.rounds ? 'selected' : ''}>${round} 轮</option>`).join('')}</select></label><button type="button" id="connect-discussion" class="text-button">连接参与成员</button></div><div id="discussion-status" class="discussion-status" role="status" aria-live="polite"></div>`;
  const save = () => {
    localStorage.setItem(`hub.discussion.${detail!.conversation.id}`, JSON.stringify({ participants: [...host.querySelectorAll<HTMLInputElement>('[data-discussion-member]:checked')].map(input => input.dataset.discussionMember!), rounds: Number(host.querySelector<HTMLSelectElement>('#discussion-rounds')!.value) }));
    updateDiscussionControls();
  };
  host.querySelectorAll('input,select').forEach(input => input.addEventListener('change', save));
  host.querySelector('#connect-discussion')!.addEventListener('click', () => void connectDiscussion());
}

function updateDiscussionControls() {
  queueMicrotask(paintToolChips);
  if (!detail || detail.conversation.kind !== 'group' || screen !== 'chat') return;
  const host = main.querySelector<HTMLElement>('#discussion-controls');
  const groupSend = main.querySelector<HTMLButtonElement>('#send-discussion');
  if (detail.project) {
    if (host) host.hidden = true;
    const members = detail.conversation.members.filter(agent => agent !== 'albion-wsl');
    const connected = members.every(agent => runtimes[agent]?.connection === 'connected');
    const activeWorkflow = workflowBusy(currentWorkflow());
    const available = members.length > 0;
    if (groupSend) {
      groupSend.textContent = detail.project_questions?.some(question=>question.status==='pending') ? '提交回答' : '发送';
      groupSend.disabled = !available || discussionBusy(currentDiscussion()) || discussionBusy(latestDiscussion) || sending || groupConnecting || settingsPending > 0 || detail.conversation.archived;
    }
    const projectInput = main.querySelector<HTMLTextAreaElement>('#message-input');
    if (projectInput) projectInput.placeholder = activeWorkflow ? '给团队追加消息、补充角色或调整任务方向…' : '描述目标与分工；管家按你的原话协调，未指定模型沿用项目配置…';
    const composerHint = main.querySelector('.composer-bottom > span');
    if (composerHint) composerHint.textContent = activeWorkflow ? 'Enter 发到团队 · Shift + Enter 换行' : 'Enter 发送需求并自动协作 · Shift + Enter 换行';
    const save = main.querySelector<HTMLButtonElement>('#save-message');
    if (save) save.disabled = detail.conversation.archived || sending || activeWorkflow;
    const note = main.querySelector('#runtime-note');
    if (note) note.textContent = activeWorkflow ? '团队会话进行中 · 新消息会发给当前团队，Agent 的推理与工具过程在各自模块。' : available ? '项目群聊 · 发送需求后 Agent 会自动讨论、分工并执行。' : '发送需求后，管家会连接需要工作的项目成员。';
    const badge = main.querySelector('.connection-badge');
    if (badge) badge.innerHTML = `<span class="status-dot ${connected ? '' : 'muted'}"></span>${activeWorkflow ? '项目协作中' : connected ? '项目组已连接' : '请连接项目成员'}`;
    return;
  }
  if (host) host.hidden = false;
  const job = currentDiscussion();
  const busy = discussionBusy(job);
  const options = groupOptions();
  const allReady = options.participants.length >= 2 && options.participants.every(id => runtimes[id]?.connection === 'connected' && !busyIn(runtimes[id], selectedId));
  const anotherBusy = discussionBusy(latestDiscussion) && latestDiscussion?.conversation_id !== selectedId;
  const locked = busy || workflowBusy(currentWorkflow()) || sending || groupConnecting || detail.conversation.archived;
  main.querySelectorAll<HTMLInputElement | HTMLSelectElement>('[data-discussion-member], #discussion-rounds').forEach(input => { input.disabled = locked; });
  const connect = main.querySelector<HTMLButtonElement>('#connect-discussion');
  if (connect) { connect.disabled = locked || options.participants.length < 2; connect.textContent = groupConnecting ? '正在连接…' : allReady ? '参与成员已连接' : '连接参与成员'; }
  const send = groupSend;
  if (send) send.disabled = locked || settingsPending > 0 || !allReady || anotherBusy;
  const stop = main.querySelector<HTMLButtonElement>('#cancel-discussion');
  if (stop) { stop.hidden = !busy; stop.disabled = job?.status === 'cancelling'; stop.textContent = job?.status === 'cancelling' ? '正在停止…' : '停止讨论'; }
  const save = main.querySelector<HTMLButtonElement>('#save-message');
  if (save) save.disabled = locked;
  const status = main.querySelector('#discussion-status');
  const labels: Record<Discussion['status'], string> = { running: '讨论进行中', cancelling: '等待当前成员停止，后续发言已取消', completed: '讨论已完成', interrupted: '讨论已停止', failed: '讨论失败' };
  const active = job?.turns.find(turn => ['starting', 'running', 'cancelling'].includes(turn.status));
  if (status) status.textContent = job ? `${labels[job.status]} · ${job.turns.filter(turn => turn.status === 'completed').length}/${job.participants.length * job.rounds} 次发言${active ? ` · 第 ${active.round} 轮 ${member(active.agent_id).name}` : ''}${job.error ? ` · ${job.error}` : ''}` : anotherBusy ? '另一个群聊正在讨论，请等待或在该群停止。' : '选择二至五位成员，按顺序共享群内发言。' + member('albion-wsl').name + '暂时只参加私聊。';
  const note = main.querySelector('#runtime-note');
  if (note) note.textContent = '真实群聊 · 每位成员使用独立上下文，只接收本群公开记录；当前仅讨论，未执行项目工具。';
  const badge = main.querySelector('.connection-badge');
  if (badge) badge.innerHTML = `<span class="status-dot ${allReady ? '' : 'muted'}"></span>${busy ? '正在讨论' : allReady ? '可开始讨论' : '请连接参与成员'}`;
}

async function connectDiscussion() {
  if (!detail || groupConnecting || discussionBusy(currentDiscussion())) return;
  const participants = groupOptions().participants;
  groupConnecting = true; updateRuntimeControls();
  try { for (const id of participants) if (runtimes[id].connection !== 'connected') await connectAgent(id); }
  finally { groupConnecting = false; updateRuntimeControls(); }
}

async function connectProjectMembers() {
  if (!detail || !detail.project || groupConnecting || workflowBusy(currentWorkflow())) return;
  groupConnecting = true; updateRuntimeControls();
  try {
    for (const id of detail.conversation.members.filter(agent => agent !== 'albion-wsl')) {
      if (runtimes[id]?.connection !== 'connected') await connectAgent(id);
    }
  } finally { groupConnecting = false; updateRuntimeControls(); }
}

async function sendGroupMessage() {
  await settingsQueue;
  if (!detail || detail.conversation.kind !== 'group' || sending || detail.conversation.archived || discussionBusy(currentDiscussion()) || discussionBusy(latestDiscussion)) return;
  const id = detail.conversation.id;
  if(attachmentsBusy(id)){toast('附件正在保存，请稍后发送',true);return;}
  const content = composeMessage(id,main.querySelector<HTMLTextAreaElement>('#message-input')!.value);
  if (!content) return;
  const question=detail.project_questions?.find(question=>question.status==='pending');
  if(question){
    try { await invoke('answer_project_question',{conversationId:detail.conversation.id,questionId:question.id,messageId:crypto.randomUUID(),content});
      submittedAttachments(id,content);localStorage.removeItem(draftKey(detail.conversation.id));await selectConversation(detail.conversation.id);
    } catch(error){toast(errorText(error),true);}return;
  }
  if (detail.project) {
    const activeWorkflow = currentWorkflow();
    if (workflowBusy(activeWorkflow)) {
      sending = true; updateRuntimeControls();
      try {
        await invoke('post_project_team_message', { conversationId: id, content });
        const refreshed = await invoke<ConversationDetail>('get_conversation', { id });
        submittedAttachments(id,content);
        if (selectedId === id) detail = refreshed;
        // 这条路径不重绘整页，发出后要自己清空输入框（用户若已接着输入新内容则保留）。
        const input = main.querySelector<HTMLTextAreaElement>('#message-input');
        if (input && input.value.trim() === attachmentText(content)) { input.value = ''; resizeComposer(input); }
        if (readDraft(id).trim() === attachmentText(content)) localStorage.removeItem(draftKey(id));
      } catch (error) { toast(errorText(error), true); }
      finally { sending = false; updateRuntimeControls(); }
      return;
    }
    if (!pendingProject || pendingProject.conversationId !== id || pendingProject.content !== content) pendingProject = { conversationId: id, messageId: crypto.randomUUID(), content };
    sending = true; updateRuntimeControls();
    try {
      adoptProject(await invoke<Workflow>('start_project', { conversationId: id, messageId: pendingProject.messageId, content }));
      pendingProject = null;
      submittedAttachments(id,content);
      if (readDraft(id).trim() === attachmentText(content)) localStorage.removeItem(draftKey(id));
      await refreshList();
      if (screen === 'chat' && selectedId === id) await selectConversation(id);
    } catch (error) { toast(errorText(error), true); }
    finally { sending = false; updateRuntimeControls(); }
    return;
  }
  const { participants, rounds } = groupOptions();
  if (participants.length < 2 || participants.some(id => runtimes[id].connection !== 'connected' || busyIn(runtimes[id], selectedId))) { toast('请先连接二至五位参与成员，并等待当前回复或项目任务完成', true); return; }
  if (!pendingDiscussion || pendingDiscussion.conversationId !== id || pendingDiscussion.content !== content || pendingDiscussion.rounds !== rounds || JSON.stringify(pendingDiscussion.participants) !== JSON.stringify(participants)) pendingDiscussion = { conversationId: id, messageId: crypto.randomUUID(), content, participants, rounds };
  sending = true; updateRuntimeControls();
  try {
    const job = await invoke<Discussion>('start_discussion', pendingDiscussion);
    adoptDiscussion(job); pendingDiscussion = null;
    submittedAttachments(id,content);
    if (readDraft(id).trim() === attachmentText(content)) localStorage.removeItem(draftKey(id));
    await refreshList();
    if (screen === 'chat' && selectedId === id) await selectConversation(id);
  } catch (error) { toast(errorText(error), true); }
  finally { sending = false; updateRuntimeControls(); }
}

async function cancelDiscussion() {
  const job = currentDiscussion();
  if (!job || !discussionBusy(job)) return;
  try { adoptDiscussion(await invoke<Discussion>('cancel_discussion', { id: job.id })); }
  catch (error) { toast(errorText(error), true); }
}

function syncProjectTeamTimeline() {
  if (!detail?.project || activeProjectAgent()) return;
  const host = main.querySelector<HTMLElement>('#messages');
  if (!host) return;
  const follow = host.scrollHeight - host.scrollTop - host.clientHeight < 100;
  const previousTop = host.scrollTop;
  // 恢复协作时补充的需求同时存为聊天消息和团队消息（source_message_id 指向前者），聊天里已显示就不再重复画。
  const shown = new Set(detail.messages.map(message => message.id));
  for (const message of projectTeamMessages(selectedId, detail.project_team_messages || [], roomProjects())) {
    if (message.kind === 'user' && message.source_message_id && shown.has(message.source_message_id)) continue;
    const old = host.querySelector<HTMLElement>(`[data-team-message-id="${message.id}"]`);
    const question=detail.project_questions?.find(question=>question.id===message.id);
    const paint=message.content+':'+(question?.status||'');
    if (old?.dataset.content === paint) continue;
    let html = projectTeamMessageHtml(message, escape, id => member(id).name, id => avatar(member(id)));
    if(question?.status==='pending')html=html.replace('</div></article>',`<button type="button" class="project-question-answer secondary" data-answer-project-question="${escape(question.id)}">回答 ${escape(member(question.agent_id).name)}</button></div></article>`);
    host.querySelector('.welcome')?.remove();
    if (old) old.outerHTML = html; else host.insertAdjacentHTML('beforeend', html);
    host.querySelector<HTMLElement>(`[data-team-message-id="${message.id}"]`)!.dataset.content = paint;
    host.querySelector(`[data-answer-project-question="${question?.id}"]`)?.addEventListener('click',()=>void answerProjectQuestion(question!.id));
  }
  const entries = [...host.children].filter((node): node is HTMLElement => node instanceof HTMLElement && !!node.dataset.timelineAt);
  const ordered = [...entries].sort((a, b) => Number(a.dataset.timelineAt) - Number(b.dataset.timelineAt) || Number(a.dataset.timelineOrder) - Number(b.dataset.timelineOrder));
  if (ordered.some((node, index) => node !== entries[index])) for (const node of ordered) host.append(node);
  if (follow) host.scrollTop = host.scrollHeight; else host.scrollTop = previousTop;
}

function updateProjectControls() {
  if (screen !== 'chat' || !detail || detail.conversation.id !== selectedId) return;
  if (activeProjectModule === 'progress' && detail.project) { updateProjectProgress(); return; }
  if (activeProjectAgent()) { updateRuntimeControls(); return; }
  const job = currentWorkflow();
  const busy = workflowBusy(job);
  const bind = main.querySelector<HTMLButtonElement>('#bind-project');
  const projectMembers = detail.conversation.members.filter(agent => ['codex-win', 'hermes-win', 'dsh-win', 'claude-win'].includes(agent));
  const eligible = detail.conversation.kind === 'group' && projectMembers.length >= 2;
  if (bind) { bind.disabled = !eligible || busy || discussionBusy(currentDiscussion()) || detail.conversation.archived; bind.textContent = detail.project ? '更换项目目录' : '选择项目目录'; }
  const sessions = main.querySelector<HTMLButtonElement>('[data-project-sessions]');
  if (sessions) { sessions.hidden = !detail.project; sessions.disabled = detail.conversation.archived; }
  const connect = main.querySelector<HTMLButtonElement>('#connect-project-members');
  if (connect) {
    const members = detail.conversation.members.filter(agent => agent !== 'albion-wsl');
    const ready = members.length > 0 && members.every(agent => runtimes[agent]?.connection === 'connected');
    connect.hidden = !detail.project;
    connect.disabled = !detail.project || groupConnecting || busy || discussionBusy(currentDiscussion()) || detail.conversation.archived || ready;
    connect.textContent = groupConnecting ? '正在连接成员…' : ready ? '项目成员已连接' : '连接项目成员';
  }
  const title = main.querySelector('#project-title');
  if (title) title.textContent = detail.project ? `${detail.project.name} · 项目群聊` : eligible ? '项目协作 · 选择目录后在此群聊中直接交流与开发' : '项目协作至少需要两位 Windows 成员';
  const share = main.querySelector<HTMLButtonElement>('#share-project-summary');
  if (share) {
    const project = detail.project;
    const enabled = !!project?.summary_enabled;
    share.hidden = !project;
    share.textContent = enabled ? '开发摘要：已共享' : '开发摘要：未共享';
    share.setAttribute('aria-pressed', enabled ? 'true' : 'false');
    // 任意项目有共享请求在途时都禁用：跨项目点击会覆盖 pending 对象，无法安全回滚。协作运行中仍可撤销。
    share.disabled = !!pendingSummary;
    const companion = member('albion-wsl').name;
    share.title = enabled ? `停止把该项目的开发摘要共享给${companion}（只影响该项目）` : `把该项目的开发摘要共享给${companion}（只影响该项目，从之后的消息开始生效）`;
  }
  const messages = main.querySelector('#messages');
  if (!messages) return;
  syncProjectTeamTimeline();
}

async function showProjectBinding() {
  if (!detail) return;
  try { await bindingDialog(detail.conversation, detail.project, { ...pickerHost, modal, open: openModal, coordinator: detail.project_coordinator || null, saved: async id => { if (selectedId === id && screen === 'chat') await selectConversation(id); toast('项目绑定已更新'); } }); }
  catch (error) { toast(errorText(error), true); }
}

const pickerHost = { escape, name: (id: string) => member(id).name, avatar: (id: string) => avatar(member(id)), connect: (id: string) => connectAgent(id) };
async function showProjectSessions() {
  if (!detail?.project) return;
  try { await projectSessionsDialog(detail, { ...pickerHost, modal, open: openModal, saved: async () => { projectAgentChatDetails.clear(); await selectConversation(selectedId); toast('会话绑定已更新，下一轮任务生效'); } }); }
  catch (error) { toast(errorText(error), true); }
}

async function cancelProject(id: string) {
  try { adoptProject(await invoke<Workflow>('cancel_project', { id })); updateRuntimeControls(); }
  catch (error) { toast(errorText(error), true); }
}

async function continueProject(id: string) {
  const button = main.querySelector<HTMLButtonElement>(`[data-continue-project="${CSS.escape(id)}"]`);
  if (button) { button.disabled = true; button.textContent = '正在恢复原方案…'; }
  try {
    adoptProject(await invoke<Workflow>('continue_project', { workflowId: id, autoExecute: true }));
    updateRuntimeControls();
    toast('已复用原方案，继续未完成任务');
  } catch (error) {
    toast(errorText(error), true);
    if (button?.isConnected) { button.disabled = false; button.textContent = '继续协作（复用原方案）'; }
  }
}

// 共享开关按项目范围生效：同一个项目在其他房间显示同一标记，不同项目之间互不覆盖。
async function toggleProjectSummary() {
  if (screen !== 'chat' || !detail || detail.conversation.id !== selectedId) return;
  const project = detail.project;
  if (!project) return;
  // 同一时刻只允许一个共享开关在途：跨项目的点击直接忽略，pending 对象不会被覆盖。
  if (pendingSummary) return;
  const projectId = project.id;
  const enabled = !project.summary_enabled;
  pendingSummary = { projectId, enabled };
  updateProjectControls();
  try {
    const saved = await invoke<Project>('set_project_summary', { projectId, enabled });
    // 响应可能晚于会话切换：只要当前房间仍绑定同一个项目就接受（也可以切到另一个绑定同项目的房间）。
    if (detail?.project?.id === saved.id) detail.project = saved;
    toast(enabled ? `该项目开发摘要已共享给${member('albion-wsl').name} · 从之后的消息开始生效` : '该项目开发摘要已停止共享');
  } catch (error) { toast(errorText(error), true); }
  finally {
    if (pendingSummary?.projectId === projectId) pendingSummary = null;
    updateProjectControls();
  }
}

function renderChat() {
  disposeProjectTranscript();
  enterView(`chat|${selectedId}|${activeProjectModule}`);
  if (detail?.project && activeProjectModule === 'progress') { renderProjectProgress(); return; }
  const agentId = activeProjectAgent();
  if (agentId) {
    renderProjectAgentRoom(agentId);
    return;
  }
  renderConversationChat();
}

function currentChatQuestions():ChatQuestion[] {
  const agent=activeProjectAgent();
  const privateQuestions=agent ? projectAgentChatDetails.get(projectChatFor(agent)?.conversation_id||'')?.chat_questions || [] : detail?.conversation.kind==='direct' ? detail.chat_questions || [] : [];
  const publicQuestions:ChatQuestion[]=agent ? (detail?.project_questions||[]).filter(q=>q.agent_id===agent&&!roomProjects().some(job=>job.attempts.some(a=>a.id===q.attempt_id&&a.activity==='coordinate'))).map(q=>({id:q.id,conversation_id:q.conversation_id,run_id:'',message_id:q.attempt_id||'',agent_id:q.agent_id,prompt:q.prompt,options:q.options,status:q.status,mode:'project',answer:q.answer,delivery:'sent',error:null})) : [];
  return [...privateQuestions,...publicQuestions];
}
function syncChatQuestions() {
  const questions=currentChatQuestions();
  for(const question of questions) {
    const row=main.querySelector<HTMLElement>(question.mode==='project'?`[data-attempt-id="${question.message_id}"]`:`[data-message-id="${question.message_id}"]`);
    if(!row)continue;
    let slot=row.querySelector<HTMLElement>('.chat-question-slot');
    if(!slot){slot=document.createElement('div');slot.className='chat-question-slot';row.append(slot);}
    const html=questions.filter(q=>q.message_id===question.message_id).map(q=>questionCard(q,escape)).join('');
    if(slot.innerHTML!==html)slot.innerHTML=html;
    if(question.status==='pending'&&question.mode==='native'){const bubble=row.querySelector<HTMLElement>('.bubble');if(bubble?.textContent==='正在等待回复…')bubble.hidden=true;}
  }
  const waiting=questions.some(q=>q.status==='pending');
  const note=main.querySelector<HTMLElement>('#runtime-note,.project-chat-settings-note');
  if(waiting&&note)note.textContent='等待你的回答 · 点击问题下的“回答”选择选项或输入内容';
  if(!waiting&&note?.textContent?.startsWith('等待你的回答'))note.textContent=questions.some(q=>q.delivery==='queued')?'回答已保存 · 等待成员继续':'会话就绪 · 问答已接通';
}
main.addEventListener('click',event=>{
  const button=(event.target as HTMLElement).closest<HTMLButtonElement>('[data-answer-chat-question],[data-cancel-chat-question]');
  if(!button)return;
  const id=button.dataset.answerChatQuestion||button.dataset.cancelChatQuestion!;
  const question=currentChatQuestions().find(q=>q.id===id);if(!question)return;
  if(button.dataset.cancelChatQuestion){void invoke('cancel_chat_question',{conversationId:question.conversation_id,questionId:id}).then(()=>refreshQuestionState()).catch(error=>toast(errorText(error),true));return;}
  if(question.mode==='project'){void answerProjectQuestion(question.id);return;}
  void answerDialog(question,{modal,escape,open:openModal,name:id=>member(id).name,saved:refreshQuestionState});
});
let questionRefreshBusy=false;
async function refreshQuestionState() {
  if(questionRefreshBusy)return;questionRefreshBusy=true;
  try {
    const result=await invoke<PendingChatQuestion[]|null>('pending_chat_questions');
    const latest=Array.isArray(result)?result:[];
    const changed=JSON.stringify(latest)!==JSON.stringify(pendingQuestions);pendingQuestions=latest;
    if(changed)renderList();
    const unseen=latest.find(q=>!noticedQuestions.has(q.id));
    if(unseen){noticedQuestions.add(unseen.id);toast(`${member(unseen.agent_id).name} 等待你的回答；请打开对应会话点击“回答”`);}
    let notice=document.querySelector<HTMLButtonElement>('#question-notice');
    if(latest.length&&!notice){notice=document.createElement('button');notice.id='question-notice';notice.className='question-notice';document.querySelector('.sidebar')!.append(notice);notice.addEventListener('click',()=>{const q=pendingQuestions[0];if(q)void(q.group_id?selectProjectModule(q.group_id,q.kind==='coordination'?'team':q.agent_id):selectConversation(q.conversation_id));});}
    if(notice){notice.hidden=!latest.length;notice.textContent=latest.length?`${member(latest[0].agent_id).name} 等待回答 · ${latest.length}`:'';}
    if(screen==='chat'&&detail) {
      const agent=activeProjectAgent(),chat=agent?projectChatFor(agent):null;
      const id=chat?.conversation_id || (detail.conversation.kind==='direct'?detail.conversation.id:null);
      if(id){const fresh=await invoke<ConversationDetail>('get_conversation',{id});if(chat)projectAgentChatDetails.set(id,fresh);else if(selectedId===id)detail=fresh;syncChatQuestions();}
    }
  }catch{/* A stopped application or an older mock has no question endpoint. */}
  finally{questionRefreshBusy=false;}
}
setInterval(()=>void refreshQuestionState(),1000);

async function answerProjectQuestion(id:string) {
  const question=detail?.project_questions?.find(question=>question.id===id&&question.status==='pending');
  if(!question||!detail)return;
  openModal(`${member(question.agent_id).name} 的问题`,question.prompt,`<form id="project-answer-form">${question.options.length?`<label class="field">选择<select id="project-answer-option"><option value="">自行输入回答</option>${question.options.map(option=>`<option value="${escape(option)}">${escape(option)}</option>`).join('')}</select></label>`:''}<label class="field">回答<textarea id="project-answer-text" rows="3" required></textarea></label><p class="form-error"></p><div class="modal-footer"><button class="primary">提交回答并继续</button></div></form>`);
  const form=modal.querySelector<HTMLFormElement>('#project-answer-form')!;
  form.querySelector<HTMLSelectElement>('#project-answer-option')?.addEventListener('change',event=>{form.querySelector<HTMLTextAreaElement>('#project-answer-text')!.value=(event.target as HTMLSelectElement).value;});
  form.addEventListener('submit',async event=>{event.preventDefault();try{await invoke('answer_project_question',{conversationId:question.conversation_id,questionId:id,messageId:crypto.randomUUID(),content:form.querySelector<HTMLTextAreaElement>('#project-answer-text')!.value});modal.close();await selectConversation(question.conversation_id);}catch(error){form.querySelector('.form-error')!.textContent=errorText(error);}});
}

let projectRefreshBusy=false;
setInterval(()=>{
  if(projectRefreshBusy||screen!=='chat'||!detail?.project||!roomProjects().some(workflowBusy))return;
  const group=selectedId;projectRefreshBusy=true;
  void invoke<ConversationDetail>('get_conversation',{id:group}).then(fresh=>{if(selectedId===group&&screen==='chat'){detail=fresh;updateRuntimeControls();syncProjectTeamTimeline();}}).catch(()=>{}).finally(()=>{projectRefreshBusy=false;});
},1000);

async function configureProjectMembers() {
  if (!detail?.project) return;
  try { await projectSettings(detail,{escape,modal,open:openModal,snapshot:id=>runtimes[id],name:id=>member(id).name,saved:async()=>{projectAgentChatDetails.clear();await selectConversation(selectedId);toast('项目成员配置已保存');},reopen:async()=>{projectAgentChatDetails.clear();await selectConversation(selectedId);await configureProjectMembers();},renamed:(seat,label)=>{seatLabels.set(seat,label);renderList();toast(`席位已改名为「${label}」`);}}); }
  catch(error){toast(errorText(error),true);}
}

function renderProjectProgress() {
  if (!detail?.project) return;
  main.innerHTML = projectProgressPage(detail.project.name, '', escape);
  main.querySelector('[data-project-return-team]')?.addEventListener('click',()=>void selectProjectModule(selectedId,'team'));
  main.querySelector('[data-project-configure]')?.addEventListener('click',()=>void configureProjectMembers());
  main.querySelector('[data-project-sessions]')?.addEventListener('click',()=>void showProjectSessions());
  updateProjectProgress();
}

function updateProjectProgress() {
  if (!detail?.project || activeProjectModule !== 'progress') return;
  const content = main.querySelector<HTMLElement>('.project-progress-content');
  if (!content) return;
  const jobs=roomProjects(), stamp=JSON.stringify([detail.project_questions,jobs.map(job=>[job.id,job.status,job.coordination,job.tasks.map(task=>[task.status,task.title,task.agent_id]),job.attempts.map(attempt=>[attempt.agent_id,attempt.status,attempt.stage,attempt.activity]),job.error])]);
  if (content.dataset.paint===stamp) return;
  content.innerHTML=progressContent(jobs,detail.project_agent_chats,escape,id=>member(id).name,id=>avatar(member(id)),detail.project_questions||[]);content.dataset.paint=stamp;
  content.querySelectorAll<HTMLButtonElement>('[data-progress-agent]').forEach(button=>button.addEventListener('click',()=>void selectProjectModule(selectedId,button.dataset.progressAgent!)));
  content.querySelectorAll<HTMLButtonElement>('[data-stop-project]').forEach(button=>button.addEventListener('click',()=>void cancelProject(button.dataset.stopProject!)));
  content.querySelectorAll<HTMLButtonElement>('[data-continue-project]').forEach(button=>button.addEventListener('click',()=>void continueProject(button.dataset.continueProject!)));
}

function renderProjectAgentRoom(agentId: string) {
  if (!detail) return;
  const agent = member(agentId);
  const chat = projectChatFor(agentId);
  if (!chat) return;
  main.innerHTML = projectAgentPage(agent.name, avatar(agent), detail.project?.name || detail.conversation.title, agentId, chat.conversation_id, escape);
  main.querySelector('#project-team-return')?.addEventListener('click', () => void selectProjectModule(detail!.conversation.id, 'team'));
  main.querySelector('#connect-project-agent')?.addEventListener('click', () => void connectAgent(agentId));
  const host = main.querySelector<HTMLElement>('.project-agent-chat')!;
  void loadProjectAgentChat(host, agentId);
  updateRuntimeControls();
}

function renderConversationChat() {
  if (!detail) {
    const companion = member('albion-wsl');
    main.innerHTML = `<div class="no-selection">${portrait(companion) ? `<div class="albion-hero" style="background-image:url('${portrait(companion)}')" aria-hidden="true"></div>` : ''}<span class="eyebrow">同席 · 本地协作</span><h1>为下一个想法，留一个位置。</h1><p>选择一个会话，或创建新的私聊与群聊。</p><button class="primary" id="empty-new">${icon('plus')}新建会话</button></div>`;
    main.querySelector('#empty-new')!.addEventListener('click', () => showCreate());
    return;
  }
  const conversation = detail.conversation;
  const roster = conversation.members.map(member);
  const isGroup = conversation.kind === 'group';
  const directAgent = conversation.kind === 'direct' ? roster[0] : null;
  const isNative = nativeChat();
  const key = harnessKey();
  // 回复中途的插话跟着那条回复显示，不再按自己的时间单独排。
  const steered = new Set([...detail.messages.map(message => message.thought), ...Object.values(runtimes).flatMap(snapshot => runsOf(snapshot).map(run => run.thought))].flatMap(steerIds));
  const name = directAgent?.name || ''; 
  main.innerHTML = `<header class="chat-header"><div class="chat-heading">${directAgent ? avatar(directAgent) : `<span class="avatar group-avatar">${icon('group')}</span>`}<div><h1>${escape(conversation.title)}</h1><p>${escape(directAgent ? directAgent.subtitle : `${roster.length} 位成员 · 共同讨论，分别思考`)}${conversation.archived ? ' · 已归档' : ''}</p></div></div><div class="header-actions"><span class="connection-badge"><span class="status-dot muted"></span>待接入</span>${conversation.kind === 'direct' ? `<button id="native-sessions" class="secondary" title="查看该成员自己的历史会话（只读，不会改动它）">原生会话</button>` : ''}${conversation.kind === 'direct' && clientAgents.includes(conversation.members[0]) ? `<button id="open-client" class="secondary" title="打开这位成员自己的客户端">打开客户端</button>` : ''}<button id="toggle-members" class="member-stack" aria-pressed="${membersOpen}" title="${membersOpen ? '收起成员面板' : '展开成员面板'}">${roster.slice(0, 5).map(agent => `<span class="stack-item" data-stack-agent="${escape(agent.id)}">${avatar(agent)}<i class="stack-dot"></i></span>`).join('')}${icon('panel', 'stack-toggle')}</button><button id="conversation-menu" class="icon-button" aria-label="会话操作" title="会话操作">${icon('more')}</button></div></header>
    <div class="chat-layout${membersOpen ? ' members-open' : ''}"><section class="chat-content"><div class="milestone-note"><span id="runtime-note">四位成员真实接入，私聊与群聊使用独立上下文。</span>${isNative ? `<button id="connect-${key}" class="secondary">连接 ${escape(name)}</button>` : ''}${isNative && key !== 'albion' ? `<button id="open-cli" type="button" class="secondary" title="在新终端里用 ${escape(name)} 自己的 CLI 续上这条原生会话">在 CLI 中打开</button>` : ''}</div>
      <div class="chat-tools" id="chat-tools"><button type="button" class="tool-chip" id="live-summary" data-tool-toggle="live-settings" aria-expanded="false" title="模型与思考强度"></button>${isGroup ? '<button type="button" class="tool-chip" id="discussion-summary" data-tool-toggle="discussion-controls" aria-expanded="false" title="参与讨论的成员与轮次"></button>' : ''}<div id="live-settings" class="live-settings tool-pop" aria-label="当前会话模型与思考强度"></div>${isGroup ? '<div id="discussion-controls" class="discussion-controls tool-pop" aria-label="群聊设置"></div>' : ''}</div>${isGroup ? '<div id="project-controls" class="project-controls"><div class="project-entry-copy"><strong id="project-title">项目协作 · 先绑定项目目录</strong><small>绑定后，直接在本群讨论需求或推动实现。</small></div><div class="project-entry-actions"><button id="bind-project" type="button" class="secondary">选择项目目录</button><button data-project-configure class="secondary">项目成员配置</button><button data-project-sessions class="secondary" hidden>会话绑定</button><button id="connect-project-members" type="button" class="secondary" hidden>连接项目成员</button><button id="share-project-summary" type="button" class="text-button" aria-pressed="false" hidden>开发摘要：未共享</button></div></div>' : ''}<div id="messages" class="messages" aria-label="聊天消息">
      ${detail.messages.length ? `<div class="date-divider">${date(detail.messages[0].created_at)}</div>${detail.messages.filter(message => !steered.has(message.id)).map(message => renderMessage(message, !isGroup)).join('')}` : `<div class="welcome">${directAgent && portrait(directAgent) ? `<div class="albion-hero small" style="background-image:url('${portrait(directAgent)}')" aria-hidden="true"></div>` : `<div class="seat-illustration"><span></span><span></span><span></span><span></span><div>${icon(directAgent ? 'chat' : 'group')}</div></div>`}<span class="eyebrow">${directAgent ? '留一个安静的对话空间' : '把想法带到同一张桌上'}</span><h2>${directAgent ? (directAgent.id === 'albion-wsl' ? '聊聊今天，也聊聊正在做的事。' : `从一次与 ${escape(directAgent.name)} 的对话开始。`) : '一个问题，几种视角。'}</h2><p>${directAgent ? escape(directAgent.role) : '让管家梳理需求，让实现者把方案变成结果。<br>会话与成员已经分开，新的讨论从这里开始。'}</p><div class="suggestions">${(directAgent?.id === 'albion-wsl' ? ['记录今天的开发进展', '聊聊今天发生的事情'] : ['梳理一个新的开发需求', '记录一个待解决的问题', '整理接下来的计划']).map(text => `<button data-suggestion="${escape(text)}">${escape(text)}${icon('arrow')}</button>`).join('')}</div></div>`}
      </div>
      <form id="composer" class="composer"><span class="hud-grip" data-hud-grip title="按住拖动 HUD" aria-hidden="true">⠿</span><div class="composer-top"><span>${conversation.archived ? '会话已归档' : isNative ? `${name} 私聊 · 原生工具已接入` : detail.project ? '项目群聊 · 讨论与开发共用同一空间' : '群聊讨论'}</span><span>${roster.map(agent => escape(agent.name)).join(' · ')}</span></div><textarea id="message-input" maxlength="16000" rows="1" aria-label="消息内容" placeholder="${conversation.archived ? '恢复会话后，可以继续记录。' : detail.project ? '描述目标、讨论方案，或让项目组推进实现…' : '写下你的需求、问题，或今天的想法…'}" ${conversation.archived ? 'disabled' : ''}></textarea><div class="composer-bottom"><span>${conversation.archived ? '归档会保留全部历史' : isNative ? '连接后 Enter 发送 · Shift + Enter 换行' : detail.project ? 'Enter 发送给项目组 · Shift + Enter 换行' : 'Enter 开始群聊 · Shift + Enter 换行'}</span><div class="composer-actions"><button id="hud-model" type="button" class="icon-button" title="模型与思考强度" aria-label="模型与思考强度">⌄</button><button id="hud-exit" data-hud-exit type="button" class="icon-button" aria-label="退出 HUD" title="退出 HUD（回到主窗口）">×</button>${isNative ? `<button id="cancel-${key}" type="button" class="secondary stop-reply" hidden title="停止当前回复（Esc）"><span class="stop-glyph" aria-hidden="true"></span>停止</button>` : `<button id="cancel-discussion" type="button" class="secondary" hidden>停止讨论</button>`}<button id="save-message" type="button" class="secondary" ${conversation.archived ? 'disabled' : ''}>保存草稿</button>${isNative ? `<button id="send-${key}" class="primary" disabled>发送给 ${escape(name)}${icon('arrow')}</button>` : `<button id="send-discussion" class="primary" disabled>${detail.project ? '发送给项目组' : '开始群聊'}${icon('arrow')}</button>`}</div></div></form>
      <footer class="chat-footer">本地保存，独立会话。你的私聊不会自动进入群聊。</footer>
    </section><aside class="members-panel"><div class="panel-title"><span>会话成员</span>${conversation.kind === 'group' ? `<button class="text-button" id="edit-members">管理</button>` : ''}</div><div class="member-list">${roster.map(agent => `<div class="member-card">${avatar(agent)}<div><strong>${escape(agent.name)}</strong><span>${escape(agent.subtitle)}</span><small><span class="status-dot muted"></span>待接入 · ${escape(agent.location)}</small></div></div>`).join('')}</div><p class="members-note">每位成员使用独立的会话映射，加入群聊不会合并已有私聊。</p></aside></div>`;
  renderDiscussionControls();
  main.querySelector('[data-project-configure]')?.addEventListener('click', () => void configureProjectMembers());
  main.querySelector('[data-project-sessions]')?.addEventListener('click', () => void showProjectSessions());
  main.querySelector('#bind-project')?.addEventListener('click', () => void showProjectBinding());
  main.querySelector('#connect-project-members')?.addEventListener('click', () => void connectProjectMembers());
  main.querySelector('#share-project-summary')?.addEventListener('click', () => void toggleProjectSummary());
  main.querySelector('#cancel-discussion')?.addEventListener('click', () => void cancelDiscussion());
  // HUD 里模型/强度收成一个小小的 ⌄（像 Hermes 那样），点开仍是同一套设置。
  main.querySelector('#hud-model')?.addEventListener('click', () => showModelSettings());
  main.querySelector('#native-sessions')?.addEventListener('click', () => void showNativeSessions(conversation.members[0]));
  main.querySelector('#open-client')?.addEventListener('click', () => void openClient(conversation.members[0]));
  document.querySelector('#conversation-menu')!.addEventListener('click', () => showConversationActions());
  const heading = document.querySelector<HTMLElement>('.chat-heading h1');
  if (heading && !conversation.archived) {
    heading.title = '双击重命名';
    heading.addEventListener('dblclick', () => startInlineRename(conversation.id, heading));
  }
  document.querySelector('#edit-members')?.addEventListener('click', () => showMembers());
  wireToolChips();
  main.querySelector('#toggle-members')?.addEventListener('click', event => {
    membersOpen = !membersOpen;
    localStorage.setItem('hub.membersPanel', membersOpen ? '1' : '0');
    main.querySelector('.chat-layout')?.classList.toggle('members-open', membersOpen);
    const button = event.currentTarget as HTMLButtonElement;
    button.setAttribute('aria-pressed', String(membersOpen)); button.title = membersOpen ? '收起成员面板' : '展开成员面板';
  });
  main.querySelector(`#connect-${key}`)?.addEventListener('click', () => void connectAgent());
  main.querySelector(`#cancel-${key}`)?.addEventListener('click', () => void cancelNative());
  main.querySelector('#open-cli')?.addEventListener('click', () => void openInCli(currentAgentId(), conversation.id));
  main.querySelector('#save-message')!.addEventListener('click', () => void saveMessage());
  const input = document.querySelector<HTMLTextAreaElement>('#message-input')!;
  mountAttachments(input,conversation.id,escape,message=>toast(message,true));
  input.value = readDraft(conversation.id);
  input.addEventListener('input', () => localStorage.setItem(draftKey(conversation.id), input.value));
  const submit = () => isGroup ? sendGroupMessage() : nativeChat() && currentRuntime().connection === 'connected' ? sendNative() : saveMessage();
  // IME confirmation must never send: track composition state for this textarea only.
  let composing = false;
  let composeGuardUntil = 0;
  input.addEventListener('compositionstart', () => { composing = true; });
  input.addEventListener('compositionend', () => { composing = false; composeGuardUntil = Date.now() + 50; });
  input.addEventListener('keydown', event => {
    // 像各 CLI 一样，回复进行中按 Esc 停止当前回复。
    if (event.key === 'Escape' && nativeChat() && runBusy() && !composing) { event.preventDefault(); void cancelNative(); return; }
    if (event.key !== 'Enter') return;
    if (event.isComposing || event.keyCode === 229 || composing || Date.now() < composeGuardUntil) return;
    if (event.ctrlKey || event.altKey || event.metaKey || event.shiftKey) return;
    event.preventDefault();
    void submit();
  });
  document.querySelector('#composer')!.addEventListener('submit', event => { event.preventDefault(); void submit(); });
  document.querySelectorAll<HTMLButtonElement>('[data-suggestion]').forEach(button => button.addEventListener('click', () => { input.value = button.dataset.suggestion!; localStorage.setItem(draftKey(conversation.id), input.value); input.focus(); }));
  pinToBottom(document.querySelector('#messages'));
  resizeComposer(input);
  updateRuntimeControls();
}

async function saveMessage() {
  if (!detail || sending || detail.conversation.archived) return;
  const input = document.querySelector<HTMLTextAreaElement>('#message-input')!;
  if(attachmentsBusy(detail.conversation.id)){toast('附件正在保存，请稍后发送',true);return;}
  const content = composeMessage(detail.conversation.id,input.value);
  if (!content) { input.focus(); return; }
  const id = detail.conversation.id;
  if (!pendingSave || pendingSave.conversationId !== id || pendingSave.content !== content) pendingSave = { conversationId: id, messageId: crypto.randomUUID(), content };
  const request = pendingSave;
  const button = document.querySelector<HTMLButtonElement>('#save-message')!;
  sending = true; button.disabled = true; button.textContent = '保存中…';
  try {
    await invoke<Message>('save_local_message', { ...request });
    pendingSave = null;
    submittedAttachments(id,content);
    // A switch during persistence must not clear another room's text or repaint it.
    if (readDraft(id).trim() === attachmentText(content)) localStorage.removeItem(draftKey(id));
    await refreshList();
    if (selectedId === id && screen === 'chat') await selectConversation(id);
    toast('草稿已保存到本机，尚未发送给成员');
  } catch (error) { toast(errorText(error), true); }
  finally {
    sending = false;
    if (button.isConnected) { button.disabled = false; button.innerHTML = `保存草稿${icon('arrow')}`; }
  }
}

/// 能一键打开的客户端：Hermes 桌面端、codex / claude 命令行、DSH 网页端（伴侣没有客户端）。
const clientAgents = ['hermes-win', 'codex-win', 'dsh-win', 'claude-win'];

/// 打开成员自己的客户端。
async function openClient(agentId: string) {
  const agent = member(agentId);
  const button = main.querySelector<HTMLButtonElement>('#open-client');
  if (button) { button.disabled = true; button.textContent = '正在打开…'; }
  try {
    toast(await invoke<string>('open_agent_client', { agentId }));
  } catch (error) {
    toast(`${agent.name}：${errorText(error)}`);
  } finally {
    if (button) { button.disabled = false; button.textContent = '打开客户端'; }
  }
}

/// 列出成员自己的历史会话，并允许把某一个接进当前私聊（一次只能挂一处）。
async function showNativeSessions(agentId: string) {
  const agent = member(agentId);
  const conversation = detail!.conversation;
  openModal(`${agent.name} · 原生会话`, '接入后，发给这位成员的消息就续在这个原生会话里；已有的同席聊天记录保留不动', '<div class="native-session-list"><p class="model-setting-note">正在读取…</p></div>');

  const render = async () => {
    const host = modal.querySelector('.native-session-list')!;
    try {
      const sessions = await invoke<NativeSession[]>('native_sessions', { agentId, conversationId: conversation.id });
      if (!sessions.length) {
        host.innerHTML = '<p class="model-setting-note">这个成员目前没有可列出的历史会话。它自己的会话记录在它那一边，本软件只读不改。</p>';
        return;
      }
      host.innerHTML = sessions.map(session => {
        const when = session.updated_at ? new Date(session.updated_at).toLocaleString('zh-CN', { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }) : '时间未知';
        const title = session.title?.trim().replace(/\s+/g, ' ') || '未命名会话';
        const action = session.current
          ? '<span class="native-session-state">当前会话</span>'
          : session.occupied_by
            ? `<span class="native-session-state">已被「${escape(session.occupied_by)}」占用</span>`
            : `<button class="secondary" data-attach="${escape(session.id)}" data-native-cwd="${escape(session.cwd || '')}" data-native-title="${escape(session.title?.trim() || '')}">接入</button>`;
        return `<article class="native-session" data-session-id="${escape(session.id)}" title="${escape(session.id)}"><div class="native-session-main"><strong>${escape(title)}</strong><small>${escape(when)}${session.cwd ? ` · ${escape(session.cwd)}` : ''}</small></div>${action}</article>`;
      }).join('');
    } catch (error) {
      host.innerHTML = `<p class="model-setting-note">读取失败：${escape(errorText(error))}</p>`;
    }
  };

  modal.querySelector('.native-session-list')!.addEventListener('click', async event => {
    const button = (event.target as HTMLElement).closest<HTMLButtonElement>('[data-attach]');
    if (!button) return;
    button.disabled = true;
    try {
      const saved = await invoke<ConversationDetail>('attach_native_session', {
        conversationId: conversation.id,
        agentId,
        nativeSessionId: button.dataset.attach,
        nativeCwd: button.dataset.nativeCwd || null,
        title: button.dataset.nativeTitle || null,
      });
      if (detail?.conversation.id === saved.conversation.id) { detail.sessions = saved.sessions; detail.conversation = saved.conversation; }
      await refreshList();
      main.querySelectorAll('.chat-heading h1').forEach(node => { node.textContent = saved.conversation.title; });
      renderLiveSettings(false);
      toast(`已接入，下一条给${agent.name}的消息会续在这个原生会话里`);
      await render();
    } catch (error) {
      button.disabled = false;
      toast(errorText(error), true);
    }
  });

  await render();
}

function openModal(title: string, subtitle: string, body: string) {
  modal.innerHTML = `<div class="modal-heading"><div><h2>${escape(title)}</h2><p>${escape(subtitle)}</p></div><button id="modal-close" class="icon-button" aria-label="关闭对话框">${icon('close')}</button></div>${body}`;
  modal.querySelector('#modal-close')!.addEventListener('click', () => modal.close());
  modal.showModal();
}

function memberOptions(selected: string[], type: 'checkbox' | 'radio') {
  return agents.map(agent => `<label class="member-option"><input type="${type}" name="member" value="${escape(agent.id)}" ${selected.includes(agent.id) ? 'checked' : ''}>${avatar(agent)}<span><strong>${escape(agent.name)}</strong><small>${escape(agent.subtitle)} · ${escape(agent.location)}</small></span></label>`).join('');
}

function renderLiveSettings(force = false) {
  try { renderLiveSettingsInner(force); } finally { paintToolChips(); }
}

/// 模型/思考强度、参与成员/轮次平时收成两颗摘要胶囊，点开才是完整设置（浮层）。
function paintToolChips() {
  const live = main.querySelector<HTMLElement>('#live-settings'), liveChip = main.querySelector<HTMLButtonElement>('#live-summary');
  if (live && liveChip) {
    liveChip.hidden = live.hidden;
    const picked = (selector: string) => live.querySelector<HTMLSelectElement>(selector)?.selectedOptions[0]?.textContent?.trim() || '';
    const model = picked('#live-model').replace(/^默认\s*·\s*/, '默认 · ') || (runtimes[settingsAgentId]?.connection === 'connected' ? '默认模型' : '未连接');
    const effort = picked('#live-effort') || '默认';
    const who = detail?.conversation.kind === 'group' ? `${member(settingsAgentId).name} · ` : '';
    liveChip.innerHTML = `<span class="chip-label">模型</span><span class="chip-value">${escape(`${who}${model.length > 30 ? `${model.slice(0, 29)}…` : model} · ${effort}`)}</span><span class="chip-caret" aria-hidden="true">⌄</span>`;
  }
  const discussion = main.querySelector<HTMLElement>('#discussion-controls'), discussionChip = main.querySelector<HTMLButtonElement>('#discussion-summary');
  if (discussion && discussionChip) {
    discussionChip.hidden = discussion.hidden;
    const count = discussion.querySelectorAll('[data-discussion-member]:checked').length;
    const rounds = discussion.querySelector<HTMLSelectElement>('#discussion-rounds')?.selectedOptions[0]?.textContent?.trim() || '1 轮';
    const live = discussionBusy(currentDiscussion()) ? ' · 讨论中' : '';
    discussionChip.innerHTML = `<span class="chip-label">群聊</span><span class="chip-value">${escape(`${count} 人参与 · ${rounds}${live}`)}</span><span class="chip-caret" aria-hidden="true">⌄</span>`;
  }
}

function renderLiveSettingsInner(force: boolean) {
  const host = main.querySelector<HTMLDivElement>('#live-settings');
  if (!host || !detail) return;
  const conversation = detail.conversation;
  if (conversation.kind === 'group' && detail.project) { host.hidden = true; host.innerHTML = ''; return; }
  host.hidden = false;
  const autoProjectSetting = conversation.kind === 'group' && !!detail.project;
  if (!conversation.members.includes(settingsAgentId)) settingsAgentId = conversation.members[0];
  const autoProjectModel = autoProjectSetting && settingsAgentId === 'codex-win';
  const session = detail.sessions.find(item => item.agent_id === settingsAgentId)!;
  const snapshot = runtimes[settingsAgentId];
  const connected = snapshot?.connection === 'connected';
  const catalog = connected ? snapshot.models : [];
  const signature = JSON.stringify([conversation.id, settingsAgentId, session.model, session.reasoning_effort, catalog, snapshot?.default_model, conversation.archived, autoProjectSetting, autoProjectModel]);
  const status = host.querySelector('#live-setting-status');
  if (status) status.textContent = settingsPending ? '正在保存…' : busyIn(snapshot, conversation.id) ? '下一条生效 · 当前回复保留原参数' : snapshot?.connection === 'connected' ? '切换即保存 · 下一条消息生效' : '切换即保存 · 连接后应用';
  if (!force && (host.dataset.signature === signature || settingsPending || host.contains(document.activeElement))) return;
  host.dataset.signature = signature;
  const selectedModel = session.model || snapshot?.default_model;
  const providers = modelProviders(catalog);
  const providerKey = `${conversation.id}:${settingsAgentId}`;
  const provider = liveProviderSelection[providerKey] || selectedProvider(catalog, session.model || snapshot?.default_model);
  const providerHtml = providers.length > 1 ? `<label>提供商<select id="live-provider" aria-label="当前会话模型提供商">${providerOptionsHtml(catalog, provider, escape)}</select></label>` : '';
  const modelHtml = `<select id="live-model" aria-label="当前会话模型">${modelOptionsHtml(catalog, provider, session.model, escape, autoProjectModel ? '项目自动选型 · 聊天默认' : `默认${snapshot?.default_model ? ` · ${catalog.find(item => item.id === snapshot.default_model)?.name || snapshot.default_model}` : ''}`)}${session.model && !catalog.some(item => item.id === session.model) ? `<option value="${escape(session.model)}" selected>${escape(session.model)} · 待验证</option>` : ''}</select>`;
  const efforts = catalog.find(item => item.id === selectedModel)?.efforts || Object.keys(effortLabels);
  host.innerHTML = `${conversation.kind === 'group' ? `<label>成员<select id="live-agent" aria-label="设置成员">${conversation.members.map(id => `<option value="${escape(id)}" ${id === settingsAgentId ? 'selected' : ''}>${escape(member(id).name)}</option>`).join('')}</select></label>` : ''}${providerHtml}<label class="live-model-field">模型${modelHtml}</label><label>思考<select id="live-effort" aria-label="当前会话思考强度"><option value="">${autoProjectSetting ? '自动按任务 · 聊天默认' : '默认'}</option>${efforts.map(effort => `<option value="${escape(effort)}" ${session.reasoning_effort === effort ? 'selected' : ''}>${escape(effortLabels[effort] || effort)}</option>`).join('')}</select></label><button type="button" id="live-reset" class="text-button">恢复默认</button>${connected ? '' : '<button type="button" id="live-connect" class="text-button">连接并读取模型</button>'}<span id="live-setting-status" role="status"></span>`;
  host.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLButtonElement>('input, select, button').forEach(control => { control.disabled = conversation.archived; });
  host.querySelector('#live-agent')?.addEventListener('change', event => { settingsAgentId = (event.target as HTMLSelectElement).value; renderLiveSettings(true); });
  host.querySelector('#live-connect')?.addEventListener('click', async event => {
    // 模型目录要连接该成员才读得到；连上后重渲染，模型下拉就有选项了。
    const button = event.currentTarget as HTMLButtonElement;
    button.disabled = true;
    button.textContent = '连接中…';
    await connectAgent(settingsAgentId);
    renderLiveSettings(true);
  });
  const model = host.querySelector<HTMLInputElement | HTMLSelectElement>('#live-model')!;
  const providerSelect = host.querySelector<HTMLSelectElement>('#live-provider');
  const effort = host.querySelector<HTMLSelectElement>('#live-effort')!;
  const queueSave = () => {
    const id = conversation.id, agentId = settingsAgentId;
    const nextModel = model.value.trim() || null, nextEffort = effort.value || null;
    settingsPending++;
    updateRuntimeControls();
    settingsQueue = settingsQueue.then(async () => {
      let failed = false;
      try {
        const saved = await invoke<ConversationDetail>('set_session_settings', { id, agentId: baseAgent(agentId), model: nextModel, reasoningEffort: nextEffort });
        if (detail?.conversation.id === id) detail.sessions = saved.sessions;
      } catch (error) { failed = true; toast(errorText(error), true); }
      finally {
        settingsPending--;
        if (!settingsPending && selectedId === id && screen === 'chat') renderLiveSettings(failed);
        updateRuntimeControls();
      }
    });
  };
  model.addEventListener('change', () => {
    if (model.value) liveProviderSelection[providerKey] = selectedProvider(catalog, model.value);
    const available = catalog.find(item => item.id === (model.value || snapshot?.default_model))?.efforts || Object.keys(effortLabels);
    const prior = effort.value;
    effort.innerHTML = `<option value="">默认</option>${available.map(value => `<option value="${escape(value)}">${escape(effortLabels[value] || value)}</option>`).join('')}`;
    effort.value = available.includes(prior) ? prior : '';
    queueSave();
  });
  providerSelect?.addEventListener('change', () => {
    liveProviderSelection[providerKey] = providerSelect.value;
    const next = modelProviders(catalog).find(provider => provider.id === providerSelect.value)?.models[0];
    model.innerHTML = modelOptionsHtml(catalog, providerSelect.value, next?.id || null, escape, autoProjectModel ? '项目自动选型 · 聊天默认' : `默认${snapshot?.default_model ? ` · ${catalog.find(item => item.id === snapshot.default_model)?.name || snapshot.default_model}` : ''}`);
    const available = catalog.find(item => item.id === model.value)?.efforts || Object.keys(effortLabels);
    effort.innerHTML = `<option value="">默认</option>${available.map(value => `<option value="${escape(value)}">${escape(effortLabels[value] || value)}</option>`).join('')}`;
    effort.value = '';
    queueSave();
  });
  effort.addEventListener('change', queueSave);
  host.querySelector('#live-reset')!.addEventListener('click', () => { delete liveProviderSelection[providerKey]; model.value = ''; effort.value = ''; queueSave(); });
  const nextStatus = host.querySelector('#live-setting-status')!;
  nextStatus.textContent = busyIn(snapshot, conversation.id) ? '下一条生效 · 当前回复保留原参数' : snapshot?.connection === 'connected' ? '切换即保存 · 下一条消息生效' : '切换即保存 · 连接后应用';
}

function showModelSettings(initialAgent?: string) {
  if (!detail) return;
  if (detail.project) { void configureProjectMembers(); return; }
  const conversation = detail.conversation;
  const agentId = initialAgent && conversation.members.includes(initialAgent) ? initialAgent : conversation.members[0];
  const settings = detail.sessions.find(session => session.agent_id === agentId)!;
  const snapshot = runtimes[agentId];
  const connected = snapshot?.connection === 'connected';
  const catalog = connected ? snapshot.models : [];
  const labels: Record<string, string> = { none: '关闭思考', off: '关闭思考', minimal: '最少', low: '低', medium: '中', high: '高', xhigh: '很高', max: '最大', ultra: 'Ultra' };
  const inherited = snapshot?.default_model;
  const providers = modelProviders(catalog);
  const provider = selectedProvider(catalog, settings.model || inherited);
  const providerControl = providers.length > 1 ? `<label class="field">提供商<select id="session-provider">${providerOptionsHtml(catalog, provider, escape)}</select></label>` : '';
  const modelControl = `<select id="session-model" name="model">${modelOptionsHtml(catalog, provider, settings.model, escape, `沿用本机默认${inherited ? ` · ${catalog.find(item => item.id === inherited)?.name || inherited}` : ''}`)}${settings.model && !catalog.some(model => model.id === settings.model) ? `<option value="${escape(settings.model)}" selected>${escape(settings.model)} · 当前目录不可用</option>` : ''}</select>`;
  // 模型目录来自该成员的原生进程，只有连接时读得到。没连接就给一个一键入口，别让人对着空下拉没法下手。
  const note = connected
    ? (agentId === 'codex-win' ? '选项来自本机 Codex 模型目录；实际调用成功才表示模型可用。' : agentId === 'claude-win' ? '模型与强度等级来自本机 Claude Code 账户的模型目录；强度以 --effort 传给 Claude Code。' : '模型来自 Hermes 原生目录；思考强度传给当前 agent，由 Hermes 根据模型与提供商映射。')
    : `${snapshot ? '连接成员后可读取模型目录。当前设置只保存到本机会话，发送时再校验。' : '该成员尚未接入。设置仅保存在本机，接入后才能应用到真实会话。'} <button type="button" class="text-button" id="model-connect">连接并读取模型</button>`;
  openModal('会话模型设置', '每位成员分别保存，从下一条消息开始使用。', `<form id="model-form"><label class="field">会话成员<select id="settings-agent">${conversation.members.map(id => `<option value="${escape(id)}" ${id === agentId ? 'selected' : ''}>${escape(member(id).name)}</option>`).join('')}</select></label>${providerControl}<label class="field">模型${modelControl}</label><label class="field">思考强度<select id="session-effort" name="effort"></select></label><p class="model-setting-note">${note}</p><p class="form-error" role="alert"></p><div class="modal-footer"><button type="button" class="secondary" id="reset-model">恢复默认</button><button class="primary" ${conversation.archived ? 'disabled' : ''}>保存设置${icon('check')}</button></div></form>`);
  const form = modal.querySelector<HTMLFormElement>('#model-form')!;
  const modelInput = form.querySelector<HTMLInputElement | HTMLSelectElement>('#session-model')!;
  const effortInput = form.querySelector<HTMLSelectElement>('#session-effort')!;
  const updateEfforts = (desired = effortInput.value) => {
    const selected = catalog.find(model => model.id === (modelInput.value || inherited));
    const efforts = selected ? selected.efforts : Object.keys(labels);
    effortInput.innerHTML = `<option value="">沿用默认强度</option>${efforts.map(effort => `<option value="${escape(effort)}">${escape(labels[effort] || effort)}</option>`).join('')}`;
    effortInput.value = efforts.includes(desired) ? desired : '';
  };
  updateEfforts(settings.reasoning_effort || '');
  modelInput.addEventListener('change', () => updateEfforts());
  form.querySelector<HTMLSelectElement>('#session-provider')?.addEventListener('change', event => {
    const selectedProviderId = (event.target as HTMLSelectElement).value;
    modelInput.innerHTML = modelOptionsHtml(catalog, selectedProviderId, null, escape, `沿用本机默认${inherited ? ` · ${catalog.find(item => item.id === inherited)?.name || inherited}` : ''}`);
    updateEfforts('');
  });
  form.querySelector('#settings-agent')!.addEventListener('change', event => { modal.close(); showModelSettings((event.target as HTMLSelectElement).value); });
  form.querySelector('#reset-model')!.addEventListener('click', () => { modelInput.value = ''; updateEfforts(''); });
  form.querySelector('#model-connect')?.addEventListener('click', async () => {
    // 连接会返回快照（含模型目录），回来重开这个弹窗，模型下拉就有选项了。
    modal.close();
    await connectAgent(agentId);
    showModelSettings(agentId);
  });
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const button = form.querySelector<HTMLButtonElement>('[type=submit], .primary')!;
    button.disabled = true;
    try {
      await invoke<ConversationDetail>('set_session_settings', { id: conversation.id, agentId, model: modelInput.value || null, reasoningEffort: effortInput.value || null });
      modal.close();
      if (selectedId === conversation.id && screen === 'chat') await selectConversation(conversation.id);
      toast(`${member(agentId).name} 的会话设置已保存${snapshot?.connection === 'connected' ? '，下一条消息生效' : '，连接后应用'}`);
    } catch (error) { form.querySelector('.form-error')!.textContent = errorText(error); }
    finally { button.disabled = false; }
  });
}

function showCreate(initialKind: 'direct' | 'group' = 'group') {
  openModal('新建会话', '选择对话成员，为这个话题留一个独立空间。', `<form id="create-form"><div class="kind-switch"><button type="button" data-kind="direct" class="${initialKind === 'direct' ? 'selected' : ''}">${icon('chat')}私聊</button><button type="button" data-kind="group" class="${initialKind === 'group' ? 'selected' : ''}">${icon('group')}群聊</button></div><label class="field">会话名称<input name="title" required maxlength="80" placeholder="例如：框架开发 / 今天的想法" autocomplete="off"></label><span class="field-label">${initialKind === 'group' ? '选择二至五位成员' : '选择一位成员'}</span><div class="member-options">${memberOptions(initialKind === 'group' ? ['hermes-win', 'codex-win', 'dsh-win'] : ['codex-win'], initialKind === 'group' ? 'checkbox' : 'radio')}</div><p class="form-error" role="alert"></p><div class="modal-footer"><span>现有会话与私人记忆保持独立</span><button class="primary" type="submit">创建会话${icon('arrow')}</button></div></form>`);
  modal.querySelectorAll<HTMLButtonElement>('[data-kind]').forEach(button => button.addEventListener('click', () => { modal.close(); showCreate(button.dataset.kind as 'direct' | 'group'); }));
  const form = modal.querySelector<HTMLFormElement>('#create-form')!;
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const data = new FormData(form);
    const button = form.querySelector<HTMLButtonElement>('[type=submit]')!; button.disabled = true;
    try {
      const conversation = await invoke<Conversation>('create_conversation', { title: String(data.get('title') || ''), kind: initialKind, members: data.getAll('member').map(String) });
      modal.close(); archived = false; search = ''; filter = 'all'; document.querySelector<HTMLInputElement>('#search')!.value = '';
      await refreshList(); await selectConversation(conversation.id); toast('新的会话已创建');
    } catch (error) { form.querySelector('.form-error')!.textContent = errorText(error); }
    finally { button.disabled = false; }
  });
  form.querySelector<HTMLInputElement>('[name=title]')!.focus();
}

function showMembers() {
  if (!detail) return;
  const conversation = detail.conversation;
  openModal('管理群聊成员', '保留的成员沿用当前会话映射，新成员使用独立映射。', `<form id="members-form"><div class="member-options">${memberOptions(conversation.members, 'checkbox')}</div><p class="form-error" role="alert"></p><div class="modal-footer"><span>至少保留两位成员</span><button class="primary">保存成员${icon('check')}</button></div></form>`);
  const form = modal.querySelector<HTMLFormElement>('#members-form')!;
  form.addEventListener('submit', async event => {
    event.preventDefault(); const button = form.querySelector<HTMLButtonElement>('button')!; button.disabled = true;
    try {
      await invoke('update_group_members', { id: conversation.id, members: new FormData(form).getAll('member').map(String) });
      modal.close(); await refreshList(); await selectConversation(conversation.id); toast('群聊成员已更新');
    } catch (error) { form.querySelector('.form-error')!.textContent = errorText(error); }
    finally { button.disabled = false; }
  });
}

function showConversationActions() {
  if (!detail) return;
  const conversation = detail.conversation;
  openModal('会话设置', '改名与归档会保留历史和会话映射。', `<form id="rename-form"><label class="field">会话名称<input name="title" required maxlength="80" value="${escape(conversation.title)}"></label><p class="form-error" role="alert"></p><button class="primary full-width">${icon('edit')}保存名称</button></form><div class="action-divider"></div><button id="archive-action" class="settings-action">${icon('archive')}<span>${conversation.archived ? '恢复会话' : '归档会话'}</span><small>${conversation.archived ? '回到最近会话' : '保留全部历史'}</small></button><button id="delete-action" class="settings-action danger">${icon('close')}<span>删除会话</span><small>删除前会再次确认</small></button>`);
  const form = modal.querySelector<HTMLFormElement>('#rename-form')!;
  if (conversation.kind === 'group') {
    const button = document.createElement('button');
    button.className = 'settings-action';
    button.id = 'members-action';
    button.innerHTML = `${icon('group')}<span>管理群聊成员</span><small>${conversation.members.length} 位成员</small>`;
    button.addEventListener('click', () => { modal.close(); showMembers(); });
    modal.querySelector('#archive-action')!.before(button);
  }
  form.addEventListener('submit', async event => {
    event.preventDefault();
    try { await invoke('rename_conversation', { id: conversation.id, title: String(new FormData(form).get('title') || '') }); modal.close(); await refreshList(); await selectConversation(conversation.id); toast('会话名称已更新'); }
    catch (error) { form.querySelector('.form-error')!.textContent = errorText(error); }
  });
  modal.querySelector('#archive-action')!.addEventListener('click', () => { modal.close(); void toggleArchive(conversation); });
  modal.querySelector('#delete-action')!.addEventListener('click', () => { modal.close(); confirmDeleteConversation(conversation); });
}

/// 归档/恢复会话（设置弹窗与侧栏右键菜单共用）。
async function toggleArchive(conversation: Conversation) {
  try {
    await invoke('archive_conversation', { id: conversation.id, archived: !conversation.archived });
    toast(conversation.archived ? '会话已恢复' : '会话已归档');
    if (selectedId === conversation.id) { archived = !conversation.archived; await refreshList(); await selectConversation(conversation.id); }
    else await refreshList();
  } catch (error) { toast(errorText(error), true); }
}

/// 删除会话前二次确认（设置弹窗与侧栏右键菜单共用）。
function confirmDeleteConversation(conversation: Conversation) {
  openModal('删除这个会话？', `“${conversation.title}”中的 ${conversation.message_count} 条本地消息和会话映射将被删除。`, `<p class="delete-notice">此操作不可撤销。其他会话保持独立。</p><div class="modal-footer"><button class="secondary" id="cancel-delete">保留会话</button><button class="danger-button" id="confirm-delete">确认删除</button></div>`);
  modal.querySelector('#cancel-delete')!.addEventListener('click', () => modal.close());
  modal.querySelector('#confirm-delete')!.addEventListener('click', async () => {
    const button = modal.querySelector<HTMLButtonElement>('#confirm-delete')!; button.disabled = true;
    try {
      await invoke('delete_conversation', { id: conversation.id }); localStorage.removeItem(draftKey(conversation.id)); modal.close();
      if (selectedId === conversation.id) {
        detail = null; selectedId = ''; localStorage.removeItem('hub.selected'); await refreshList();
        if (conversations[0]) await selectConversation(conversations[0].id); else renderChat();
      } else await refreshList();
      toast('会话已删除');
    } catch (error) {
      if (errorText(error).includes('会话不存在')) { modal.close(); await refreshList(); toast('会话已不存在，已从列表移除'); return; }
      toast(errorText(error), true); button.disabled = false;
    }
  });
}

// `ServiceInfo` and the update-check result shapes live in `./types`.

// Card order matches the four managed members rendered by `renderServices`.
const serviceCardOrder = ['hermes-win', 'codex-win', 'dsh-win', 'claude-win', 'albion-wsl'] as const;

// Passes through only the backend's real data: a version comes from the runtime handshake
// (or a later install manifest), otherwise "unknown" is shown. No update claim is made here.
function serviceVersionText(info: ServiceInfo | undefined, snapshot: RuntimeSnapshot) {
  const version = info ? info.runtime_version : snapshot.version;
  if (!version) return '版本未检测';
  const source = info ? info.version_source : 'handshake';
  return `运行 v${escape(version.replace(/^v/i, ''))} · 来源 ${escape(source)}`;
}

function serviceStatusHtml(info: ServiceInfo | undefined, snapshot: RuntimeSnapshot) {
  const connection = info?.connection ?? snapshot.connection;
  const busy = info ? info.busy : snapshot.connection === 'connecting' || busySnapshot(snapshot);
  const label = connection === 'connected' ? '接口已连接' : connection === 'connecting' ? '正在连接' : '未连接';
  const update = info ? `<span data-service-update>更新：${info.update_state === 'unchecked' ? '未检测' : escape(info.update_state)}</span>` : '';
  return `<span class="status-dot ${connection === 'connected' ? '' : 'muted'}"></span>${label}${busy ? ' · 运行中' : ''}<span>${serviceVersionText(info, snapshot)}</span>${update}`;
}

// The backend re-detects the service under its maintenance lock and reports the
// latest published release. This flow is check-only: nothing is downloaded and the
// installation is never changed, so no prepare or apply call belongs here.
/// 「检查更新」的结果按成员存下来（徽标文字 + 详情），每次重绘后重新贴上。
const serviceChecks: Record<string, { badge: string; detail: string; tip: string; prepareLabel: string }> = {};

/// 把上次的检查结论贴回卡片（重绘后也要贴，否则结论会被冲掉）。
function applyCheckResult(card: HTMLElement, agentId: string) {
  const saved = serviceChecks[agentId];
  if (!saved) return;
  const badge = card.querySelector<HTMLElement>('[data-service-update]');
  if (badge) badge.textContent = saved.badge;
  const host = card.querySelector<HTMLElement>('.service-check-result');
  // 结论行不显示（保留在 DOM 里给验收读），细节挂卡片悬停。
  if (host) { host.innerHTML = saved.detail; host.title = saved.tip; }
  card.title = saved.tip;
}

async function checkServiceUpdate(agentId: string, card: HTMLElement) {
  const button = card.querySelector<HTMLButtonElement>('[data-service-check]');
  const host = card.querySelector<HTMLElement>('.service-check-result');
  if (!button || !host) return;
  button.disabled = true;
  button.textContent = '检查中…';
  try {
    const result = await invoke<ServiceCheckResult>('check_service_update', { serviceId: agentId });
    const stamp = Number(result.checked_at);
    const checkedAt = Number.isFinite(stamp) ? `${date(stamp)} ${time(stamp)}` : result.checked_at;
    const runtime = result.runtime_version ? `运行 v${escape(result.runtime_version)}` : '运行版本未检测';
    const installed = result.installed_version ? `安装 v${escape(result.installed_version)}` : '安装版本未检测';
    const latest = result.latest_version ? `发行 v${escape(result.latest_version)}` : '发行版本未知';
    // Only a real backend verdict is shown: null means the remote comparison did not
    // complete, so the local detection is never presented as "up to date".
    const verdict = result.update_available === true ? '发现新版本' : result.update_available === false ? '已与发行版本核对，无更新' : '未完成远程版本比较';
    const planLine = agentId === 'dsh-win' || agentId === 'codex-win'
      ? '可准备隔离候选后再更新；当前检查没有更改安装'
      : agentId === 'hermes-win'
        ? '更新会交接给 Hermes 自己的更新通道：它会退出并重启'
        : agentId === 'claude-win'
          ? 'Claude Code 自己负责更新：在终端运行 claude update，同席不覆盖安装'
        : result.update_available === null ? '仅本地版本检测，同席不代更新' : '由外部安装维护，同席不代你更新';
    // 这行字贴着按钮显示很格格不入：卡片上不再出现结论行，只留在 DOM 里供验收读取，
    // 细节挂到卡片悬停上，要看再hover。
    const detail = `<span>${escape(verdict)} · ${escape(checkedAt)}</span>`;
    const tip = `${result.note} · ${runtime} · ${installed} · ${latest} · 来源 ${result.version_source} · 检查来源 ${result.check_source} · ${verdict} · ${planLine}`;
    // 结论存成状态再渲染，别只写一次 DOM：服务页会因 inventory / 运行时变化整页重绘，
    // 一次性的写入会被那次重绘冲掉（徽标退回「未检测」，看着像检查没生效）。
    serviceChecks[agentId] = {
      badge: `更新：${result.update_available === true ? (result.latest_version ? `发现新版本 v${result.latest_version}` : '发现新版本') : result.update_available === false ? '已是最新' : '未检测'}`,
      detail,
      tip,
      // 有新版时把按钮写清版本号，也是状态：重绘后不能退回「准备更新」。
      prepareLabel: result.update_available === true && result.latest_version ? `更新到 v${result.latest_version}` : '准备更新',
    };
    applyCheckResult(card, agentId);
    // 只有有受管安装槽的成员有这两个按钮（DSH 与 Codex）。
    const prepare = card.querySelector<HTMLButtonElement>('[data-service-prepare]');
    if (prepare) prepare.textContent = serviceChecks[agentId].prepareLabel;
    const apply = card.querySelector<HTMLButtonElement>('[data-service-apply]');
    if (apply && result.update_available === true && result.latest_version && !apply.textContent?.includes('v')) apply.textContent = `启用 v${result.latest_version}`;
  } catch (error) { toast(errorText(error), true); }
  finally { if (button.isConnected) { button.disabled = false; button.textContent = '检查更新'; } }
}

// 停掉本软件自己启动的原生子进程并重新连接：维护门禁在后端，期间拒绝新消息与任务。
// 成功后用返回的真实快照接管运行时；在服务页时整体重绘卡片。
async function restartService(agentId: string, card: HTMLElement) {
  const button = card.querySelector<HTMLButtonElement>('[data-service-restart]');
  if (!button) return;
  button.disabled = true;
  button.textContent = '重启中…';
  try {
    adoptRuntime(await invoke<RuntimeSnapshot>('restart_service', { serviceId: agentId }), agentId);
    toast(`${member(agentId).name} 已重启`);
    if (screen === 'services') renderServices();
  } catch (error) { toast(errorText(error), true); }
  finally { if (button.isConnected) { button.disabled = false; button.textContent = '重启'; } }
}

// DSH 的隔离更新状态只来自后端：卡片只显示真实版本，不渲染计划标识、磁盘槽路径或原始记录。
const serviceUpdates: Record<string, ServiceUpdateStatus> = {};
const servicePending: Record<string, 'prepare' | 'apply' | 'rollback' | null> = {};
const serviceVersion = (value: string) => escape(value.replace(/^v/i, ''));

// 只反映后端状态：没有已验证候选就不能启用，没有可回退版本就不能回退。
function paintServiceUpdate(card: HTMLElement, agentId: string) {
  const host = card.querySelector<HTMLElement>('.service-update');
  if (!host) return;
  const status = serviceUpdates[agentId];
  const pending = servicePending[agentId] || null;
  const ready = !!status?.ready_plan_id;
  const revertible = !!status?.rollback_available;
  const prepare = card.querySelector<HTMLButtonElement>('[data-service-prepare]');
  if (prepare) { prepare.disabled = pending !== null; prepare.textContent = pending === 'prepare' ? '准备中…' : (serviceChecks[agentId]?.prepareLabel || '准备更新'); }
  const apply = card.querySelector<HTMLButtonElement>('[data-service-apply]');
  if (apply) { apply.disabled = pending !== null || !ready; apply.textContent = pending === 'apply' ? '启用中…' : '启用候选'; }
  const revert = card.querySelector<HTMLButtonElement>('[data-service-rollback]');
  if (revert) { revert.disabled = pending !== null || !revertible; revert.textContent = pending === 'rollback' ? '回退中…' : '回退上一版'; }
  const lines: string[] = [];
  if (status?.active_version) lines.push(`当前 v${serviceVersion(status.active_version)}`);
  if (status?.candidate_version) lines.push(`候选 v${serviceVersion(status.candidate_version)} · 已验证接口待启用`);
  if (revertible && status?.previous_version) lines.push(`可回退到 v${serviceVersion(status.previous_version)}`);
  host.hidden = lines.length === 0;
  host.innerHTML = lines.map(line => `<span>${line}</span>`).join('');
}

/// 最近一次 `service_inventory` 的结果：卡片重画（例如更新状态回填）时不该把版本与徽标弄丢。
const serviceInventory = new Map<string, ServiceInfo>();

function paintServiceCard(card: HTMLElement, agentId: string, info?: ServiceInfo) {
  // 没显式给 inventory 就取最近一次的：否则一次不带 info 的重画会把状态行退回占位符。
  const resolved = info ?? serviceInventory.get(agentId);
  const snapshot = runtimes[agentId];
  const connection = resolved?.connection ?? snapshot.connection;
  card.querySelector('.service-status')!.innerHTML = serviceStatusHtml(resolved, snapshot);
  const control = card.querySelector<HTMLButtonElement>('[data-service-connect]')!;
  control.disabled = connection === 'connecting';
  control.id = `service-${harnessKey(agentId)}-control`;
  control.textContent = connection === 'connected' ? `断开 ${member(agentId).name}` : connection === 'connecting' ? '正在连接…' : `连接 ${member(agentId).name}`;
  control.title = '连接本机原生接口，当前仅支持独立私聊';
  control.onclick = () => void (connection === 'connected' ? disconnectAgent(agentId) : connectAgent(agentId));
  const check = card.querySelector<HTMLButtonElement>('[data-service-check]')!;
  check.disabled = connection === 'connecting';
  check.onclick = () => void checkServiceUpdate(agentId, card);
  const restart = card.querySelector<HTMLButtonElement>('[data-service-restart]')!;
  restart.hidden = connection !== 'connected';
  restart.disabled = connection !== 'connected';
  restart.textContent = '重启';
  restart.onclick = () => void restartService(agentId, card);
  const prepare = card.querySelector<HTMLButtonElement>('[data-service-prepare]');
  if (prepare) prepare.onclick = () => void prepareServiceUpdate(agentId, card);
  const apply = card.querySelector<HTMLButtonElement>('[data-service-apply]');
  if (apply) apply.onclick = () => void applyServiceUpdate(agentId, card);
  const revert = card.querySelector<HTMLButtonElement>('[data-service-rollback]');
  if (revert) revert.onclick = () => void rollbackServiceUpdate(agentId, card);
  paintServiceUpdate(card, agentId);
  // 维护动作在途时整张卡的操作都禁用：进度由后端维护，界面不自行推断完成。
  if (servicePending[agentId]) card.querySelectorAll<HTMLButtonElement>('.service-actions button').forEach(item => { item.disabled = true; });
  applyCheckResult(card, agentId);
}

/// 维护动作结束后重画这张卡。
///
/// **不看调用方传进来的 card**：动作期间可能整页重绘（`adoptRuntime` 在服务页会重画），
/// 旧引用已经脱离文档；此时那句「card.isConnected 才重画」会被跳过，而重绘时
/// `servicePending` 还在，按钮被整体禁掉——再没人放开，于是卡片看起来卡死。
function repaintServiceCard(agentId: string) {
  if (screen !== 'services') return;
  const card = main.querySelector<HTMLElement>(`.service-card[data-service-id="${agentId}"]`);
  if (card) paintServiceCard(card, agentId);
}

// 只有显式点击才会下载候选；“检查更新”保持只读，绝不自动触发准备。
async function prepareServiceUpdate(agentId: string, card: HTMLElement) {
  if (servicePending[agentId]) return;
  servicePending[agentId] = 'prepare';
  paintServiceCard(card, agentId);
  try {
    const plan = await invoke<ServicePlanResult>('prepare_service_update', { serviceId: agentId });
    if (!plan.supported) { toast(plan.reason || '该成员暂不支持隔离更新', true); return; }
    // 候选版本与可启用条件以后端随后返回的状态为准，前端不缓存本地推断。
    serviceUpdates[agentId] = await invoke<ServiceUpdateStatus>('service_update_status', { serviceId: agentId });
    toast(plan.candidate_version ? `候选 v${String(plan.candidate_version).replace(/^v/i, '')} 已验证，确认后可启用` : '候选已准备，确认后可启用');
  } catch (error) { toast(errorText(error), true); }
  finally {
    servicePending[agentId] = null;
    repaintServiceCard(agentId);
  }
}

// 启用使用准备时就绪的计划标识，仅作为调用参数传递，不渲染给用户。
async function applyServiceUpdate(agentId: string, card: HTMLElement) {
  const planId = serviceUpdates[agentId]?.ready_plan_id;
  if (!planId) { toast('没有已验证的候选版本可启用', true); return; }
  if (servicePending[agentId]) return;
  servicePending[agentId] = 'apply';
  paintServiceCard(card, agentId);
  try {
    adoptRuntime(await invoke<RuntimeSnapshot>('apply_service_update', { serviceId: agentId, planId }), agentId);
    serviceUpdates[agentId] = await invoke<ServiceUpdateStatus>('service_update_status', { serviceId: agentId });
    toast(`${member(agentId).name} 已启用候选版本`);
  } catch (error) { toast(errorText(error), true); }
  finally {
    servicePending[agentId] = null;
    repaintServiceCard(agentId);
  }
}

// 回退只在后端报告可回退时可用；成功后接管真实快照并整体重绘服务页。
async function rollbackServiceUpdate(agentId: string, card: HTMLElement) {
  if (!serviceUpdates[agentId]?.rollback_available) { toast('当前没有可回退的版本', true); return; }
  if (servicePending[agentId]) return;
  servicePending[agentId] = 'rollback';
  paintServiceCard(card, agentId);
  let applied = false;
  try {
    adoptRuntime(await invoke<RuntimeSnapshot>('rollback_service_update', { serviceId: agentId }), agentId);
    serviceUpdates[agentId] = await invoke<ServiceUpdateStatus>('service_update_status', { serviceId: agentId });
    applied = true;
    toast(`${member(agentId).name} 已回退到上一版本`);
  } catch (error) { toast(errorText(error), true); }
  finally { servicePending[agentId] = null; }
  if (screen !== 'services') return;
  if (applied) renderServices();
  else repaintServiceCard(agentId);
}

function renderServices() {
  disposeProjectTranscript();
  ++detailGeneration; screen = 'services'; renderList(); enterView('services');
  main.innerHTML = `<header class="chat-header"><div><span class="eyebrow">你的协作成员</span><h1>成员与服务</h1></div><button id="back-to-chat" class="secondary">${icon('chat')}返回会话</button></header><div class="services-content">${setupBanner(escape)}<div class="services-intro"><span class="eyebrow">四位技术成员 + 你的伴侣</span><h2>在同一个空间里，<br>保留各自的职责。</h2><p>DSH 与 Codex 支持隔离更新与回退（可准备候选后一键切换）· Hermes 只做只读核对、更新交给它自己的通道 · Claude 核对 npm 发行版、由 claude update 自行更新 · 伴侣在你自己的服务里更新</p><span class="phase-chip">版本检查保持只读 · 更新需显式确认</span></div><div class="service-grid">${agents.map(agent => `<article class="service-card" data-service-id="${escape(agent.id)}">${avatar(agent, 'large')}<div class="service-card-heading"><h3>${escape(agent.name)}</h3><span>${escape(agent.location)}</span></div><strong>${escape(agent.subtitle)}</strong><p>${escape(agent.role)}</p>${setupLine(agent.id, escape)}<div class="service-status"><span class="status-dot muted"></span>待接入<span>版本未检测</span></div><div class="service-actions"><button data-service-connect class="secondary" disabled>连接</button><button data-service-check class="secondary" title="重新检测该服务的安装与运行版本">检查更新</button><button data-service-restart class="secondary" hidden title="停掉本软件自己启动的原生子进程并重新连接，期间拒绝新消息与任务">重启</button>${['dsh-win', 'codex-win'].includes(agent.id) ? `<button data-service-prepare class="secondary" disabled title="只在显式点击后下载并验证隔离候选，检查更新不会触发下载">准备更新</button><button data-service-apply class="secondary" disabled title="只在候选已验证就绪时启用；启用后由后端保持原有连接状态">启用候选</button><button data-service-rollback class="secondary" disabled title="只在存在可回退的上一版本时可用">回退上一版</button>` : ''}</div><div class="service-check-result" hidden></div>${['dsh-win', 'codex-win'].includes(agent.id) ? '<div class="service-update" hidden></div>' : ''}</article>`).join('')}</div><div class="storage-card"><div>${icon('settings')}<strong>独立的本地数据</strong><span>v${escape(info?.version || '0.1.0')}</span></div><p>会话与消息草稿保存到本机，现有 agent 的配置与服务不受影响。</p><code>${escape(info?.database_path || '')}</code></div></div>`;
  main.querySelector('#back-to-chat')!.addEventListener('click', () => { screen = 'chat'; if (selectedId) void selectConversation(selectedId); else renderChat(); });
  const cards = [...main.querySelectorAll<HTMLElement>('.service-card')];
  const paint = (inventory?: Map<string, ServiceInfo>) => {
    if (screen !== 'services') return;
    cards.forEach((card, index) => {
      const agentId = card.dataset.serviceId || serviceCardOrder[index];
      paintServiceCard(card, agentId, inventory?.get(agentId));
    });
  };
  paint();
  wireSetup();
  if (!scanned().length) void scanAgents().then(paintSetup).catch(error => toast(errorText(error), true));
  // Read-only inventory merge; on failure keep the runtime-based cards instead of blanking the screen.
  // 失败要说出来：静默 catch 只会让「卡片停在占位符」变成一个查不出的幽灵。
  void invoke<ServiceInfo[]>('service_inventory')
    .then(items => {
      serviceInventory.clear();
      items.forEach(item => serviceInventory.set(item.id, item));
      paint(serviceInventory);
    })
    .catch(error => console.error('service_inventory 合并失败', error));
  // 更新状态以后端为准：只有页面仍在服务页且卡片仍然存在时才回填，离开后不写入。
  // 有受管安装槽的成员都要问一遍（现在 DSH 与 Codex 各一套自己的槽）。
  for (const id of ['dsh-win', 'codex-win']) {
    const card = cards.find(item => item.dataset.serviceId === id);
    if (!card) continue;
    void invoke<ServiceUpdateStatus>('service_update_status', { serviceId: id })
      .then(status => {
        if (screen !== 'services' || !card.isConnected) return;
        serviceUpdates[id] = status;
        paintServiceCard(card, id);
      })
      .catch(() => {});
  }
}

document.querySelector('#new-conversation')!.addEventListener('click', () => showCreate());
document.querySelectorAll<HTMLButtonElement>('[data-filter]').forEach(button => button.addEventListener('click', () => { filter = button.dataset.filter as typeof filter; renderList(); }));
installConversationMenu(document.querySelector<HTMLDivElement>('#conversation-list')!);
document.querySelector<HTMLInputElement>('#search')!.addEventListener('input', event => {
  search = (event.target as HTMLInputElement).value;
  // Invalidate an older request immediately, including the debounce interval.
  ++listGeneration; clearTimeout(searchTimer);
  searchTimer = setTimeout(() => void refreshList().catch(error => toast(errorText(error), true)), 180);
});
document.querySelector('#archive-filter')!.addEventListener('click', async () => {
  archived = !archived;
  try { await refreshList(); } catch (error) { toast(errorText(error), true); }
});
document.querySelector('#service-link')!.addEventListener('click', () => renderServices());
document.addEventListener('keydown', event => { if (event.ctrlKey && event.key.toLowerCase() === 'k') { event.preventDefault(); document.querySelector<HTMLInputElement>('#search')!.focus(); } });

/// 扫描结果回来后只重画接入横条和每张卡的安装行，不重画整页（卡上的更新状态保留）。
function paintSetup() {
  if (screen !== 'services') return;
  main.querySelector('#setup-center')?.replaceWith(Object.assign(document.createElement('div'), { innerHTML: setupBanner(escape) }).firstElementChild!);
  for (const card of main.querySelectorAll<HTMLElement>('.service-card')) card.querySelector('.setup-line')?.replaceWith(Object.assign(document.createElement('div'), { innerHTML: setupLine(card.dataset.serviceId!, escape) }).firstElementChild!);
  wireSetup();
}

function wireSetup() {
  const host = { escape, open: openModal, modal, toast };
  main.querySelector('#setup-rescan')?.addEventListener('click', async () => { try { await scanAgents(); paintSetup(); toast('已重新扫描本机'); } catch (error) { toast(errorText(error), true); } });
  main.querySelector<HTMLInputElement>('#auto-connect')?.addEventListener('change', event => setAutoConnect((event.target as HTMLInputElement).checked));
  main.querySelector('#setup-connect-all')?.addEventListener('click', async event => {
    const button = event.currentTarget as HTMLButtonElement; button.disabled = true; button.textContent = '正在接入…';
    for (const env of scanned()) if (env.installed && runtimes[env.id]?.connection !== 'connected') await connectAgent(env.id);
    if (button.isConnected) { button.disabled = false; button.textContent = '接入已安装的成员'; }
  });
  main.querySelectorAll<HTMLButtonElement>('[data-agent-install]').forEach(button => button.onclick = () => confirmInstall(button.dataset.agentInstall!, host, () => toast('装好后回到这里点「重新扫描」')));
  main.querySelectorAll<HTMLButtonElement>('[data-agent-setup]').forEach(button => button.onclick = () => void runSetup(button.dataset.agentSetup!, host));
  main.querySelectorAll<HTMLButtonElement>('[data-companion-config]').forEach(button => button.onclick = () => void companionDialog({ ...host, saved: reloadAgents }).catch(error => toast(errorText(error), true)));
}

/// 伴侣改了名字、头像后重新取成员名单，侧栏与当前页一起更新。
async function reloadAgents() {
  agents = await invoke<Agent[]>('list_agents');
  await refreshList();
  if (screen === 'services') renderServices(); else if (selectedId) await selectConversation(selectedId);
}

/// 启动时自动接入：不弹连接成功的提示，失败也只在服务页显示。
async function connectQuietly(agentId: string) {
  try { adoptRuntime(await invoke<RuntimeSnapshot>(`connect_${harnessKey(agentId)}`), agentId); updateRuntimeControls(); if (screen === 'services') renderServices(); }
  catch { /* 留给服务页的状态行 */ }
}

async function boot() {  const controls = document.querySelectorAll<HTMLButtonElement>('#new-conversation, #service-link, #archive-filter, [data-filter]');
  controls.forEach(button => { button.disabled = true; });
  document.querySelector<HTMLInputElement>('#search')!.disabled = true;
  if (!isTauri()) {
    main.innerHTML = `<div class="no-selection">${brand}<h1>请从桌面软件打开同席。</h1><p>这个页面需要桌面应用提供本地数据存储。<br>运行 Agent Hub.exe，或使用 npm run desktop。</p></div>`;
    document.querySelector<HTMLButtonElement>('#new-conversation')!.disabled = true;
    document.querySelector<HTMLButtonElement>('#service-link')!.disabled = true;
    return;
  }
  try {
    if (!eventSubscribed) {
      for (const agentId of ['codex-win', 'hermes-win', 'dsh-win', 'claude-win', 'albion-wsl']) {
        await listen<RuntimeSnapshot>(`${harnessKey(agentId)}-state`, event => {
          const old = runtimes[agentId];
          if (!adoptRuntime(event.payload, agentId)) return;
          updateRuntimeControls();
          if (screen === 'services' && old.connection !== event.payload.connection) renderServices();
          for (const active of runsOf(event.payload)) {
            if (detail?.project_agent_chats?.some(chat => chat.conversation_id === active.conversation_id)) {
              const completed = (active.tool_events || []).filter(tool => tool.status !== 'running');
              const stamp = `${active.id}:${active.status}:${completed.map(tool => `${tool.event_id}:${tool.status}:${tool.output?.length}`).join('|')}`;
              if ((completed.length || !liveRun(active)) && projectChatRefreshStamp[active.conversation_id] !== stamp) {
                projectChatRefreshStamp[active.conversation_id] = stamp;
                void refreshProjectAgentChat(active.conversation_id);
              }
            }
            // 每个会话的运行各自收尾：结束时刷新列表并重新载入正在看的会话。
            const final = `${active.id}:${active.status}`;
            if (!liveRun(active) && lastFinalRuns[`${agentId}:${active.conversation_id}`] !== final) {
              const first = lastFinalRuns[`${agentId}:${active.conversation_id}`] === undefined && !lastFinalSeeded.has(agentId);
              lastFinalRuns[`${agentId}:${active.conversation_id}`] = final;
              if (first) continue;
              void refreshList().then(() => { if (screen === 'chat' && selectedId === active.conversation_id) return selectConversation(selectedId); }).catch(error => toast(errorText(error), true));
              if (active.error) toast(active.error, true);
            }
          }
          lastFinalSeeded.add(agentId);
        });
      }
      await listen<{ conversation_id: string; agent_id: string }[]>('cli-handoffs', event => adoptCliHandoffs(event.payload));
      await listen<{ conversation_id: string; agent_id: string; imported: number }>('cli-returned', event => cliReturned(event.payload.conversation_id, event.payload.agent_id, event.payload.imported));
      await listen<{ revision: number; discussion: Discussion }>('discussion-state', event => {
        if (event.payload.revision < discussionRevision) return;
        discussionRevision = event.payload.revision;
        adoptDiscussion(event.payload.discussion);
        if (!discussionBusy(event.payload.discussion)) {
          void refreshList().then(() => { if (screen === 'chat' && selectedId === event.payload.discussion.conversation_id) return selectConversation(selectedId); }).catch(error => toast(errorText(error), true));
        }
      });
      await listen<{ revision: number; workflow: Workflow }>('project-state', event => {
        if (event.payload.revision < projectRevision) return;
        projectRevision = event.payload.revision;
        adoptProject(event.payload.workflow, true); updateRuntimeControls();
        if (!workflowBusy(event.payload.workflow)) void refreshList().catch(error => toast(errorText(error), true));
      });
      eventSubscribed = true;
    }
    for (const agentId of ['codex-win', 'hermes-win', 'dsh-win', 'claude-win', 'albion-wsl']) adoptRuntime(await invoke<RuntimeSnapshot>(`${harnessKey(agentId)}_status`), agentId);
    latestDiscussion = await invoke<Discussion | null>('discussion_status');
    for (const job of await invoke<Workflow[]>('project_status')) adoptProject(job);
    [agents, info] = await Promise.all([invoke<Agent[]>('list_agents'), invoke<AppInfo>('app_info')]);
    document.querySelector('#app-version')!.textContent = info.version;
    updateMemberCount();
    adoptCliHandoffs(await invoke<{ conversation_id: string; agent_id: string }[]>('cli_handoffs'));
    await refreshList();
    const initial = conversations.find(conversation => conversation.id === selectedId) || conversations.find(conversation => conversation.kind === 'group') || conversations[0];
    if (initial) await selectConversation(initial.id); else renderChat();
    controls.forEach(button => { button.disabled = false; });
    document.querySelector<HTMLInputElement>('#search')!.disabled = false;
    // 后台扫描并接入本机的成员；一个都没找到的新用户，第一次直接带到接入页。
    if (info?.autoconnect !== false) void autoConnect(id => runtimes[id]?.connection === 'connected', connectQuietly).then(() => {
      let onboarded = false;
      try { onboarded = localStorage.getItem('hub.onboarded') === '1'; localStorage.setItem('hub.onboarded', '1'); } catch { /* 没有存储就每次都不打扰 */ onboarded = true; }
      if (!onboarded && scanned().length && !scanned().some(env => env.installed)) { renderServices(); toast('还没在本机找到任何 agent：在「接入成员」里一键安装'); }
    });
  } catch (error) {
    main.innerHTML = `<div class="no-selection"><h1>本地工作空间未能打开。</h1><p>${escape(errorText(error))}</p><button id="retry-boot" class="primary">重试</button></div>`;
    main.querySelector('#retry-boot')!.addEventListener('click', () => void boot());
  }
}

void boot();
