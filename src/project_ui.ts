import { invoke } from '@tauri-apps/api/core';
import type { Conversation, ExecutionChoice, ModelOption, Project, ProjectAttempt, ProjectTask, RoleChoice, Roles, Workflow } from './types';

type Escape = (value: string | number) => string;
export const workflowBusy = (job: Workflow | null | undefined) => !!job && ['queued', 'planning', 'running', 'verifying', 'reviewing', 'cancelling'].includes(job.status);
export const projectLabels: Record<string, string> = { queued: '等待项目空闲', planning: '方案与任务分发', running: '正在实现', verifying: '正在实际验收', reviewing: 'Hermes 功能校验', cancelling: '正在停止', completed: '协作完成', failed: '协作失败', interrupted: '协作已中断', starting: '准备会话', skipped: '未执行' };
const stages: Record<string, string> = { implement: '项目实现', verify: '实际检查', review: '功能校验', repair: '复杂修复' };
const stageLabel = (attempt: ProjectAttempt) => attempt.stage === 'plan' ? attempt.agent_id === 'codex-win' ? '项目方案' : attempt.agent_id === 'hermes-win' ? '任务分发' : '项目方案' : attempt.stage === 'repair' && attempt.agent_id === 'codex-win' ? '疑难诊断' : attempt.stage === 'repair' && attempt.agent_id === 'dsh-win' ? '实际修复' : stages[attempt.stage] || attempt.stage;
const effortLabels: Record<string, string> = { none: '关闭思考', off: '关闭思考', minimal: '最少', low: '低', medium: '中', high: '高', xhigh: '很高', max: '最大', ultra: 'Ultra' };
const roleNames: Record<'plan' | 'implement' | 'review', string> = { plan: '规划', implement: '执行', review: '验收' };
const effortOptions = (models: ModelOption[], modelId: string) => models.find(item => item.id === modelId)?.efforts || Object.keys(effortLabels);

/// 逐任务确认区的一行：标题、执行者、授权文件，加模型与强度两个下拉（预填规划给出的选型建议）。
function confirmRow(task: ProjectTask, suggestion: ExecutionChoice | null | undefined, models: ModelOption[], escape: Escape, name: (id: string) => string) {
  const model = task.model || suggestion?.model || '';
  const effort = task.effort || suggestion?.reasoning_effort || '';
  const files = task.files.length ? ` · ${task.files.map(escape).join(' · ')}` : '';
  return `<div class="task-confirm-row" data-task-position="${task.position}" data-task-agent="${escape(task.agent_id)}"><div class="task-confirm-title"><strong>${escape(task.title)}</strong><span>${escape(name(task.agent_id))}${files}</span></div><label>模型<select data-task-model aria-label="任务模型"><option value="">自动选型</option>${models.map(item => `<option value="${escape(item.id)}" ${item.id === model ? 'selected' : ''}>${escape(item.name)}</option>`).join('')}</select></label><label>强度<select data-task-effort aria-label="任务思考强度"><option value="">自动</option>${effortOptions(models, model).map(value => `<option value="${escape(value)}" ${value === effort ? 'selected' : ''}>${escape(effortLabels[value] || value)}</option>`).join('')}</select></label></div>`;
}

function confirmBlock(job: Workflow, escape: Escape, name: (id: string) => string, catalog: (id: string) => ModelOption[]) {
  return `<div class="task-confirm"><div class="task-confirm-heading">规划完成 · 确认每个任务的模型与思考强度后开始执行</div>${job.tasks.map(task => confirmRow(task, job.plan?.tasks[task.position]?.execution, catalog(task.agent_id), escape, name)).join('')}<button type="button" class="primary" data-confirm-project>确认并开始执行</button></div>`;
}

export function workflowCard(job: Workflow, escape: Escape, name: (id: string) => string, model: (id: string, value: string | null) => string, catalog: (id: string) => ModelOption[]) {
  return `<article class="workflow-card" data-workflow-id="${escape(job.id)}"><div class="workflow-heading"><strong>项目协作</strong><span class="workflow-status ${escape(job.status)}">${escape(projectLabels[job.status] || job.status)}</span>${workflowBusy(job) ? `<button type="button" class="text-button" data-stop-project="${escape(job.id)}" ${job.status === 'cancelling' ? 'disabled' : ''}>停止协作</button>` : ''}</div>${job.plan ? `<p class="workflow-summary">${escape(job.plan.summary)}</p><ol class="task-list">${job.tasks.map(task => `<li data-task-id="${escape(task.id)}"><div><strong>${escape(task.title)}</strong><span>${escape(name(task.agent_id))} · ${escape(projectLabels[task.status] || task.status)}</span></div><small>${task.files.map(escape).join(' · ')}${task.depends_on.length ? ` · 依赖 ${task.depends_on.map(i => i + 1).join('、')}` : ''}</small><small>执行参数：${task.model ? escape(model(task.agent_id, task.model)) : '自动选型'}${task.effort ? ` · ${escape(task.effort)}` : ''}</small>${task.worktree || task.branch ? `<small>${task.worktree ? `工作树 ${escape(task.worktree)}` : ''}${task.worktree && task.branch ? ' · ' : ''}${task.branch ? `分支 ${escape(task.branch)}` : ''}</small>` : ''}${task.error ? `<p class="workflow-error">${escape(task.error)}</p>` : ''}</li>`).join('')}</ol>` : '<p class="workflow-summary">等待 Codex 拟定项目方案…</p>'}${job.status === 'planning' && job.plan ? confirmBlock(job, escape, name, catalog) : ''}
  <div class="attempt-list">${job.attempts.filter(attempt => attempt.stage !== 'verify' || attempt.checks.length > 0).map(attempt => `<details data-attempt-id="${escape(attempt.id)}" ${!['plan', 'review'].includes(attempt.stage) && ['starting', 'running', 'cancelling'].includes(attempt.status) ? 'open' : ''}><summary>${escape(name(attempt.agent_id))} · ${escape(stageLabel(attempt))}<span>${escape(projectLabels[attempt.status] || attempt.status)}</span></summary><small>${attempt.stage === 'verify' ? '本地检查程序 · 无模型调用' : `${escape(model(attempt.agent_id, attempt.model))}${attempt.reasoning_effort ? ` · ${escape(attempt.reasoning_effort)}` : ''}`}</small>${attempt.output ? `<pre class="attempt-output">${escape(attempt.output)}</pre>` : ''}${attempt.error ? `<p class="workflow-error">${escape(attempt.error)}</p>` : ''}${attempt.checks.map(check => `<div class="project-check ${check.exit_code === 0 && !check.timed_out ? 'passed' : 'failed'}"><strong>${escape(check.name)}</strong><span>${check.timed_out ? '超时' : check.exit_code === null ? '未完成' : `退出码 ${check.exit_code}`} · ${check.duration_ms} ms</span><code>${escape(check.program)} ${check.args.map(escape).join(' ')}</code>${check.output ? `<pre>${escape(check.output)}</pre>` : ''}</div>`).join('')}</details>`).join('')}</div>${job.changes.length ? `<details class="project-changes"><summary>实际文件记录 · ${new Set(job.changes.map(change => change.path)).size} 个文件</summary><ul>${job.changes.map(change => `<li><code>${escape(change.path)}</code><span>${change.operation === 'delete' ? '删除' : '写入'} · ${escape(change.after_hash?.slice(0, 12) || '无')}</span></li>`).join('')}</ul></details>` : ''}${job.summary ? `<p class="workflow-result">${escape(job.summary)}</p>` : ''}${job.error ? `<p class="workflow-error">${escape(job.error)}</p>` : ''}</article>`;
}

interface DialogHost { modal: HTMLDialogElement; escape: Escape; open: (title: string, note: string, body: string) => void; saved: (room: string) => Promise<void>; }
export async function bindingDialog(room: Conversation, current: Project | null, host: DialogHost) {
  const projects = await invoke<Project[]>('list_projects');
  const { escape, modal } = host;
  host.open('绑定项目', '选择项目目录。绑定后，点击“执行项目”提交编码需求。', `<form id="project-form"><label class="field">已登记项目<select id="project-choice"><option value="">登记新项目</option>${projects.map(project => `<option value="${escape(project.id)}" ${project.id === current?.id ? 'selected' : ''}>${escape(project.name)}</option>`).join('')}</select></label><div id="new-project-fields"><label class="field">项目名称<input id="project-name" maxlength="80" placeholder="例如：个人工具箱"></label><label class="field">项目目录<input id="project-root" placeholder="D:\\Projects\\my-app" autocomplete="off"></label></div><p id="registered-project-info" class="model-setting-note"></p><label class="field summary-toggle"><input id="project-summary" type="checkbox"${current?.summary_enabled ? ' checked' : ''}>将此项目开发摘要共享给阿尔比恩</label><p class="model-setting-note">默认关闭。开启后只共享该项目的协作进展，从之后的消息开始生效；已经说过或已经提交的内容不会被撤回。</p><p class="form-error" role="alert"></p><div class="modal-footer">${current ? '<button type="button" id="unbind-project" class="secondary">解除绑定</button>' : '<span>新会话不会导入其他会话的历史</span>'}<button class="primary" type="submit">绑定项目</button></div></form>`);
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
        const project = await invoke<Project>('register_project', { name: value('project-name'), root: value('project-root'), checks: [] });
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
  open: (title: string, note: string, body: string) => void;
}
/// 启动项目协作前的角色配置：规划 / 执行 / 验收各选一位已连接成员，再选模型与强度（留空即自动选型）。
export function planningDialog(host: PlanningHost): Promise<Roles | null> {
  const { escape, modal } = host;
  const roles = ['plan', 'implement', 'review'] as const;
  const preferred = { plan: 'codex-win', implement: 'dsh-win', review: 'hermes-win' } as const;
  const pick = (role: typeof roles[number]): RoleChoice => {
    const list = host.members(role);
    return { agent: list.includes(preferred[role]) ? preferred[role] : list[0] || '', model: null, effort: null };
  };
  const state: Roles = { plan: pick('plan'), implement: pick('implement'), review: pick('review') };
  const modelOptionsHtml = (models: ModelOption[], selected: string | null) => `<option value="">自动选型</option>${models.map(item => `<option value="${escape(item.id)}" ${item.id === selected ? 'selected' : ''}>${escape(item.name)}</option>`).join('')}`;
  const effortOptionsHtml = (models: ModelOption[], model: string | null, selected: string | null) => `<option value="">自动</option>${effortOptions(models, model || '').map(value => `<option value="${escape(value)}" ${value === selected ? 'selected' : ''}>${escape(effortLabels[value] || value)}</option>`).join('')}`;
  host.open('配置项目角色', '为规划、执行、验收各选一位成员与参数；模型与强度留空即由该成员自动选型。确认后先出方案，逐项确认任务后再开始执行。', `<form id="role-form">${roles.map(role => {
    const models = host.catalog(state[role].agent);
    const options = host.members(role).map(id => `<option value="${escape(id)}" ${id === state[role].agent ? 'selected' : ''}>${escape(host.name(id))}</option>`).join('');
    return `<div class="role-field"><span>${roleNames[role]}</span><label>成员<select data-role-agent="${role}">${options}</select></label><label>模型<select data-role-model="${role}">${modelOptionsHtml(models, state[role].model)}</select></label><label>强度<select data-role-effort="${role}">${effortOptionsHtml(models, state[role].model, state[role].effort)}</select></label></div>`;
  }).join('')}<p class="model-setting-note">成员只列出当前会话里已连接、且能承担该角色的成员；“自动选型”表示由该成员按任务选择模型与强度。</p><p class="form-error" role="alert"></p><div class="modal-footer"><button type="button" id="role-cancel" class="secondary">取消</button><button class="primary" type="submit">开始规划</button></div></form>`);
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
    const select = form.querySelector<HTMLSelectElement>(`[data-role-model="${role}"]`)!;
    select.innerHTML = modelOptionsHtml(host.catalog(state[role].agent), state[role].model);
    select.value = state[role].model || '';
    syncEfforts(role);
  };
  if (roles.some(role => !host.members(role).length)) form.querySelector<HTMLButtonElement>('.primary')!.disabled = true;
  roles.forEach(role => {
    form.querySelector<HTMLSelectElement>(`[data-role-agent="${role}"]`)!.addEventListener('change', event => { state[role].agent = (event.target as HTMLSelectElement).value; state[role].model = null; state[role].effort = null; syncModels(role); });
    form.querySelector<HTMLSelectElement>(`[data-role-model="${role}"]`)!.addEventListener('change', event => { state[role].model = (event.target as HTMLSelectElement).value || null; state[role].effort = null; syncEfforts(role); });
    form.querySelector<HTMLSelectElement>(`[data-role-effort="${role}"]`)!.addEventListener('change', event => { state[role].effort = (event.target as HTMLSelectElement).value || null; });
  });
  form.addEventListener('submit', event => { event.preventDefault(); finish(state); modal.close(); });
  form.querySelector('#role-cancel')!.addEventListener('click', () => { finish(null); modal.close(); });
  return result;
}
