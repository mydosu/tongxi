import { invoke, isTauri } from '@tauri-apps/api/core';
import { emit, listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import type { Agent, AppInfo, Conversation, ConversationDetail, Discussion, Message, NativeSession, Project, RunRecord, RuntimeSnapshot, ServiceCheckResult, ServiceInfo, ServicePlanResult, ServiceUpdateStatus, TaskChoice, Workflow } from './types';
import { bindingDialog, planningDialog, workflowBusy, workflowCard, workflowRoles } from './project_ui';
import { modelOptionsHtml, modelProviders, providerOptionsHtml, selectedProvider } from './model_select';
import './styles.css';

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
  close: '<path d="m6 6 12 12M6 18 18 6"/>',
  edit: '<path d="m15 5 4 4M4 20l5-1L20 8l-4-4L5 15z"/>',
};
const icon = (name: string, className = '') => `<svg class="icon ${className}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${icons[name] || icons.chat}</svg>`;
const escape = (value: string | number) => String(value).replace(/[&<>"']/g, char => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[char]!));
const avatar = (agent: Agent, size = '') => `<span class="avatar ${escape(agent.accent)} ${size}" aria-hidden="true">${agent.id === 'albion-wsl' ? 'A' : agent.name[0]}</span>`;
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
const runtimes: Record<string, RuntimeSnapshot> = { 'codex-win': emptyRuntime(), 'hermes-win': emptyRuntime(), 'dsh-win': emptyRuntime(), 'albion-wsl': emptyRuntime() };
const currentAgentId = () => detail?.conversation.kind === 'direct' ? detail.conversation.members[0] : 'codex-win';
const currentRuntime = () => runtimes[currentAgentId()] || emptyRuntime();
const harnessKey = (id = currentAgentId()) => id === 'hermes-win' ? 'hermes' : id === 'dsh-win' ? 'dsh' : id === 'albion-wsl' ? 'albion' : 'codex';
const busySnapshot = (snapshot: RuntimeSnapshot) => !!snapshot.active && ['starting', 'running', 'cancelling'].includes(snapshot.active.status);

let pendingSend: { conversationId: string; messageId: string; content: string } | null = null;
const lastFinalRuns: Record<string, string> = {};
let eventSubscribed = false;
let latestDiscussion: Discussion | null = null;
const projectJobs = new Map<string, Workflow>();
let projectRevision = 0;
let pendingProject: { conversationId: string; messageId: string; content: string } | null = null;
let pendingSummary: { projectId: string; enabled: boolean } | null = null;
const roomProjects = () => [...new Map([...(detail?.workflows || []), ...projectJobs.values()].filter(job => job.conversation_id === selectedId).map(job => [job.id, job])).values()].sort((a, b) => b.created_at - a.created_at);
const currentWorkflow = () => roomProjects()[0] || null;
const projectAgentBusy = (id: string) => [...projectJobs.values()].some(job => workflowBusy(job) && job.attempts.some(attempt => attempt.agent_id === id && ['starting', 'running', 'cancelling'].includes(attempt.status)));
function adoptProject(job: Workflow, event = false) {
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
const runBusy = () => busySnapshot(currentRuntime());
const nativeChat = () => detail?.conversation.kind === 'direct' && ['codex-win', 'hermes-win', 'dsh-win', 'albion-wsl'].includes(currentAgentId());
const connectionLabel = () => currentRuntime().connection === 'connected' ? '接口已连接' : currentRuntime().connection === 'connecting' ? '正在连接' : '未连接';
function adoptRuntime(snapshot: RuntimeSnapshot, agentId = currentAgentId()) {
  const old = runtimes[agentId];
  if (old && snapshot.revision < old.revision) return false;
  runtimes[agentId] = snapshot;
  return true;
}

app.innerHTML = `
  <div class="shell">
    <aside class="sidebar">
      <div class="brand" id="window-drag-region" title="拖动窗口，双击最大化">${brand}<div><strong>同席</strong><span>AGENT HUB</span></div><span class="version" id="app-version">…</span></div>
      <div class="sidebar-top"><span class="eyebrow">你的协作空间</span><button id="new-conversation" class="icon-button filled" aria-label="新建会话" title="新建会话">${icon('plus')}</button></div>
      <label class="search">${icon('search')}<input id="search" placeholder="搜索会话或消息" aria-label="搜索会话或消息" maxlength="120"><kbd>Ctrl K</kbd></label>
      <div class="filters" aria-label="会话类型"><button data-filter="all" class="selected">全部</button><button data-filter="direct">私聊</button><button data-filter="group">群聊</button></div>
      <div class="list-label"><span id="list-label">最近会话</span><button id="archive-filter" title="查看归档">${icon('archive')}<span>归档</span></button></div>
      <div id="conversation-list" class="conversation-list" aria-label="会话列表"></div>
      <div class="sidebar-bottom"><button id="service-link">${icon('settings')}<span>成员与服务</span><span class="count-pill">4</span></button><div class="local-indicator"><span class="status-dot"></span>本地工作空间<span>桌面版</span></div></div>
    </aside>
    <main id="main" class="main"><div class="loading">正在打开你的工作空间…</div></main>
  </div>
  <div class="window-controls" aria-label="窗口操作"><button id="hud-toggle" aria-label="HUD 模式" title="HUD 模式">▣</button><button id="window-minimize" aria-label="最小化" title="最小化">−</button><button id="window-maximize" aria-label="最大化" title="最大化">□</button><button id="window-close" aria-label="关闭窗口" title="关闭窗口">×</button></div>
  ${(['North', 'South', 'East', 'West', 'NorthEast', 'NorthWest', 'SouthEast', 'SouthWest'] as const).map(direction => `<div class="resize-edge resize-${direction}" data-resize="${direction}" aria-hidden="true"></div>`).join('')}
  <div id="toast" class="toast" role="status" aria-live="polite"></div>
  <dialog id="modal" class="modal"></dialog>
`;

const main = document.querySelector<HTMLDivElement>('#main')!;
main.addEventListener('focusout', () => { setTimeout(() => renderLiveSettings(), 0); });
const modal = document.querySelector<HTMLDialogElement>('#modal')!;

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
    const applyTyping = () => document.documentElement.toggleAttribute('data-hud-typing', !!(document.querySelector('#message-input:focus') || (modal.open && modal.contains(document.activeElement))));
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
    if (input?.id !== 'message-input') return;
    resizeComposer(input);
  });
  const action = (work: () => Promise<unknown>) => void work().catch(error => toast(errorText(error), true));
  document.querySelector('#window-minimize')!.addEventListener('click', () => action(() => window.hide()));
  document.querySelector('#window-maximize')!.addEventListener('click', () => action(() => window.toggleMaximize()));
  document.querySelector('#window-close')!.addEventListener('click', () => action(() => window.close()));
  const hudButton = document.querySelector<HTMLButtonElement>('#hud-toggle')!;
  if (document.documentElement.dataset.hud === '1') {
    hudButton.hidden = true;
    // 退出按钮嵌在输入栏里，跟着会话一起重渲染，所以走委托。
    document.addEventListener('click', event => {
      if (!(event.target as Element | null)?.closest('#hud-exit')) return;
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
    if (target.closest('#window-drag-region, .chat-header, html[data-hud] .composer')) {
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
  return agents.find(agent => agent.id === id) || { id, name: id, subtitle: '', role: '', location: '', accent: '', status: 'not_connected' };
}
function time(timestamp: number) { return new Intl.DateTimeFormat('zh-CN', { hour: '2-digit', minute: '2-digit' }).format(timestamp); }
function date(timestamp: number) { return new Intl.DateTimeFormat('zh-CN', { month: 'short', day: 'numeric' }).format(timestamp); }
function draftKey(id: string) { return `hub.draft.${id}`; }
function readDraft(id: string) { return localStorage.getItem(draftKey(id)) || ''; }

function renderList() {
  document.querySelectorAll<HTMLButtonElement>('[data-filter]').forEach(button => button.classList.toggle('selected', button.dataset.filter === filter));
  document.querySelector('#list-label')!.textContent = archived ? '归档会话' : '最近会话';
  document.querySelector('#archive-filter')!.classList.toggle('active', archived);
  const visible = conversations.filter(conversation => filter === 'all' || conversation.kind === filter);
  const list = document.querySelector<HTMLDivElement>('#conversation-list')!;
  list.innerHTML = visible.length ? visible.map(conversation => {
    const agent = conversation.kind === 'direct' ? member(conversation.members[0]) : null;
    return `<button class="conversation ${screen === 'chat' && conversation.id === selectedId ? 'active' : ''}" data-conversation="${escape(conversation.id)}">
      ${agent ? avatar(agent) : `<span class="avatar group-avatar">${icon('group')}</span>`}
      <span class="conversation-text"><span class="conversation-title">${escape(conversation.title)}</span><span class="conversation-preview">${escape(conversation.preview || (conversation.kind === 'group' ? `${conversation.members.length} 位成员 · 新的讨论` : agent!.subtitle))}</span></span>
      <span class="conversation-meta">${conversation.message_count ? `<span>${time(conversation.updated_at)}</span><small>${conversation.message_count}</small>` : '<span class="new-dot"></span>'}</span>
    </button>`;
  }).join('') : `<div class="list-empty">${search ? '没有找到匹配的会话' : archived ? '还没有归档会话' : '这里还没有会话'}<span>${search ? '试试其他关键词' : '点击上方 + 开始新的对话'}</span></div>`;
  list.querySelectorAll<HTMLButtonElement>('[data-conversation]').forEach(button => button.addEventListener('click', () => void selectConversation(button.dataset.conversation!)));
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
  localStorage.setItem('hub.selected', id);
  renderList();
  if (hudActive) void emit('hud:select', { id });
  try {
    const result = await invoke<ConversationDetail>('get_conversation', { id });
    if (generation !== detailGeneration || screen !== 'chat') return;
    detail = result;
    for (const job of result.workflows || []) adoptProject(job);
    renderChat();
  } catch (error) { if (generation === detailGeneration) toast(errorText(error), true); }
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
  input.style.height = 'auto';
  input.style.height = `${Math.min(input.scrollHeight, cap)}px`;
}

function renderMessage(message: Message) {
  const own = message.sender_id === 'user';
  const labels: Record<Message['status'], string> = { local_only: '已保存到本机 · 尚未发送', pending: '正在提交原生会话', delivered: '已提交原生会话', streaming: '正在等待或接收回复', completed: '回复完成 · 已保存', interrupted: '已停止 · 保留已有内容', failed: '运行失败 · 可以发送新消息重试' };
  // 后端推理块，放在正文之前；没有就不占位。
  // 思考中保持展开（Hermes 那种边想边看，内容随流式增长）；回复结束自动收成一行。
  const thought = (message.thought || '').trim();
  const thinking = message.status === 'streaming' && thought.length > 0;
  const thoughtHtml = thought
    ? `<details class="thought"${thinking ? ' open' : ''}><summary>${thinking ? '思考中…' : '思考过程'}</summary><div class="thought-body">${escape(thought)}</div></details>`
    : '';
  return `<article class="message ${own ? 'own' : 'agent-message'}" data-message-id="${escape(message.id)}"><div class="message-author"><strong>${own ? '你' : escape(member(message.sender_id)?.name || '成员')}</strong><time>${time(message.created_at)}</time></div>${thoughtHtml}<div class="bubble">${escape(message.content || (message.status === 'streaming' ? '正在等待回复…' : message.status === 'interrupted' ? '已停止，尚无回复内容。' : '本次运行未产生回复内容。'))}</div><div class="message-delivery">${icon('check')}${labels[message.status]}</div></article>`;
}

function updateRuntimeControls() {
  if (screen !== 'chat' || !detail || detail.conversation.id !== selectedId) return;
  const isNative = nativeChat();
  const currentRun = currentRuntime().active?.conversation_id === selectedId && runBusy();
  const badge = main.querySelector('.connection-badge');
  if (badge) badge.innerHTML = `<span class="status-dot ${isNative && currentRuntime().connection === 'connected' ? '' : 'muted'}"></span>${isNative ? connectionLabel() : '待接入'}`;
  main.querySelectorAll('.member-card').forEach((card, index) => {
    const snapshot = runtimes[detail!.conversation.members[index]];
    if (snapshot) card.querySelector('small')!.innerHTML = `<span class="status-dot ${snapshot.connection === 'connected' ? '' : 'muted'}"></span>${snapshot.connection === 'connected' ? '接口已连接' : snapshot.connection === 'connecting' ? '正在连接' : '未连接'} · ${escape(member(detail!.conversation.members[index]).location)}`;
  });
  const phase = main.querySelector('.phase-progress > span');
  if (phase) phase.textContent = '第七步 · 项目协作与验收';
  const connect = main.querySelector<HTMLButtonElement>(`#connect-${harnessKey()}`);
  if (connect) { connect.hidden = currentRuntime().connection === 'connected'; connect.disabled = currentRuntime().connection === 'connecting'; connect.textContent = currentRuntime().connection === 'connecting' ? '正在连接…' : `连接 ${member(currentAgentId()).name}`; }
  const send = main.querySelector<HTMLButtonElement>(`#send-${harnessKey()}`);
  if (send) send.disabled = detail.conversation.archived || sending || settingsPending > 0 || runBusy() || projectAgentBusy(currentAgentId()) || currentRuntime().connection !== 'connected';
  const cancel = main.querySelector<HTMLButtonElement>(`#cancel-${harnessKey()}`);
  if (cancel) { cancel.hidden = !currentRun; cancel.disabled = currentRuntime().active?.status === 'cancelling'; cancel.textContent = currentRuntime().active?.status === 'cancelling' ? '正在停止…' : '停止回复'; }
  const note = main.querySelector('#runtime-note');
  if (note && isNative) note.textContent = currentRuntime().error || (currentRun ? (currentRuntime().active?.status === 'starting' ? '正在准备原生会话…' : currentRuntime().active?.status === 'cancelling' ? '等待原生停止确认…' : `正在接收 ${member(currentAgentId()).name} 回复…`) : runBusy() ? `${member(currentAgentId()).name} 正在其他会话回复，完成或停止后可发送。` : `${member(currentAgentId()).name} 私聊已接入；${currentAgentId() === 'hermes-win' ? '当前仅聊天，工具未启用' : '当前为只读对话模式'}，尚未绑定项目。`);
  renderLiveSettings();
  updateDiscussionControls();
  updateProjectControls();
  for (const snapshot of Object.values(runtimes)) {
    const active = snapshot.active;
    if (active?.conversation_id !== selectedId) continue;
    const messages = main.querySelector('#messages')!;
    const old = messages.querySelector(`[data-message-id="${active.assistant_message_id}"]`);
    const html = renderMessage({ id: active.assistant_message_id, conversation_id: selectedId, sender_id: active.agent_id, content: active.text, thought: active.thought, status: busySnapshot(snapshot) ? 'streaming' : active.status as Message['status'], created_at: detail.messages.find(message => message.id === active.assistant_message_id)?.created_at || Date.now() });
    const nearBottom = messages.scrollHeight - messages.scrollTop - messages.clientHeight < 120;
    if (old) old.outerHTML = html;
    else { messages.querySelector('.welcome')?.remove(); messages.insertAdjacentHTML('beforeend', html); }
    if (nearBottom) pinToBottom(messages);
  }
}

async function connectAgent(agentId = currentAgentId()) {
  try { adoptRuntime(await invoke<RuntimeSnapshot>(`connect_${harnessKey(agentId)}`), agentId); updateRuntimeControls(); if (screen === 'services') renderServices(); toast(`${member(agentId).name} 原生接口已连接`); }
  catch (error) { toast(errorText(error), true); }
}
async function disconnectAgent(agentId = currentAgentId()) {
  try { adoptRuntime(await invoke<RuntimeSnapshot>(`disconnect_${harnessKey(agentId)}`), agentId); updateRuntimeControls(); if (screen === 'services') renderServices(); toast(`${member(agentId).name} 已断开`); }
  catch (error) { toast(errorText(error), true); }
}

async function sendNative() {
  if (!detail || !nativeChat() || sending || settingsPending > 0 || runBusy() || projectAgentBusy(currentAgentId()) || detail.conversation.archived || currentRuntime().connection !== 'connected') return;
  const input = main.querySelector<HTMLTextAreaElement>('#message-input')!;
  const content = input.value.trim();
  if (!content) { input.focus(); return; }
  const id = detail.conversation.id;
  if (!pendingSend || pendingSend.conversationId !== id || pendingSend.content !== content) pendingSend = { conversationId: id, messageId: crypto.randomUUID(), content };
  const request = pendingSend;
  sending = true; updateRuntimeControls();
  try {
    await invoke<RunRecord>(`send_${harnessKey()}_message`, request);
    pendingSend = null;
    if (readDraft(id).trim() === content) localStorage.removeItem(draftKey(id));
    await refreshList();
    if (selectedId === id && screen === 'chat') await selectConversation(id);
  } catch (error) { toast(errorText(error), true); }
  finally { sending = false; updateRuntimeControls(); }
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
  if (!detail || detail.conversation.kind !== 'group' || screen !== 'chat') return;
  const host = main.querySelector<HTMLElement>('#discussion-controls');
  const groupSend = main.querySelector<HTMLButtonElement>('#send-discussion');
  if (detail.project) {
    if (host) host.hidden = true;
    const ready = detail.conversation.members.filter(agent => agent !== 'albion-wsl').every(agent => runtimes[agent]?.connection === 'connected' && !busySnapshot(runtimes[agent]) && !projectAgentBusy(agent));
    if (groupSend) {
      groupSend.textContent = '发送给项目组';
      groupSend.disabled = !ready || workflowBusy(currentWorkflow()) || discussionBusy(currentDiscussion()) || discussionBusy(latestDiscussion) || sending || groupConnecting || settingsPending > 0 || detail.conversation.archived;
    }
    const save = main.querySelector<HTMLButtonElement>('#save-message');
    if (save) save.disabled = detail.conversation.archived || sending || workflowBusy(currentWorkflow());
    const note = main.querySelector('#runtime-note');
    if (note) note.textContent = ready ? '项目群聊 · 直接发消息。Agent 会结合本轮目标讨论；需要改动时自动分工、实现并验收。' : '请连接项目群成员后开始协作。';
    const badge = main.querySelector('.connection-badge');
    if (badge) badge.innerHTML = `<span class="status-dot ${ready ? '' : 'muted'}"></span>${workflowBusy(currentWorkflow()) ? '项目协作中' : ready ? '项目组已连接' : '请连接项目成员'}`;
    return;
  }
  if (host) host.hidden = false;
  const job = currentDiscussion();
  const busy = discussionBusy(job);
  const options = groupOptions();
  const allReady = options.participants.length >= 2 && options.participants.every(id => runtimes[id]?.connection === 'connected' && !busySnapshot(runtimes[id]) && !projectAgentBusy(id));
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
  if (status) status.textContent = job ? `${labels[job.status]} · ${job.turns.filter(turn => turn.status === 'completed').length}/${job.participants.length * job.rounds} 次发言${active ? ` · 第 ${active.round} 轮 ${member(active.agent_id).name}` : ''}${job.error ? ` · ${job.error}` : ''}` : anotherBusy ? '另一个群聊正在讨论，请等待或在该群停止。' : '选择二至四位成员，按顺序共享群内发言。阿尔比恩需要显式邀请。';
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
  if (!detail || detail.conversation.kind !== 'group' || sending || detail.conversation.archived || workflowBusy(currentWorkflow()) || discussionBusy(currentDiscussion()) || discussionBusy(latestDiscussion)) return;
  const id = detail.conversation.id;
  const content = main.querySelector<HTMLTextAreaElement>('#message-input')!.value.trim();
  if (!content) return;
  if (detail.project) {
    const members = detail.conversation.members.filter(agent => agent !== 'albion-wsl');
    if (members.some(agent => runtimes[agent]?.connection !== 'connected' || busySnapshot(runtimes[agent]) || projectAgentBusy(agent))) {
      toast('请先连接项目群成员，并等待当前任务完成', true);
      return;
    }
    if (!pendingProject || pendingProject.conversationId !== id || pendingProject.content !== content) pendingProject = { conversationId: id, messageId: crypto.randomUUID(), content };
    sending = true; updateRuntimeControls();
    try {
      adoptProject(await invoke<Workflow>('start_project', { conversationId: id, messageId: pendingProject.messageId, content }));
      pendingProject = null;
      if (readDraft(id).trim() === content) localStorage.removeItem(draftKey(id));
      await refreshList();
      if (screen === 'chat' && selectedId === id) await selectConversation(id);
    } catch (error) { toast(errorText(error), true); }
    finally { sending = false; updateRuntimeControls(); }
    return;
  }
  const { participants, rounds } = groupOptions();
  if (participants.length < 2 || participants.some(id => runtimes[id].connection !== 'connected' || busySnapshot(runtimes[id]) || projectAgentBusy(id))) { toast('请先连接二至四位参与成员，并等待当前回复或项目任务完成', true); return; }
  if (!pendingDiscussion || pendingDiscussion.conversationId !== id || pendingDiscussion.content !== content || pendingDiscussion.rounds !== rounds || JSON.stringify(pendingDiscussion.participants) !== JSON.stringify(participants)) pendingDiscussion = { conversationId: id, messageId: crypto.randomUUID(), content, participants, rounds };
  sending = true; updateRuntimeControls();
  try {
    const job = await invoke<Discussion>('start_discussion', pendingDiscussion);
    adoptDiscussion(job); pendingDiscussion = null;
    if (readDraft(id).trim() === content) localStorage.removeItem(draftKey(id));
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

function updateProjectControls() {
  if (screen !== 'chat' || !detail || detail.conversation.id !== selectedId) return;
  const job = currentWorkflow();
  const busy = workflowBusy(job);
  const bind = main.querySelector<HTMLButtonElement>('#bind-project');
  const projectMembers = detail.conversation.members.filter(agent => ['codex-win', 'hermes-win', 'dsh-win'].includes(agent));
  const eligible = detail.conversation.kind === 'group' && projectMembers.length >= 2;
  if (bind) { bind.disabled = !eligible || busy || discussionBusy(currentDiscussion()) || detail.conversation.archived; bind.textContent = detail.project ? '更换项目目录' : '选择项目目录'; }
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
    share.title = enabled ? '停止把该项目的开发摘要共享给阿尔比恩（只影响该项目）' : '把该项目的开发摘要共享给阿尔比恩（只影响该项目，从之后的消息开始生效）';
  }
  const messages = main.querySelector('#messages');
  if (!messages) return;
  for (const workflow of roomProjects().reverse()) {
    const state = workflow.attempts.some(attempt => attempt.native_turn_id) ? 'delivered' : workflowBusy(workflow) ? 'pending' : 'failed';
    let user = messages.querySelector<HTMLElement>(`[data-message-id="${workflow.user_message_id}"]`);
    if (!user) { messages.querySelector('.welcome')?.remove(); messages.insertAdjacentHTML('beforeend', renderMessage({ id: workflow.user_message_id, conversation_id: workflow.conversation_id, sender_id: 'user', content: workflow.request, status: state, created_at: workflow.created_at })); user = messages.querySelector(`[data-message-id="${workflow.user_message_id}"]`)!; }
    const current = messages.querySelector<HTMLElement>(`[data-workflow-id="${workflow.id}"]`);
    const stamp = `${workflow.updated_at}:${workflow.status}:${workflow.tasks.map(task => task.agent_id).join(',')}:${workflow.attempts.map(attempt => `${attempt.status}:${attempt.output.length}`).join('|')}:${workflow.changes.length}`;
    if (user.dataset.projectDelivery !== state) { user.dataset.projectDelivery = state; const delivery = user.querySelector('.message-delivery'); if (delivery) delivery.innerHTML = `${icon('check')}${state === 'delivered' ? '已提交原生会话' : state === 'pending' ? '等待项目调度' : '项目请求未完成'}`; }
    if (current?.dataset.paint === stamp) continue;
    const open = new Map([...current?.querySelectorAll<HTMLDetailsElement>('details[data-attempt-id]') || []].map(node => [node.dataset.attemptId!, node.open]));
    const nearBottom = messages.scrollHeight - messages.scrollTop - messages.clientHeight < 100;
    const html = workflowCard(workflow, escape, id => member(id)?.name || id, (id, value) => runtimes[id]?.models.find(model => model.id === value)?.name || value || '沿用本机默认', id => runtimes[id]?.models || [], roleMembers('implement'));
    if (current) current.outerHTML = html; else user.insertAdjacentHTML('afterend', html);
    const card = messages.querySelector<HTMLElement>(`[data-workflow-id="${workflow.id}"]`)!;
    card.dataset.paint = stamp;
    card.querySelectorAll<HTMLDetailsElement>('[data-attempt-id]').forEach(node => { if (open.has(node.dataset.attemptId!)) node.open = open.get(node.dataset.attemptId!)!; });
    card.querySelector('[data-stop-project]')?.addEventListener('click', () => void cancelProject(workflow.id));
    card.querySelector('[data-continue-project]')?.addEventListener('click', () => void continueProject(workflow.id));
    card.querySelector('[data-edit-project-roles]')?.addEventListener('click', () => void editPausedProjectRoles(workflow.id));
    card.querySelector('[data-confirm-project]')?.addEventListener('click', () => void confirmProject(workflow.id, card));
    card.querySelectorAll<HTMLSelectElement>('[data-task-agent-select]').forEach(select => select.addEventListener('change', () => {
      const row = select.closest<HTMLElement>('[data-task-position]')!;
      const agent = select.value;
      const models = runtimes[agent]?.models || [];
      const providers = modelProviders(models);
      const rolePreset = agent === row.dataset.roleAgent && row.dataset.roleModel ? row.dataset.roleModel : '';
      const planPreset = agent === row.dataset.planAgent && row.dataset.planModel ? row.dataset.planModel : '';
      const presetModel = rolePreset || planPreset;
      const presetEffort = rolePreset ? row.dataset.roleEffort || '' : planPreset ? row.dataset.planEffort || '' : '';
      const provider = selectedProvider(models, presetModel);
      row.dataset.taskAgent = agent;
      row.classList.toggle('multi-provider', providers.length > 1);
      row.querySelector<HTMLElement>('[data-task-provider-wrap]')!.hidden = providers.length < 2;
      row.querySelector<HTMLSelectElement>('[data-task-provider]')!.innerHTML = providerOptionsHtml(models, provider, escape);
      const modelSelect = row.querySelector<HTMLSelectElement>('[data-task-model]')!;
      modelSelect.innerHTML = modelOptionsHtml(models, provider, presetModel || null, escape, '自动选型');
      const efforts = models.find(item => item.id === presetModel)?.efforts || Object.keys(effortLabels);
      row.querySelector<HTMLSelectElement>('[data-task-effort]')!.innerHTML = `<option value="">自动</option>${efforts.map(value => `<option value="${escape(value)}" ${value === presetEffort ? 'selected' : ''}>${escape(effortLabels[value] || value)}</option>`).join('')}`;
    }));
    card.querySelectorAll<HTMLSelectElement>('[data-task-provider]').forEach(select => select.addEventListener('change', () => {
      const row = select.closest<HTMLElement>('[data-task-position]')!;
      const agent = row.dataset.taskAgent || '';
      row.querySelector<HTMLSelectElement>('[data-task-model]')!.innerHTML = modelOptionsHtml(runtimes[agent]?.models || [], select.value, null, escape, '自动选型');
      row.querySelector<HTMLSelectElement>('[data-task-effort]')!.innerHTML = '<option value="">自动</option>';
    }));
    // 换模型后强度选项跟着该模型的可用档位走，旧选择作废。
    card.querySelectorAll<HTMLSelectElement>('[data-task-model]').forEach(select => select.addEventListener('change', () => {
      const row = select.closest<HTMLElement>('[data-task-position]')!;
      const efforts = runtimes[row.dataset.taskAgent || '']?.models.find(item => item.id === select.value)?.efforts || Object.keys(effortLabels);
      row.querySelector<HTMLSelectElement>('[data-task-effort]')!.innerHTML = `<option value="">自动</option>${efforts.map(value => `<option value="${escape(value)}">${escape(effortLabels[value] || value)}</option>`).join('')}`;
    }));
    if (nearBottom) messages.scrollTop = messages.scrollHeight;
  }
}

async function showProjectBinding() {
  if (!detail) return;
  try { await bindingDialog(detail.conversation, detail.project, { modal, escape, open: openModal, saved: async id => { if (selectedId === id && screen === 'chat') await selectConversation(id); toast('项目绑定已更新'); } }); }
  catch (error) { toast(errorText(error), true); }
}

/// 角色可选成员：只列当前群中已连接、且有原生模型目录的 Windows 项目成员。
const roleMembers = (_role: 'plan' | 'implement' | 'review') => {
  if (!detail) return [];
  const capable = ['codex-win', 'hermes-win', 'dsh-win'];
  return detail.conversation.members.filter(id => capable.includes(id) && runtimes[id]?.connection === 'connected' && runtimes[id]?.models.length > 0);
};

/// 逐任务确认：把卡片里选定的模型与强度随任务一起提交，随后开始执行。
async function confirmProject(workflowId: string, card: HTMLElement) {
  const tasks: TaskChoice[] = [...card.querySelectorAll<HTMLElement>('[data-task-position]')].map(row => ({
    position: Number(row.dataset.taskPosition),
    agent_id: row.querySelector<HTMLSelectElement>('[data-task-agent-select]')!.value,
    model: row.querySelector<HTMLSelectElement>('[data-task-model]')!.value || null,
    effort: row.querySelector<HTMLSelectElement>('[data-task-effort]')!.value || null,
  }));
  const button = card.querySelector<HTMLButtonElement>('[data-confirm-project]');
  if (button) { button.disabled = true; button.textContent = '正在开始…'; }
  try { adoptProject(await invoke<Workflow>('confirm_project', { workflowId, tasks })); updateRuntimeControls(); toast('已确认任务参数，开始执行'); }
  catch (error) {
    toast(errorText(error), true);
    if (button?.isConnected) { button.disabled = false; button.textContent = '确认并开始执行'; }
  }
}

async function cancelProject(id: string) {
  try { adoptProject(await invoke<Workflow>('cancel_project', { id })); updateRuntimeControls(); }
  catch (error) { toast(errorText(error), true); }
}

async function continueProject(id: string) {
  const button = main.querySelector<HTMLButtonElement>(`[data-continue-project="${CSS.escape(id)}"]`);
  if (button) { button.disabled = true; button.textContent = '正在恢复原方案…'; }
  try {
    adoptProject(await invoke<Workflow>('continue_project', { workflowId: id }));
    updateRuntimeControls();
    toast('已复用原方案，请确认任务执行成员与模型后继续');
  } catch (error) {
    toast(errorText(error), true);
    if (button?.isConnected) { button.disabled = false; button.textContent = '继续协作（复用原方案）'; }
  }
}

async function editPausedProjectRoles(id: string) {
  const workflow = [...projectJobs.values()].find(job => job.id === id);
  if (!workflow) { toast('找不到这项暂停的协作', true); return; }
  const roles = await planningDialog({
    modal, escape, name: agentId => member(agentId)?.name || agentId,
    members: roleMembers, catalog: agentId => runtimes[agentId]?.models || [], open: openModal,
    initial: workflowRoles(workflow.roles), title: '调整暂停协作的阶段设置', submitLabel: '保存阶段设置',
    note: '可修改各阶段的成员、模型与思考强度。已生成的规划会保留，不会重新调用规划模型；继续时还可以为未完成任务逐项选择执行者和模型。',
  });
  if (!roles) return;
  try {
    adoptProject(await invoke<Workflow>('update_paused_project_roles', { workflowId: id, roles }));
    toast('阶段设置已保存；继续时可再逐项调整未完成任务');
  } catch (error) { toast(errorText(error), true); }
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
    toast(enabled ? '该项目开发摘要已共享给阿尔比恩 · 从之后的消息开始生效' : '该项目开发摘要已停止共享');
  } catch (error) { toast(errorText(error), true); }
  finally {
    if (pendingSummary?.projectId === projectId) pendingSummary = null;
    updateProjectControls();
  }
}

function renderChat() {
  if (!detail) {
    main.innerHTML = `<div class="no-selection"><span class="eyebrow">同席 · 本地协作</span><h1>为下一个想法，留一个位置。</h1><p>选择一个会话，或创建新的私聊与群聊。</p><button class="primary" id="empty-new">${icon('plus')}新建会话</button></div>`;
    main.querySelector('#empty-new')!.addEventListener('click', () => showCreate());
    return;
  }
  const conversation = detail.conversation;
  const roster = conversation.members.map(member);
  const isGroup = conversation.kind === 'group';
  const directAgent = conversation.kind === 'direct' ? roster[0] : null;
  const isNative = nativeChat();
  const key = harnessKey();
  const name = directAgent?.name || ''; 
  main.innerHTML = `<header class="chat-header"><div class="chat-heading">${directAgent ? avatar(directAgent) : `<span class="avatar group-avatar">${icon('group')}</span>`}<div><h1>${escape(conversation.title)}</h1><p>${escape(directAgent ? directAgent.subtitle : `${roster.length} 位成员 · 共同讨论，分别思考`)}${conversation.archived ? ' · 已归档' : ''}</p></div></div><div class="header-actions"><span class="connection-badge"><span class="status-dot muted"></span>待接入</span>${conversation.kind === 'direct' ? `<button id="native-sessions" class="secondary" title="查看该成员自己的历史会话（只读，不会改动它）">原生会话</button>` : ''}${conversation.kind === 'direct' && clientAgents.includes(conversation.members[0]) ? `<button id="open-client" class="secondary" title="打开这位成员自己的客户端">打开客户端</button>` : ''}<button id="conversation-menu" class="icon-button" aria-label="会话操作" title="会话操作">${icon('more')}</button></div></header>
    <div class="chat-layout"><section class="chat-content"><div class="milestone-note"><span class="note-mark">07</span><span id="runtime-note">四位成员真实接入，私聊与群聊使用独立上下文。</span>${isNative ? `<button id="connect-${key}" class="secondary">连接 ${escape(name)}</button>` : ''}</div>
      <div id="live-settings" class="live-settings" aria-label="当前会话模型与思考强度"></div>${isGroup ? '<div id="discussion-controls" class="discussion-controls" aria-label="群聊设置"></div><div id="project-controls" class="project-controls"><div class="project-entry-copy"><strong id="project-title">项目协作 · 先绑定项目目录</strong><small>绑定后，直接在本群讨论需求或推动实现。</small></div><div class="project-entry-actions"><button id="bind-project" type="button" class="secondary">选择项目目录</button><button id="connect-project-members" type="button" class="secondary" hidden>连接项目成员</button><button id="share-project-summary" type="button" class="text-button" aria-pressed="false" hidden>开发摘要：未共享</button></div></div>' : ''}<div id="messages" class="messages" aria-label="聊天消息">
      ${detail.messages.length ? `<div class="date-divider">${date(detail.messages[0].created_at)}</div>${detail.messages.map(renderMessage).join('')}` : `<div class="welcome"><div class="seat-illustration"><span></span><span></span><span></span><span></span><div>${icon(directAgent ? 'chat' : 'group')}</div></div><span class="eyebrow">${directAgent ? '留一个安静的对话空间' : '把想法带到同一张桌上'}</span><h2>${directAgent ? (directAgent.id === 'albion-wsl' ? '聊聊今天，也聊聊正在做的事。' : `从一次与 ${escape(directAgent.name)} 的对话开始。`) : '一个问题，几种视角。'}</h2><p>${directAgent ? escape(directAgent.role) : '让管家梳理需求，让实现者把方案变成结果。<br>会话与成员已经分开，新的讨论从这里开始。'}</p><div class="suggestions">${(directAgent?.id === 'albion-wsl' ? ['记录今天的开发进展', '聊聊今天发生的事情'] : ['梳理一个新的开发需求', '记录一个待解决的问题', '整理接下来的计划']).map(text => `<button data-suggestion="${escape(text)}">${escape(text)}${icon('arrow')}</button>`).join('')}</div></div>`}
      </div>
      <form id="composer" class="composer"><div class="composer-top"><span>${conversation.archived ? '会话已归档' : isNative ? `${name} 私聊 · ${key === 'hermes' ? '仅聊天' : '只读对话'}` : detail.project ? '项目群聊 · 讨论与开发共用同一空间' : '群聊讨论'}</span><span>${roster.map(agent => escape(agent.name)).join(' · ')}</span></div><textarea id="message-input" maxlength="16000" rows="1" aria-label="消息内容" placeholder="${conversation.archived ? '恢复会话后，可以继续记录。' : detail.project ? '描述目标、讨论方案，或让项目组推进实现…' : '写下你的需求、问题，或今天的想法…'}" ${conversation.archived ? 'disabled' : ''}></textarea><div class="composer-bottom"><span>${conversation.archived ? '归档会保留全部历史' : isNative ? '连接后 Enter 发送 · Shift + Enter 换行' : detail.project ? 'Enter 发送给项目组 · Shift + Enter 换行' : 'Enter 开始群聊 · Shift + Enter 换行'}</span><div class="composer-actions"><button id="hud-model" type="button" class="icon-button" title="模型与思考强度" aria-label="模型与思考强度">⌄</button><button id="hud-exit" type="button" class="icon-button" aria-label="退出 HUD" title="退出 HUD（回到主窗口）">×</button>${isNative ? `<button id="cancel-${key}" type="button" class="secondary" hidden>停止回复</button>` : `<button id="cancel-discussion" type="button" class="secondary" hidden>停止讨论</button>`}<button id="save-message" type="button" class="secondary" ${conversation.archived ? 'disabled' : ''}>保存草稿</button>${isNative ? `<button id="send-${key}" class="primary" disabled>发送给 ${escape(name)}${icon('arrow')}</button>` : `<button id="send-discussion" class="primary" disabled>${detail.project ? '发送给项目组' : '开始群聊'}${icon('arrow')}</button>`}</div></div></form>
      <footer class="chat-footer">本地保存，独立会话。你的私聊不会自动进入群聊。</footer>
    </section><aside class="members-panel"><div class="panel-title"><span>会话成员</span>${conversation.kind === 'group' ? `<button class="text-button" id="edit-members">管理</button>` : ''}</div><div class="member-list">${roster.map(agent => `<div class="member-card">${avatar(agent)}<div><strong>${escape(agent.name)}</strong><span>${escape(agent.subtitle)}</span><small><span class="status-dot muted"></span>待接入 · ${escape(agent.location)}</small></div></div>`).join('')}</div><div class="workspace-note"><span class="eyebrow">会话边界</span><h3>各自的上下文<br>共同的讨论空间</h3><p>每位成员使用独立的会话映射。加入群聊不会合并已有私聊。</p>${conversation.kind === 'group' ? `<div class="tiny-rule"></div><p>阿尔比恩可以受邀参加，也保留与你单独交流的空间。</p>` : ''}</div><div class="phase-progress"><span>第一步 · 桌面与会话</span><div><i></i><i></i><i></i><i></i></div><small>接下来：开发摘要与更新管理</small></div></aside></div>`;
  renderDiscussionControls();
  main.querySelector('#bind-project')?.addEventListener('click', () => void showProjectBinding());
  main.querySelector('#connect-project-members')?.addEventListener('click', () => void connectProjectMembers());
  main.querySelector('#share-project-summary')?.addEventListener('click', () => void toggleProjectSummary());
  main.querySelector('#cancel-discussion')?.addEventListener('click', () => void cancelDiscussion());
  // HUD 里模型/强度收成一个小小的 ⌄（像 Hermes 那样），点开仍是同一套设置。
  main.querySelector('#hud-model')?.addEventListener('click', () => showModelSettings());
  main.querySelector('#native-sessions')?.addEventListener('click', () => void showNativeSessions(conversation.members[0]));
  main.querySelector('#open-client')?.addEventListener('click', () => void openClient(conversation.members[0]));
  document.querySelector('#conversation-menu')!.addEventListener('click', () => showConversationActions());
  document.querySelector('#edit-members')?.addEventListener('click', () => showMembers());
  main.querySelector(`#connect-${key}`)?.addEventListener('click', () => void connectAgent());
  main.querySelector(`#cancel-${key}`)?.addEventListener('click', () => void cancelNative());
  main.querySelector('#save-message')!.addEventListener('click', () => void saveMessage());
  const input = document.querySelector<HTMLTextAreaElement>('#message-input')!;
  input.value = readDraft(conversation.id);
  input.addEventListener('input', () => localStorage.setItem(draftKey(conversation.id), input.value));
  const submit = () => isGroup ? sendGroupMessage() : nativeChat() && currentRuntime().connection === 'connected' ? sendNative() : saveMessage();
  // IME confirmation must never send: track composition state for this textarea only.
  let composing = false;
  let composeGuardUntil = 0;
  input.addEventListener('compositionstart', () => { composing = true; });
  input.addEventListener('compositionend', () => { composing = false; composeGuardUntil = Date.now() + 50; });
  input.addEventListener('keydown', event => {
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
  const content = input.value.trim();
  if (!content) { input.focus(); return; }
  const id = detail.conversation.id;
  if (!pendingSave || pendingSave.conversationId !== id || pendingSave.content !== content) pendingSave = { conversationId: id, messageId: crypto.randomUUID(), content };
  const request = pendingSave;
  const button = document.querySelector<HTMLButtonElement>('#save-message')!;
  sending = true; button.disabled = true; button.textContent = '保存中…';
  try {
    await invoke<Message>('save_local_message', { ...request });
    pendingSave = null;
    // A switch during persistence must not clear another room's text or repaint it.
    if (readDraft(id).trim() === content) localStorage.removeItem(draftKey(id));
    await refreshList();
    if (selectedId === id && screen === 'chat') await selectConversation(id);
    toast('草稿已保存到本机，尚未发送给成员');
  } catch (error) { toast(errorText(error), true); }
  finally {
    sending = false;
    if (button.isConnected) { button.disabled = false; button.innerHTML = `保存草稿${icon('arrow')}`; }
  }
}

/// 能一键打开的客户端：Hermes 桌面端、codex 命令行、DSH 网页端（阿尔比恩不做）。
const clientAgents = ['hermes-win', 'codex-win', 'dsh-win'];

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
        const title = session.title?.trim() || `会话 ${session.id.slice(0, 8)}`;
        const action = session.current
          ? '<span class="native-session-state">当前会话</span>'
          : session.occupied_by
            ? `<span class="native-session-state">已被「${escape(session.occupied_by)}」占用</span>`
            : `<button class="secondary" data-attach="${escape(session.id)}" data-native-cwd="${escape(session.cwd || '')}">接入</button>`;
        return `<article class="native-session" data-session-id="${escape(session.id)}"><div class="native-session-main"><strong>${escape(title)}</strong><small>${escape(when)}${session.cwd ? ` · ${escape(session.cwd)}` : ''}</small><code>${escape(session.id)}</code></div>${action}</article>`;
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
      });
      if (detail?.conversation.id === saved.conversation.id) detail.sessions = saved.sessions;
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
  const host = main.querySelector<HTMLDivElement>('#live-settings');
  if (!host || !detail) return;
  const conversation = detail.conversation;
  const autoProjectSetting = conversation.kind === 'group' && !!detail.project;
  if (!conversation.members.includes(settingsAgentId)) settingsAgentId = conversation.members[0];
  const autoProjectModel = autoProjectSetting && settingsAgentId === 'codex-win';
  const session = detail.sessions.find(item => item.agent_id === settingsAgentId)!;
  const snapshot = runtimes[settingsAgentId];
  const connected = snapshot?.connection === 'connected';
  const catalog = connected ? snapshot.models : [];
  const signature = JSON.stringify([conversation.id, settingsAgentId, session.model, session.reasoning_effort, catalog, snapshot?.default_model, conversation.archived, autoProjectSetting, autoProjectModel]);
  const status = host.querySelector('#live-setting-status');
  if (status) status.textContent = settingsPending ? '正在保存…' : snapshot?.active?.conversation_id === conversation.id && busySnapshot(snapshot) ? '下一条生效 · 当前回复保留原参数' : snapshot?.connection === 'connected' ? '切换即保存 · 下一条消息生效' : '切换即保存 · 连接后应用';
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
        const saved = await invoke<ConversationDetail>('set_session_settings', { id, agentId, model: nextModel, reasoningEffort: nextEffort });
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
    model.innerHTML = modelOptionsHtml(catalog, providerSelect.value, null, escape, autoProjectModel ? '项目自动选型 · 聊天默认' : `默认${snapshot?.default_model ? ` · ${catalog.find(item => item.id === snapshot.default_model)?.name || snapshot.default_model}` : ''}`);
    const available = catalog.find(item => item.id === model.value)?.efforts || Object.keys(effortLabels);
    effort.innerHTML = `<option value="">默认</option>${available.map(value => `<option value="${escape(value)}">${escape(effortLabels[value] || value)}</option>`).join('')}`;
    effort.value = '';
    queueSave();
  });
  effort.addEventListener('change', queueSave);
  host.querySelector('#live-reset')!.addEventListener('click', () => { delete liveProviderSelection[providerKey]; model.value = ''; effort.value = ''; queueSave(); });
  const nextStatus = host.querySelector('#live-setting-status')!;
  nextStatus.textContent = snapshot?.active?.conversation_id === conversation.id && busySnapshot(snapshot) ? '下一条生效 · 当前回复保留原参数' : snapshot?.connection === 'connected' ? '切换即保存 · 下一条消息生效' : '切换即保存 · 连接后应用';
}

function showModelSettings(initialAgent?: string) {
  if (!detail) return;
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
    ? (agentId === 'codex-win' ? '选项来自本机 Codex 模型目录；实际调用成功才表示模型可用。' : '模型来自 Hermes 原生目录；思考强度传给当前 agent，由 Hermes 根据模型与提供商映射。')
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
  openModal('新建会话', '选择对话成员，为这个话题留一个独立空间。', `<form id="create-form"><div class="kind-switch"><button type="button" data-kind="direct" class="${initialKind === 'direct' ? 'selected' : ''}">${icon('chat')}私聊</button><button type="button" data-kind="group" class="${initialKind === 'group' ? 'selected' : ''}">${icon('group')}群聊</button></div><label class="field">会话名称<input name="title" required maxlength="80" placeholder="例如：框架开发 / 今天的想法" autocomplete="off"></label><span class="field-label">${initialKind === 'group' ? '选择二至四位成员' : '选择一位成员'}</span><div class="member-options">${memberOptions(initialKind === 'group' ? ['hermes-win', 'codex-win', 'dsh-win'] : ['codex-win'], initialKind === 'group' ? 'checkbox' : 'radio')}</div><p class="form-error" role="alert"></p><div class="modal-footer"><span>现有会话与私人记忆保持独立</span><button class="primary" type="submit">创建会话${icon('arrow')}</button></div></form>`);
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
  modal.querySelector('#archive-action')!.addEventListener('click', async () => {
    try { await invoke('archive_conversation', { id: conversation.id, archived: !conversation.archived }); modal.close(); archived = !conversation.archived; await refreshList(); await selectConversation(conversation.id); toast(conversation.archived ? '会话已恢复' : '会话已归档'); }
    catch (error) { toast(errorText(error), true); }
  });
  modal.querySelector('#delete-action')!.addEventListener('click', () => {
    modal.close();
    openModal('删除这个会话？', `“${conversation.title}”中的 ${conversation.message_count} 条本地消息和会话映射将被删除。`, `<p class="delete-notice">此操作不可撤销。其他会话保持独立。</p><div class="modal-footer"><button class="secondary" id="cancel-delete">保留会话</button><button class="danger-button" id="confirm-delete">确认删除</button></div>`);
    modal.querySelector('#cancel-delete')!.addEventListener('click', () => modal.close());
    modal.querySelector('#confirm-delete')!.addEventListener('click', async () => {
      const button = modal.querySelector<HTMLButtonElement>('#confirm-delete')!; button.disabled = true;
      try {
        await invoke('delete_conversation', { id: conversation.id }); localStorage.removeItem(draftKey(conversation.id)); modal.close();
        detail = null; selectedId = ''; localStorage.removeItem('hub.selected'); await refreshList();
        if (conversations[0]) await selectConversation(conversations[0].id); else renderChat(); toast('会话已删除');
      } catch (error) { toast(errorText(error), true); button.disabled = false; }
    });
  });
}

// `ServiceInfo` and the update-check result shapes live in `./types`.

// Card order matches the four managed members rendered by `renderServices`.
const serviceCardOrder = ['hermes-win', 'codex-win', 'dsh-win', 'albion-wsl'] as const;

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
  ++detailGeneration; screen = 'services'; renderList();
  main.innerHTML = `<header class="chat-header"><div><span class="eyebrow">你的协作成员</span><h1>成员与服务</h1></div><button id="back-to-chat" class="secondary">${icon('chat')}返回会话</button></header><div class="services-content"><div class="services-intro"><span class="eyebrow">四个角色，各有所长</span><h2>在同一个空间里，<br>保留各自的职责。</h2><p>DSH 与 Codex 支持隔离更新与回退（可准备候选后一键切换）· Hermes 只做只读核对、更新交接给它自己的通道 · 阿尔比恩核对本地版本</p><span class="phase-chip">四位成员已接入私聊 · 版本检查保持只读 · 更新需显式确认</span></div><div class="service-grid">${agents.map(agent => `<article class="service-card" data-service-id="${escape(agent.id)}">${avatar(agent, 'large')}<div class="service-card-heading"><h3>${escape(agent.name)}</h3><span>${escape(agent.location)}</span></div><strong>${escape(agent.subtitle)}</strong><p>${escape(agent.role)}</p><div class="service-status"><span class="status-dot muted"></span>待接入<span>版本未检测</span></div><div class="service-actions"><button data-service-connect class="secondary" disabled>连接</button><button data-service-check class="secondary" title="重新检测该服务的安装与运行版本">检查更新</button><button data-service-restart class="secondary" hidden title="停掉本软件自己启动的原生子进程并重新连接，期间拒绝新消息与任务">重启</button>${['dsh-win', 'codex-win'].includes(agent.id) ? `<button data-service-prepare class="secondary" disabled title="只在显式点击后下载并验证隔离候选，检查更新不会触发下载">准备更新</button><button data-service-apply class="secondary" disabled title="只在候选已验证就绪时启用；启用后由后端保持原有连接状态">启用候选</button><button data-service-rollback class="secondary" disabled title="只在存在可回退的上一版本时可用">回退上一版</button>` : ''}</div><div class="service-check-result" hidden></div>${['dsh-win', 'codex-win'].includes(agent.id) ? '<div class="service-update" hidden></div>' : ''}</article>`).join('')}</div><div class="storage-card"><div>${icon('settings')}<strong>独立的本地数据</strong><span>v${escape(info?.version || '0.1.0')}</span></div><p>会话与消息草稿保存到本机，现有 agent 的配置与服务不受影响。</p><code>${escape(info?.database_path || '')}</code></div></div>`;
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
      for (const agentId of ['codex-win', 'hermes-win', 'dsh-win', 'albion-wsl']) {
        await listen<RuntimeSnapshot>(`${harnessKey(agentId)}-state`, event => {
          const old = runtimes[agentId];
          if (!adoptRuntime(event.payload, agentId)) return;
          updateRuntimeControls();
          if (screen === 'services' && old.connection !== event.payload.connection) renderServices();
          const active = event.payload.active;
          if (active && !busySnapshot(event.payload) && `${active.id}:${active.status}` !== lastFinalRuns[agentId]) {
            lastFinalRuns[agentId] = `${active.id}:${active.status}`;
            void refreshList().then(() => { if (screen === 'chat' && selectedId === active.conversation_id) return selectConversation(selectedId); }).catch(error => toast(errorText(error), true));
            if (active.error) toast(active.error, true);
          }
        });
      }
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
    for (const agentId of ['codex-win', 'hermes-win', 'dsh-win', 'albion-wsl']) adoptRuntime(await invoke<RuntimeSnapshot>(`${harnessKey(agentId)}_status`), agentId);
    latestDiscussion = await invoke<Discussion | null>('discussion_status');
    for (const job of await invoke<Workflow[]>('project_status')) adoptProject(job);
    [agents, info] = await Promise.all([invoke<Agent[]>('list_agents'), invoke<AppInfo>('app_info')]);
    document.querySelector('#app-version')!.textContent = info.version;
    await refreshList();
    const initial = conversations.find(conversation => conversation.id === selectedId) || conversations.find(conversation => conversation.kind === 'group') || conversations[0];
    if (initial) await selectConversation(initial.id); else renderChat();
    controls.forEach(button => { button.disabled = false; });
    document.querySelector<HTMLInputElement>('#search')!.disabled = false;
  } catch (error) {
    main.innerHTML = `<div class="no-selection"><h1>本地工作空间未能打开。</h1><p>${escape(errorText(error))}</p><button id="retry-boot" class="primary">重试</button></div>`;
    main.querySelector('#retry-boot')!.addEventListener('click', () => void boot());
  }
}

void boot();
