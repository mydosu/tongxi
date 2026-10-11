import { invoke } from '@tauri-apps/api/core';
import type { Conversation, Project, ProjectTeamMessage, Workflow } from './types';
import { attachmentHtml, attachmentText } from './attachments';

type Escape = (value: string | number) => string;
interface DialogHost { escape: Escape; modal: HTMLDialogElement; open: (title: string, subtitle: string, body: string) => void; saved: (id: string) => Promise<void>; }
export function projectTeamMessages(room: string, saved: ProjectTeamMessage[], jobs: Workflow[]) {
  return [...new Map([...saved, ...jobs.flatMap(job => job.team_messages || [])].filter(message => message.conversation_id === room).map(message => [message.id, message])).values()].sort((a, b) => a.created_at - b.created_at || a.id.localeCompare(b.id));
}
export function projectTeamMessageHtml(message: ProjectTeamMessage, escape: Escape, name: (id: string) => string, avatar: (id: string) => string) {
  const author = message.kind === 'user' ? '你' : message.sender_agent ? name(message.sender_agent) : '同席';
  const portrait = message.sender_agent ? avatar(message.sender_agent) : `<span class="system-avatar">${message.kind === 'user' ? '你' : '·'}</span>`;
  return `<article class="message project-team-message ${message.kind === 'user' ? 'own' : 'agent-message'} ${message.kind === 'system' ? 'system' : ''}" data-team-message-id="${escape(message.id)}" data-timeline-at="${message.created_at}" data-timeline-order="2">${portrait}<div class="project-team-message-body"><div class="message-author"><strong>${escape(author)}</strong><time>${new Intl.DateTimeFormat('zh-CN', { hour: '2-digit', minute: '2-digit' }).format(message.created_at)}</time></div><div class="bubble">${escape(attachmentText(message.content))}</div>${attachmentHtml(message.content,message.conversation_id,escape)}</div></article>`;
}
export const workflowBusy = (job: Workflow | null | undefined) => !!job && ['queued', 'planning', 'running', 'verifying', 'reviewing', 'cancelling'].includes(job.status);
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
