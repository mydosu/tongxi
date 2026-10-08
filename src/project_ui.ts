import { invoke } from '@tauri-apps/api/core';
import type { Conversation, ExecutionChoice, ModelOption, Project, ProjectAgentChat, ProjectAttempt, ProjectTask, ProjectTeamMessage, RoleChoice, Roles, Workflow } from './types';
import { modelOptionsHtml, modelProviders, providerOptionsHtml, selectedProvider } from './model_select';

type Escape = (value: string | number) => string;
export const workflowBusy = (job: Workflow | null | undefined) => !!job && ['queued', 'planning', 'running', 'verifying', 'reviewing', 'cancelling'].includes(job.status);
export const projectLabels: Record<string, string> = { queued: '等待项目空闲', planning: '方案与任务分发', running: '正在实现', verifying: '正在实际验收', reviewing: '功能验收中', cancelling: '正在停止', completed: '协作完成', failed: '协作失败', interrupted: '协作已中断', starting: '准备会话', skipped: '未执行' };
const stages: Record<string, string> = { implement: '项目实现', review: '功能校验', repair: '复杂修复' };
const stageLabel = (attempt: ProjectAttempt) => attempt.stage === 'plan' ? attempt.agent_id === 'codex-win' ? '项目方案' : attempt.agent_id === 'hermes-win' ? '任务分发' : '项目方案' : attempt.stage === 'repair' && attempt.agent_id === 'codex-win' ? '疑难诊断' : attempt.stage === 'repair' && attempt.agent_id === 'dsh-win' ? '实际修复' : stages[attempt.stage] || attempt.stage;
const attemptStatusLabel = (attempt: ProjectAttempt) => attempt.status === 'completed' ? ({ plan: '方案完成', implement: '任务完成', review: '验收完成', repair: '修复完成' }[attempt.stage] || '步骤完成') : projectLabels[attempt.status] || attempt.status;
const taskStatusLabel = (task: ProjectTask, job: Workflow) => task.status === 'queued' && job.status === 'interrupted' && task.depends_on.some(position => job.tasks[position]?.status === 'interrupted') ? '等待依赖' : ({ queued: '待执行', running: '正在执行', completed: '任务完成', failed: '任务失败', interrupted: '已中断', skipped: '未执行' }[task.status] || task.status);
export function workflowRoles(value: Workflow['roles']): Roles | null {
  if (typeof value !== 'string') return value || null;
  try { const parsed = JSON.parse(value); return parsed && typeof parsed === 'object' ? parsed as Roles : null; } catch { return null; }
}
const effortLabels: Record<string, string> = { none: '关闭思考', off: '关闭思考', minimal: '最少', low: '低', medium: '中', high: '高', xhigh: '很高', max: '最大', ultra: 'Ultra' };
const roleNames: Record<'plan' | 'implement' | 'review', string> = { plan: '规划', implement: '执行', review: '验收' };
const effortOptions = (models: ModelOption[], modelId: string) => models.find(item => item.id === modelId)?.efforts || Object.keys(effortLabels);

/// 逐任务确认区的一行：标题、执行者、授权文件，加模型与强度两个下拉（预填规划给出的选型建议）。
function confirmRow(task: ProjectTask, suggestion: ExecutionChoice | null | undefined, roleDefault: RoleChoice | null, models: ModelOption[], eligibleAgents: string[], escape: Escape, name: (id: string) => string) {
  const roleModel = roleDefault?.model && models.some(item => item.id === roleDefault.model) ? roleDefault.model : '';
  const suggestedModel = suggestion && models.some(item => item.id === suggestion.model) ? suggestion.model : '';
  const model = task.model || roleModel || suggestedModel;
  const effort = task.effort || (roleModel && model === roleModel ? roleDefault?.effort : null) || (suggestedModel && model === suggestedModel ? suggestion?.reasoning_effort : null) || '';
  const provider = selectedProvider(models, model);
  const providers = modelProviders(models);
  const candidates = [...new Set([task.agent_id, ...eligibleAgents])];
  const agentOptions = candidates.map(agent => `<option value="${escape(agent)}" ${agent === task.agent_id ? 'selected' : ''} ${eligibleAgents.includes(agent) ? '' : 'disabled'}>${escape(name(agent))}${eligibleAgents.includes(agent) ? '' : ' · 未连接'}</option>`).join('');
  const files = task.files.length ? ` · ${task.files.map(escape).join(' · ')}` : '';
  const locked = task.status === 'completed';
  return `<div class="task-confirm-row ${providers.length > 1 ? 'multi-provider' : ''}" data-task-position="${task.position}" data-task-agent="${escape(task.agent_id)}" data-role-agent="${escape(roleDefault?.agent || '')}" data-role-model="${escape(roleDefault?.model || '')}" data-role-effort="${escape(roleDefault?.effort || '')}" data-plan-agent="${escape(suggestion ? task.agent_id : '')}" data-plan-model="${escape(suggestion?.model || '')}" data-plan-effort="${escape(suggestion?.reasoning_effort || '')}"><div class="task-confirm-title"><strong>${escape(task.title)}</strong><span>${escape(name(task.agent_id))}${files}</span></div><label>执行者<select data-task-agent-select aria-label="任务执行成员" ${locked ? 'disabled' : ''}>${agentOptions}</select></label><label data-task-provider-wrap ${providers.length > 1 ? '' : 'hidden'}>提供商<select data-task-provider aria-label="任务模型提供商" ${locked ? 'disabled' : ''}>${providerOptionsHtml(models, provider, escape)}</select></label><label>模型<select data-task-model aria-label="任务模型" ${locked ? 'disabled' : ''}>${modelOptionsHtml(models, provider, model || null, escape, '自动选型')}${model && !models.some(item => item.id === model) ? `<option value="${escape(model)}" selected>${escape(model)} · 当前目录不可用</option>` : ''}</select></label><label>强度<select data-task-effort aria-label="任务思考强度" ${locked ? 'disabled' : ''}><option value="">自动</option>${effortOptions(models, model).map(value => `<option value="${escape(value)}" ${value === effort ? 'selected' : ''}>${escape(effortLabels[value] || value)}</option>`).join('')}</select></label></div>`;
}

function confirmBlock(job: Workflow, escape: Escape, name: (id: string) => string, catalog: (id: string) => ModelOption[], eligibleAgents: string[]) {
  const roles = workflowRoles(job.roles);
  return `<div class="task-confirm"><div class="task-confirm-heading">规划完成 · 为每项任务选择 Codex、Hermes 或 DSH，并确认模型与思考强度</div>${job.tasks.map(task => {
    const planned = job.plan?.tasks[task.position];
    const suggestion = planned?.agent_id === task.agent_id ? planned.execution : null;
    const roleDefault = roles?.implement.agent === task.agent_id ? roles.implement : null;
    return confirmRow(task, suggestion, roleDefault, catalog(task.agent_id), eligibleAgents, escape, name);
  }).join('')}<button type="button" class="primary" data-confirm-project>确认并开始执行</button></div>`;
}

export function projectAgentActivity(job: Workflow, agentId: string, escape: Escape, name: (id: string) => string, model: (id: string, value: string | null) => string, avatar: (id: string) => string = id => `<span class="project-agent-avatar">${escape(name(id).slice(0, 1))}</span>`) {
  const attempts = job.attempts.filter(attempt => attempt.agent_id === agentId && attempt.stage !== 'verify');
  const active = attempts.some(attempt => ['starting','running','cancelling'].includes(attempt.status));
  const rows = attempts.map(attempt => {
    const running = ['starting','running','cancelling'].includes(attempt.status);
    return `<details class="agent-attempt" data-attempt-id="${escape(attempt.id)}" data-attempt-status="${escape(attempt.status)}" ${running ? 'open' : ''}><summary><span>${escape(stageLabel(attempt))}</span><span>${escape(attemptStatusLabel(attempt))}</span></summary><small>${escape(model(attempt.agent_id, attempt.model))}${attempt.reasoning_effort ? ` · ${escape(attempt.reasoning_effort)}` : ''}</small>${attempt.thought ? `<details class="attempt-thought" data-thought-attempt-id="${escape(attempt.id)}" data-active="${running}" ${running ? 'open' : ''}><summary class="attempt-thought-summary">${running ? '思考中' : '思考过程'}</summary><div class="attempt-thought-output" data-follow="${running}"><div class="attempt-thought-content">${escape(attempt.thought)}</div></div></details>` : ''}${attempt.tool_events?.length ? `<details class="attempt-tool-events"><summary>工具活动 · ${attempt.tool_events.length}</summary>${attempt.tool_events.map(event => `<details class="message-tool-event"><summary><span>${escape(event.title || event.kind)}</span><small>${escape(event.status === 'running' ? '进行中' : event.status === 'failed' ? '失败' : '完成')}</small></summary>${event.input ? `<pre>${escape(event.input)}</pre>` : ''}${event.output ? `<pre>${escape(event.output)}</pre>` : ''}</details>`).join('')}</details>` : ''}${attempt.output ? `<pre class="attempt-output">${escape(attempt.output)}</pre>` : ''}${attempt.error ? `<p class="workflow-error">${escape(attempt.error)}</p>` : ''}</details>`;
  }).join('');
  return `<section class="project-agent-activity" data-agent-activity="${escape(agentId)}"><header>${avatar(agentId)}<div><strong>${escape(name(agentId))} · 项目执行记录</strong><small>${active ? '正在执行；可单独中断' : '本项目中的思考、工具和输出记录'}</small></div>${active ? `<button type="button" class="text-button" data-stop-project-agent="${escape(agentId)}">中断此 Agent</button>` : ''}</header><div class="agent-task-log">${rows || '<p class="team-feed-empty">该 Agent 本轮尚未执行任务。</p>'}</div></section>`;
}

export function workflowCard(job: Workflow, escape: Escape, name: (id: string) => string, catalog: (id: string) => ModelOption[], eligibleAgents: string[], agentChats: ProjectAgentChat[] = [], teamMessages: ProjectTeamMessage[] = [], avatar: (id: string) => string = id => `<span class="project-agent-avatar">${escape(name(id).slice(0, 1))}</span>`) {
  const duplicateWorkflowError = !!job.error && (job.attempts.some(attempt => attempt.error === job.error) || job.tasks.some(task => task.error === job.error));
  const finalReviewer = job.status === 'completed' && [...job.attempts].reverse().find(attempt => attempt.stage === 'review' && attempt.status === 'completed')?.agent_id;
  const taskRows = job.tasks.map(task => `<li class="team-task-row" data-task-id="${escape(task.id)}"><span>${escape(task.title)}</span><small>${escape(name(task.agent_id))} · ${escape(taskStatusLabel(task,job))}</small></li>`).join('');
  const teamFeed = `<article class="team-message own" data-team-request="${escape(job.id)}"><span class="system-avatar">你</span><div><strong>本轮需求</strong><p>${escape(job.request)}</p></div></article>${teamMessages.map(message => `<article class="team-message ${message.kind === 'system' ? 'system' : message.kind === 'user' ? 'own' : ''}" data-team-message-id="${escape(message.id)}">${message.sender_agent ? avatar(message.sender_agent) : `<span class="system-avatar">${message.kind === 'user' ? '你' : '·'}</span>`}<div><strong>${message.kind === 'system' ? '同席' : message.kind === 'user' ? '你' : escape(name(message.sender_agent || ''))}</strong><p>${escape(message.content)}</p></div></article>`).join('')}`;
  const teamResult = job.summary ? `<div class="workflow-result"><strong>${finalReviewer ? `${escape(name(finalReviewer))} 验收结论` : '项目结果'}</strong><p>${escape(job.summary)}</p></div>` : '';
  const panels = `<section class="team-panel" data-project-panel="team"><div class="team-progress"><div><strong>任务进度</strong><span>${job.tasks.filter(task => task.status === 'completed').length}/${job.tasks.length}</span></div>${job.tasks.length ? `<ol class="team-task-list">${taskRows}</ol>` : `<p class="workflow-summary">${job.status === 'completed' ? '本轮为对话回复，没有文件任务。' : '正在整理本轮目标…'}</p>`}${job.plan?.summary ? `<details class="team-plan"><summary>本轮安排</summary><p>${escape(job.plan.summary)}</p></details>` : ''}${teamResult}${job.error && !duplicateWorkflowError ? `<p class="workflow-error">${escape(job.error)}</p>` : ''}</div><div class="team-feed" aria-live="polite">${teamFeed || `<p class="team-feed-empty">Agent 在团队会话中分享的消息会显示在这里。</p>`}</div></section>`;
  return `<article class="workflow-card" data-workflow-id="${escape(job.id)}"><header class="workflow-heading"><strong>项目协作</strong><span class="workflow-status ${escape(job.status)}">${escape(projectLabels[job.status] || job.status)}</span>${workflowBusy(job) ? `<button type="button" class="text-button" data-stop-project="${escape(job.id)}" ${job.status === 'cancelling' ? 'disabled' : ''}>停止全部</button>` : ''}${['failed', 'interrupted'].includes(job.status) && job.attempts.some(attempt => attempt.stage === 'plan' && attempt.status === 'completed') ? `<button type="button" class="text-button" data-continue-project="${escape(job.id)}" title="复用本次已生成的方案，不重新调用规划模型">继续协作</button>` : ''}${['failed', 'interrupted'].includes(job.status) && job.plan && job.attempts.some(attempt => attempt.stage === 'plan' && attempt.status === 'completed') ? `<button type="button" class="text-button" data-edit-project-roles="${escape(job.id)}">调整阶段设置</button>` : ''}</header><details class="project-accordion" data-project-accordion ${workflowBusy(job) ? 'open' : ''}><summary><span>团队协作空间</span><small>${agentChats.length} 个 Agent · ${job.tasks.filter(task => task.status === 'completed').length}/${job.tasks.length} 项完成</small></summary><div class="project-workspace">${panels}</div></details>${job.status === 'planning' && job.plan ? confirmBlock(job, escape, name, catalog, eligibleAgents) : ''}</article>`;
}

interface DialogHost { modal: HTMLDialogElement; escape: Escape; open: (title: string, note: string, body: string) => void; saved: (room: string) => Promise<void>; }
export async function bindingDialog(room: Conversation, current: Project | null, host: DialogHost) {
  const projects = await invoke<Project[]>('list_projects');
  const { escape, modal } = host;
  host.open('绑定项目', '为当前群聊选择唯一项目目录。若目录还没有 Git 仓库，同席会创建本地仓库和安全基线提交，不会推送。绑定后直接在群里交流与开发。', `<form id="project-form"><label class="field">已登记项目<select id="project-choice"><option value="">登记新项目</option>${projects.map(project => `<option value="${escape(project.id)}" ${project.id === current?.id ? 'selected' : ''}>${escape(project.name)}</option>`).join('')}</select></label><div id="new-project-fields"><label class="field">项目名称<input id="project-name" maxlength="80" placeholder="例如：个人工具箱"></label><label class="field">项目目录<input id="project-root" placeholder="D:\\Projects\\my-app" autocomplete="off"></label></div><p id="registered-project-info" class="model-setting-note"></p><label class="field summary-toggle"><input id="project-summary" type="checkbox"${current?.summary_enabled ? ' checked' : ''}>将此项目开发摘要共享给阿尔比恩</label><p class="model-setting-note">默认关闭。开启后只共享该项目的协作进展，从之后的消息开始生效；已经说过或已经提交的内容不会被撤回。</p><p class="form-error" role="alert"></p><div class="modal-footer">${current ? '<button type="button" id="unbind-project" class="secondary">解除绑定</button>' : '<span>同群历史只作背景，本次消息定义当前目标</span>'}<button class="primary" type="submit">绑定项目</button></div></form>`);
  const form = modal.querySelector<HTMLFormElement>('#project-form')!;
  const choice = form.querySelector<HTMLSelectElement>('#project-choice')!;
  const summary = form.querySelector<HTMLInputElement>('#project-summary')!;
  const update = () => {
    form.querySelector<HTMLElement>('#new-project-fields')!.hidden = !!choice.value;
    const selected = projects.find(project => project.id === choice.value) || (current && current.id === choice.value ? current : null);
    form.querySelector('#registered-project-info')!.textContent = selected ? selected.root : '';
    // 未选择已登记项目时视为新项目：共享默认关闭。
    summary.checked = !!selected?.summary_enabled;
  };
  choice.addEventListener('change', update); update();
  const submit = async (unbind = false) => {
    form.querySelectorAll<HTMLButtonElement>('button').forEach(button => { button.disabled = true; });
    try {
      let projectId = choice.value || null;
      if (!unbind && !projectId) {
        const value = (id: string) => form.querySelector<HTMLInputElement>(`#${id}`)!.value.trim();
        const project = await invoke<Project>('register_project', { name: value('project-name'), root: value('project-root') });
        projectId = project.id;
      }
      await invoke('bind_project', { conversationId: room.id, projectId: unbind ? null : projectId });
      // 解绑只解除会话绑定，不会关闭该项目已经开启的共享。
      if (!unbind && projectId) await invoke<Project>('set_project_summary', { projectId, enabled: summary.checked });
      modal.close(); await host.saved(room.id);
    } catch (error) { form.querySelector('.form-error')!.textContent = error instanceof Error ? error.message : String(error); }
    finally { form.querySelectorAll<HTMLButtonElement>('button').forEach(button => { button.disabled = false; }); }
  };
  form.addEventListener('submit', event => { event.preventDefault(); void submit(); });
  form.querySelector('#unbind-project')?.addEventListener('click', () => void submit(true));
}

interface PlanningHost {
  modal: HTMLDialogElement; escape: Escape; name: (id: string) => string;
  members: (role: 'plan' | 'implement' | 'review') => string[];
  catalog: (id: string) => ModelOption[];
  settings?: (id: string) => { model: string | null; effort: string | null };
  open: (title: string, note: string, body: string) => void;
  initial?: Roles | null; title?: string; note?: string; submitLabel?: string;
}
/// 启动项目协作前的角色配置：规划 / 执行 / 验收各选一位已连接成员，再选模型与强度（留空即自动选型）。
export function planningDialog(host: PlanningHost): Promise<Roles | null> {
  const { escape, modal } = host;
  const roles = ['plan', 'implement', 'review'] as const;
  const preferred = { plan: 'codex-win', implement: 'dsh-win', review: 'hermes-win' } as const;
  const pick = (role: typeof roles[number]): RoleChoice => {
    const list = host.members(role);
    const agent = list.includes(preferred[role]) ? preferred[role] : list[0] || '';
    const settings = host.settings?.(agent);
    return { agent, model: settings?.model || null, effort: settings?.effort || null };
  };
  const state: Roles = host.initial ? {
    plan: { ...host.initial.plan }, implement: { ...host.initial.implement }, review: { ...host.initial.review },
  } : { plan: pick('plan'), implement: pick('implement'), review: pick('review') };
  const selectedProviders: Record<typeof roles[number], string> = {
    plan: selectedProvider(host.catalog(state.plan.agent), state.plan.model),
    implement: selectedProvider(host.catalog(state.implement.agent), state.implement.model),
    review: selectedProvider(host.catalog(state.review.agent), state.review.model),
  };
  const effortOptionsHtml = (models: ModelOption[], model: string | null, selected: string | null) => `<option value="">自动</option>${effortOptions(models, model || '').map(value => `<option value="${escape(value)}" ${value === selected ? 'selected' : ''}>${escape(effortLabels[value] || value)}</option>`).join('')}`;
  host.open(host.title || '配置项目角色', host.note || '规划、执行、验收三个阶段均可选择 Codex、Hermes 或 DSH。执行阶段选择的成员会承担本方案的全部实现任务和验收失败后的修复；其模型与强度用于这些任务。模型和强度留空即由该成员自动选型。', `<form id="role-form">${roles.map(role => {
    const models = host.catalog(state[role].agent);
    const provider = selectedProviders[role] || selectedProvider(models, state[role].model);
    const providers = modelProviders(models);
    const options = host.members(role).map(id => `<option value="${escape(id)}" ${id === state[role].agent ? 'selected' : ''}>${escape(host.name(id))}</option>`).join('');
    return `<div class="role-field ${providers.length > 1 ? 'multi-provider' : ''}"><span>${roleNames[role]}</span><label>成员<select data-role-agent="${role}">${options}</select></label><label data-role-provider-wrap="${role}" ${providers.length > 1 ? '' : 'hidden'}>提供商<select data-role-provider="${role}">${providerOptionsHtml(models, provider, escape)}</select></label><label>模型<select data-role-model="${role}">${modelOptionsHtml(models, provider, state[role].model, escape, '自动选型')}</select></label><label>强度<select data-role-effort="${role}">${effortOptionsHtml(models, state[role].model, state[role].effort)}</select></label></div>`;
  }).join('')}<p class="model-setting-note">成员只列出当前会话里已连接、且能承担该角色的成员；“自动选型”表示由该成员按任务选择模型与强度。</p><p class="form-error" role="alert"></p><div class="modal-footer"><button type="button" id="role-cancel" class="secondary">取消</button><button class="primary" type="submit">${escape(host.submitLabel || '开始规划')}</button></div></form>`);
  const form = modal.querySelector<HTMLFormElement>('#role-form')!;
  let settled = false;
  let resolve!: (value: Roles | null) => void;
  const result = new Promise<Roles | null>(done => { resolve = done; });
  const finish = (value: Roles | null) => { if (!settled) { settled = true; resolve(value); } };
  // 取消、Esc、右上角关闭都走 close；只在尚未结算时按取消处理。
  modal.addEventListener('close', () => finish(null), { once: true });
  const syncEfforts = (role: typeof roles[number]) => {
    const models = host.catalog(state[role].agent);
    const efforts = effortOptions(models, state[role].model || '');
    const select = form.querySelector<HTMLSelectElement>(`[data-role-effort="${role}"]`)!;
    select.innerHTML = effortOptionsHtml(models, state[role].model, state[role].effort);
    select.value = efforts.includes(state[role].effort || '') ? state[role].effort! : '';
    state[role].effort = select.value || null;
  };
  const syncModels = (role: typeof roles[number]) => {
    const models = host.catalog(state[role].agent);
    const provider = selectedProviders[role] || selectedProvider(models, state[role].model);
    const providers = modelProviders(models);
    const providerSelect = form.querySelector<HTMLSelectElement>(`[data-role-provider="${role}"]`);
    const providerWrap = form.querySelector<HTMLElement>(`[data-role-provider-wrap="${role}"]`)!;
    providerWrap.hidden = providers.length < 2;
    form.querySelector<HTMLElement>(`[data-role-agent="${role}"]`)!.closest('.role-field')!.classList.toggle('multi-provider', providers.length > 1);
    if (providerSelect) providerSelect.innerHTML = providerOptionsHtml(models, provider, escape);
    const select = form.querySelector<HTMLSelectElement>(`[data-role-model="${role}"]`)!;
    select.innerHTML = modelOptionsHtml(models, provider, state[role].model, escape, '自动选型');
    select.value = state[role].model || '';
    syncEfforts(role);
  };
  if (roles.some(role => !host.members(role).length)) form.querySelector<HTMLButtonElement>('.primary')!.disabled = true;
  roles.forEach(role => {
    form.querySelector<HTMLSelectElement>(`[data-role-agent="${role}"]`)!.addEventListener('change', event => { state[role].agent = (event.target as HTMLSelectElement).value; const settings = host.settings?.(state[role].agent); state[role].model = settings?.model || null; state[role].effort = settings?.effort || null; selectedProviders[role] = selectedProvider(host.catalog(state[role].agent), state[role].model); syncModels(role); });
    form.querySelector<HTMLSelectElement>(`[data-role-provider="${role}"]`)!.addEventListener('change', event => { selectedProviders[role] = (event.target as HTMLSelectElement).value; state[role].model = null; state[role].effort = null; syncModels(role); });
    form.querySelector<HTMLSelectElement>(`[data-role-model="${role}"]`)!.addEventListener('change', event => { state[role].model = (event.target as HTMLSelectElement).value || null; state[role].effort = null; syncEfforts(role); });
    form.querySelector<HTMLSelectElement>(`[data-role-effort="${role}"]`)!.addEventListener('change', event => { state[role].effort = (event.target as HTMLSelectElement).value || null; });
  });
  form.addEventListener('submit', event => { event.preventDefault(); finish(state); modal.close(); });
  form.querySelector('#role-cancel')!.addEventListener('click', () => { finish(null); modal.close(); });
  return result;
}
