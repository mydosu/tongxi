import { invoke } from './transport';
import type { Conversation, Project, ProjectTeamMessage, RoleChoice } from './types';
import { defaultCoordinator, mountSessionPicker, PROJECT_AGENTS, type PickerHost } from './project_sessions';
import { attachmentHtml, attachmentText } from './attachments';
import { agentMarkdown } from './agent_markdown';

type Escape = (value: string | number) => string;
interface DialogHost extends PickerHost { globalName?: (id: string) => string; modal: HTMLDialogElement; open: (title: string, subtitle: string, body: string) => void; saved: (id: string) => Promise<void>; coordinator: RoleChoice | null; }
export function projectTeamMessages(room: string, saved: ProjectTeamMessage[]) {
  return saved.filter(message => message.conversation_id === room).sort((a, b) => a.created_at - b.created_at || a.id.localeCompare(b.id));
}
const clock = (at: number) => new Intl.DateTimeFormat('zh-CN', { hour: '2-digit', minute: '2-digit' }).format(at);
/// 交回的结果给个颜色：做完了、被停下了、失败。
const returnTone = (state: string) => state.startsWith('做完') ? 'done' : state.includes('停') ? 'stopped' : 'failed';
/// 同席自己发的派活、交回记录：不用气泡，串成一条接力线（主脑 → 成员，成员交回）。
function relayHtml(message: ProjectTeamMessage, escape: Escape) {
  const text = attachmentText(message.content).trim();
  const handoff = text.match(/^主脑 → (.+?)：([\s\S]*)$/);
  const back = handoff ? null : text.match(/^(.+?) 交回任务 (\S+)：(.+)$/);
  const head = handoff ? `<strong>主脑</strong><span class="relay-arrow" aria-label="分配给">→</span><strong>${escape(handoff[1])}</strong>`
    : back ? `<strong>${escape(back[1])}</strong><span>提交</span><span class="relay-tag ${returnTone(back[3])}">${escape(back[3])}</span><small>任务 ${escape(back[2])}</small>`
    : '<strong>同席</strong>';
  const body = handoff ? `<p class="relay-text">${escape(handoff[2].trim())}</p>` : back ? '' : `<div class="relay-text markdown">${agentMarkdown(text, escape)}</div>`;
  const kind = handoff ? 'handoff' : back ? `return ${returnTone(back[3])}` : 'note';
  return `<article class="project-team-message system relay-item ${kind}" data-team-message-id="${escape(message.id)}" data-timeline-at="${message.created_at}" data-timeline-order="2"><span class="relay-node" aria-hidden="true"></span><div class="relay-body"><div class="relay-line">${head}<time>${clock(message.created_at)}</time></div>${body}</div></article>`;
}
export function projectTeamMessageHtml(message: ProjectTeamMessage, escape: Escape, name: (id: string) => string, avatar: (id: string) => string, own = '<span class="system-avatar">你</span>') {
  if (message.kind === 'system' && !message.sender_agent) return relayHtml(message, escape);
  const author = message.kind === 'user' ? '你' : message.sender_agent ? name(message.sender_agent) : '同席';
  const portrait = message.sender_agent ? avatar(message.sender_agent) : message.kind === 'user' ? own : '<span class="system-avatar">·</span>';
  return `<article class="message project-team-message ${message.kind === 'user' ? 'own' : 'agent-message'} ${message.kind === 'system' ? 'system' : ''}" data-team-message-id="${escape(message.id)}" data-timeline-at="${message.created_at}" data-timeline-order="2">${portrait}<div class="project-team-message-body"><div class="message-author"><strong>${escape(author)}</strong><time>${clock(message.created_at)}</time></div>${message.kind === 'user' ? `<div class="bubble">${escape(attachmentText(message.content))}</div>` : `<div class="bubble markdown">${agentMarkdown(attachmentText(message.content), escape)}</div>`}${attachmentHtml(message.content,message.conversation_id,escape)}</div></article>`;
}
/// 后端存的是 Windows 规范化路径（以 `\\?\` 开头），给人看时去掉这个前缀。
const plainPath = (path: string) => path.startsWith('\\\\?\\') ? path.slice(4) : path;
export async function bindingDialog(room: Conversation, current: Project | null, host: DialogHost) {
  let projects = await invoke<Project[]>('list_projects');
  const { escape, modal } = host;
  const members = room.members.filter(id => PROJECT_AGENTS.includes(id));
  let coordinator = defaultCoordinator(members, host.coordinator?.agent);
  host.open('绑定项目', '为当前群聊选择唯一项目目录。若目录还没有 Git 仓库，同席会创建本地仓库和安全基线提交，不会推送。绑定后直接在群里交流与开发。', `<form id="project-form"><label class="field">已登记项目<select id="project-choice"><option value="">登记新项目</option>${projects.map(project => `<option value="${escape(project.id)}" ${project.id === current?.id ? 'selected' : ''}>${escape(project.name)}</option>`).join('')}</select></label><div id="new-project-fields"><label class="field">项目名称<input id="project-name" maxlength="80" placeholder="例如：个人工具箱"></label><label class="field">项目目录<input id="project-root" placeholder="D:\\Projects\\my-app" autocomplete="off"></label></div><div class="registered-project"><p id="registered-project-info" class="model-setting-note"></p><button type="button" class="text-button" id="release-project" hidden title="把这个项目从目录上解开、从列表里拿掉；归档群的聊天和协作历史保留">解除登记</button></div><fieldset id="session-bind-section" class="session-bind-section"><legend>各成员的会话</legend><label class="field">主脑由谁当<select id="coordinator-choice">${members.map(id => `<option value="${escape(id)}" ${id === coordinator ? 'selected' : ''}>${escape((host.globalName ?? host.name)(id))}</option>`).join('')}</select></label><div class="session-bind-list"></div><small class="model-setting-note">可以让成员（含主脑）续接它们自己已有的原生会话；保持「新建会话」则由成员新开。点「＋」给同一成员再开一个平行的会话（模型可同可不同），主脑会把它们当作不同成员派活。项目开始后也能在成员面板里切换。</small></fieldset><label class="field summary-toggle"><input id="project-summary" type="checkbox"${current?.summary_enabled ? ' checked' : ''}>将此项目开发摘要共享给${escape(host.name('albion-wsl'))}</label><p class="model-setting-note">默认关闭。开启后只共享主脑在项目群里发的进展摘要，从之后的消息开始生效；已经说过或已经提交的内容不会被撤回。</p><p class="form-error" role="alert"></p><div class="modal-footer">${current ? '<button type="button" id="unbind-project" class="secondary">解除绑定</button>' : '<span>同群历史只作背景，本次消息定义当前目标</span>'}<button class="primary" type="submit">绑定项目</button></div></form>`);
  const form = modal.querySelector<HTMLFormElement>('#project-form')!;
  const choice = form.querySelector<HTMLSelectElement>('#project-choice')!;
  const summary = form.querySelector<HTMLInputElement>('#project-summary')!;
  const section = form.querySelector<HTMLElement>('#session-bind-section')!;
  const rows = () => [...(coordinator ? [{ agent: `${coordinator}-brain`, target: 'member' as const, current: null, busy: false }] : []), ...members.map(agent => ({ agent, target: 'member' as const, current: null, busy: false }))];
  const picker = mountSessionPicker(section.querySelector('.session-bind-list')!, rows(), host, true);
  form.querySelector<HTMLSelectElement>('#coordinator-choice')!.addEventListener('change', event => { coordinator = (event.target as HTMLSelectElement).value; picker.render(rows()); });
  // 只在新建或换到另一个项目时挑会话；已绑定项目的会话走「会话绑定」切换。
  const choosing = () => !current || choice.value !== current.id;
  const openedSeats = new Map<string, string[]>();
  const update = () => {
    section.hidden = !choosing() || members.length === 0;
    form.querySelector<HTMLElement>('#new-project-fields')!.hidden = !!choice.value;
    const selected = projects.find(project => project.id === choice.value) || (current && current.id === choice.value ? current : null);
    form.querySelector('#registered-project-info')!.textContent = selected ? plainPath(selected.root) : '';
    const release = form.querySelector<HTMLButtonElement>('#release-project')!;
    release.hidden = !choice.value || choice.value === current?.id;
    release.dataset.armed = ''; release.textContent = '解除登记';
    // 未选择已登记项目时视为新项目：共享默认关闭。
    summary.checked = !!selected?.summary_enabled;
  };
  choice.addEventListener('change', update); update();
  // 解除登记：点两下确认。解开后把目录填进「登记新项目」，可以马上换个名字重新绑定。
  form.querySelector<HTMLButtonElement>('#release-project')!.addEventListener('click', async event => {
    const button = event.currentTarget as HTMLButtonElement;
    if (!button.dataset.armed) { button.dataset.armed = '1'; button.textContent = '再点一次确认解除'; return; }
    const released = projects.find(project => project.id === choice.value);
    button.disabled = true;
    try {
      projects = await invoke<Project[]>('release_project', { projectId: choice.value });
      choice.innerHTML = `<option value="">登记新项目</option>${projects.map(project => `<option value="${escape(project.id)}" ${project.id === current?.id ? 'selected' : ''}>${escape(project.name)}</option>`).join('')}`;
      choice.value = '';
      if (released) form.querySelector<HTMLInputElement>('#project-root')!.value = plainPath(released.root);
      form.querySelector('.form-error')!.textContent = '';
      update();
      form.querySelector<HTMLInputElement>('#project-name')!.focus();
    } catch (error) { form.querySelector('.form-error')!.textContent = error instanceof Error ? error.message : String(error); }
    finally { button.disabled = false; }
  });
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
      if (!unbind && !section.hidden) {
        try {
          const same = host.coordinator?.agent === coordinator;
          await invoke('set_project_coordinator', { conversationId: room.id, coordinator: { agent: coordinator, model: same ? host.coordinator?.model || null : null, effort: same ? host.coordinator?.effort || null : null } });
          // 先把新增的席位开出来，拿到席位 id 后和其他行一起绑定会话。
          // 重试时复用上一次已经开出来的席位，不会重复开。
          const bindings = [], used = new Map<string, number>();
          for (const choice of picker.choices(true)) {
            if (choice.target !== 'seat') { bindings.push(choice); continue; }
            const pool = openedSeats.get(choice.agentId) || [], index = used.get(choice.agentId) || 0;
            used.set(choice.agentId, index + 1);
            const seat = pool[index] ?? await invoke<string>('add_project_seat', { conversationId: room.id, agentId: choice.agentId, label: null });
            if (index >= pool.length) openedSeats.set(choice.agentId, [...pool, seat]);
            bindings.push({ ...choice, agentId: seat, target: 'member' as const });
          }
          await invoke('bind_project_sessions', { conversationId: room.id, bindings });
        } catch (error) {
          // 项目已经绑上：刷新背后的界面，弹窗留着改完再保存（重复绑定同一项目无副作用）。
          await host.saved(room.id).catch(() => {});
          throw new Error(`项目已绑定，但会话绑定未完成：${error instanceof Error ? error.message : String(error)}`);
        }
      }
      modal.close(); await host.saved(room.id);
    } catch (error) { form.querySelector('.form-error')!.textContent = error instanceof Error ? error.message : String(error); }
    finally { form.querySelectorAll<HTMLButtonElement>('button').forEach(button => { button.disabled = false; }); }
  };
  form.addEventListener('submit', event => { event.preventDefault(); void submit(); });
  form.querySelector('#unbind-project')?.addEventListener('click', () => void submit(true));
}
