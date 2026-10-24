import { entryBusy, type AgentEntry } from './agent_timeline';
import { visibleQuestionReply } from './chat_questions';
import { agentMarkdown } from './agent_markdown';
import { toolActivity, toolLine, captureToolState, restoreToolState } from './tool_activity';
import { attachmentHtml, attachmentText } from './attachments';
import { plainThought, thoughtFlow, type FlowPart } from './thought_flow';
import { thoughtFollowing } from './thought_scroll';

/** 用户亲手展开/收起过的思考框（条目 id + 第几段）：节点被重建或刷新时照这个恢复，不会一点开就被重绘关掉。 */
const userOpened = new Map<string, boolean>();
const remember = (details: HTMLDetailsElement, key: () => string) =>
  details.querySelector('summary')!.addEventListener('click', () => userOpened.set(key(), !details.open));
// 思考框跟不跟到底由 thought_scroll 统一判断（按滚动方向，见那里的说明）。

interface TranscriptFormat {
  escape: (value: string | number) => string;
  name: (agent: string) => string;
  empty: string;
}

/** One keyed output list and one scroll owner for project turns and private turns. */
export class AgentTranscript {
  private list: HTMLElement;
  private following = true;
  private observer: ResizeObserver;
  private jump: HTMLButtonElement;
  private frame = 0;
  // 第一批是打开页面时已有的记录，不播入场；之后新建的条目才浮上来。
  private seeded = false;
  private scroll = () => {
    if (this.viewport.scrollHeight - this.viewport.scrollTop - this.viewport.clientHeight < 64) this.following = true;
    this.jump.hidden = this.following;
  };
  private pause = () => { this.following = false; this.jump.hidden = false; };
  private wheel = (event: WheelEvent) => { if (event.deltaY < 0) this.pause(); };
  private pointer = (event: PointerEvent) => { if (event.clientX >= this.viewport.getBoundingClientRect().right - 18) this.pause(); };
  private touch = () => this.pause();
  private key = (event: KeyboardEvent) => { if (['ArrowUp', 'PageUp', 'Home'].includes(event.key)) this.pause(); };

  constructor(readonly viewport: HTMLElement, private format: TranscriptFormat) {
    viewport.innerHTML = '<div class="agent-transcript-list"></div><button type="button" class="agent-jump-latest" hidden>回到最新 ↓</button>';
    this.list = viewport.querySelector('.agent-transcript-list')!;
    this.jump = viewport.querySelector('.agent-jump-latest')!;
    this.jump.addEventListener('click', () => this.latest());
    viewport.addEventListener('scroll', this.scroll, { passive: true });
    viewport.addEventListener('wheel', this.wheel, { passive: true });
    viewport.addEventListener('pointerdown', this.pointer);
    viewport.addEventListener('touchmove', this.touch, { passive: true });
    viewport.addEventListener('keydown', this.key);
    this.observer = new ResizeObserver(() => this.follow());
    this.observer.observe(this.list);
  }

  latest() { this.following = true; this.jump.hidden = true; this.follow(); }
  dispose() {
    this.observer.disconnect(); cancelAnimationFrame(this.frame);
    this.viewport.removeEventListener('scroll', this.scroll);
    this.viewport.removeEventListener('wheel', this.wheel);
    this.viewport.removeEventListener('pointerdown', this.pointer);
    this.viewport.removeEventListener('touchmove', this.touch);
    this.viewport.removeEventListener('keydown', this.key);
  }
  private follow() {
    cancelAnimationFrame(this.frame);
    this.frame = requestAnimationFrame(() => {
      for (const thought of this.list.querySelectorAll<HTMLDetailsElement>('.thought[data-active="true"][open]')) {
        const body = thought.querySelector<HTMLElement>('.thought-body')!;
        if (thoughtFollowing(body)) body.scrollTop = body.scrollHeight;
      }
      if (this.following && this.viewport.isConnected) this.viewport.scrollTop = this.viewport.scrollHeight;
    });
  }

  update(entries: AgentEntry[]) {
    const anchor = !this.following && [...this.list.children].find(node => node.getBoundingClientRect().bottom > this.viewport.getBoundingClientRect().top);
    const offset = anchor ? anchor.getBoundingClientRect().top : 0;
    const nodes = new Map([...this.list.querySelectorAll<HTMLElement>(':scope > [data-entry-id]')].map(node => [node.dataset.entryId!, node]));
    const keys = new Set(entries.map(entry => entry.id));
    for (const [key, node] of nodes) if (!keys.has(key)) node.remove();
    this.list.querySelector('.project-agent-chat-empty')?.remove();
    let cursor = this.list.firstElementChild;
    for (const entry of entries) {
      const node = nodes.get(entry.id) || this.create(entry);
      if (this.seeded && !nodes.has(entry.id)) node.classList.add('arrive');
      this.patch(node, entry);
      if (node !== cursor) this.list.insertBefore(node, cursor);
      cursor = node.nextElementSibling;
    }
    this.seeded = true;
    if (!entries.length) this.list.innerHTML = this.format.empty;
    if (anchor && anchor.isConnected) this.viewport.scrollTop += anchor.getBoundingClientRect().top - offset;
    this.follow();
  }

  private create(entry: AgentEntry) {
    const node = document.createElement('article');
    node.dataset.entryId = entry.id;
    node.className = `agent-transcript-entry ${entry.kind === 'user' ? 'agent-user-entry' : 'agent-output-entry'}`;
    node.innerHTML = '<header class="agent-turn-meta"><span data-entry-label></span><time></time><span data-entry-live></span><span data-entry-control></span></header><details class="thought agent-turn-thought"><summary></summary><div class="thought-body"></div></details><div class="agent-turn-tools"></div><div class="agent-turn-flow" hidden></div><div class="agent-turn-text"></div><details class="agent-protocol" hidden><summary></summary><pre></pre></details><p class="agent-turn-error" hidden></p><div class="agent-turn-actions"></div>';
    const thought = node.querySelector<HTMLDetailsElement>('.thought')!;
    remember(thought, () => `${node.dataset.entryId}:plain`);
    return node;
  }

  private patch(node: HTMLElement, entry: AgentEntry) {
    const { escape, name } = this.format;
    const busy = entryBusy(entry.status);
    if (entry.messageId) node.dataset.messageId = entry.messageId;
    const label = entry.author === 'user' ? '你' : name(entry.author);
    node.querySelector('[data-entry-label]')!.textContent = label;
    node.querySelector('time')!.textContent = new Intl.DateTimeFormat('zh-CN', { hour: '2-digit', minute: '2-digit' }).format(entry.at);
    node.querySelector<HTMLElement>('[data-entry-label]')!.title = [entry.model, entry.effort].filter(Boolean).join(' · ');
    node.querySelector('[data-entry-live]')!.textContent = busy ? entry.status === 'cancelling' ? '正在停止…' : '正在处理…' : entry.status === 'interrupted' ? '已中断' : '';
    // 思考与正文交替时按顺序穿插显示；带提问卡片的回复仍用“思考块 + 正文”。
    const flow = entry.kind === 'agent' && !entry.body.includes('```tongxi-question') ? thoughtFlow(entry.thought, entry.body)?.filter(part => part.kind === 'thought' || part.kind === 'text' || (part.kind === 'steer' && !!entry.steers?.[part.text]) || (part.kind === 'tool' && entry.tools.some(event => event.event_id === part.text))) ?? null : null;
    this.patchFlow(node.querySelector<HTMLElement>('.agent-turn-flow')!, flow, busy, entry);
    const plain = flow ? '' : plainThought(entry.thought);
    const thought = node.querySelector<HTMLDetailsElement>('.agent-turn-thought')!;
    const previousBusy = thought.dataset.active === 'true';
    const wasHidden = thought.hidden;
    thought.hidden = !plain;
    thought.dataset.active = String(busy);
    const chosen = userOpened.get(`${entry.id}:plain`);
    if (chosen !== undefined) { if (thought.open !== chosen) thought.open = chosen; }
    else if (wasHidden || !thought.dataset.initialized || previousBusy !== busy) thought.open = busy;
    thought.dataset.initialized = 'true';
    const summary = thought.querySelector('summary')!; const state = busy ? '思考中' : '已思考'; if (summary.textContent !== state) summary.textContent = state;
    const body = thought.querySelector<HTMLElement>('.thought-body')!;
    if (body.textContent !== plain) {
      const follow = thoughtFollowing(body);
      body.textContent = plain;
      if (follow) body.scrollTop = body.scrollHeight;
    }
    // 只找条目自己那一层：穿插显示的每段正文、插话也用 agent-turn-text，而且排在前面。
    const text = node.querySelector<HTMLElement>(':scope > .agent-turn-text')!;
    const output = flow?.length ? '' : attachmentText(entry.kind==='user'?entry.body:visibleQuestionReply(entry.body,busy)) || (busy && !entry.thought && !entry.tools.length ? '正在等待原生输出…' : '');
    if (text.dataset.output !== output) {
      if (entry.kind === 'agent') text.innerHTML = agentMarkdown(output, escape); else text.textContent = output;
      text.dataset.output = output;
    }
    text.hidden = !output;
    let attached=node.querySelector<HTMLElement>(':scope > .agent-turn-attachments');
    if(!attached){attached=document.createElement('div');attached.className='agent-turn-attachments';text.after(attached);}
    const attachment=attachmentHtml(entry.body,entry.attachmentRoom||'',escape);
    if(attached.innerHTML!==attachment)attached.innerHTML=attachment;
    const protocol = node.querySelector<HTMLDetailsElement>('.agent-protocol')!;
    protocol.hidden = !entry.protocol;
    protocol.querySelector('summary')!.textContent = entry.protocol?.label || '';
    if (protocol.querySelector('pre')!.textContent !== (entry.protocol?.raw || '')) protocol.querySelector('pre')!.textContent = entry.protocol?.raw || '';
    const error = node.querySelector<HTMLElement>('.agent-turn-error')!;
    error.textContent = entry.error || (entry.status === 'failed' ? '本次运行失败，已有内容已保留。' : '');
    error.hidden = !error.textContent;
    const tools = node.querySelector<HTMLElement>('.agent-turn-tools')!;
    // 带位置标记的调用已穿插在 flow 里，这里只汇总没有标记的（旧消息）。
    const placed = new Set(flow?.flatMap(part => part.kind === 'tool' ? [part.text] : []));
    const loose = entry.tools.filter(event => !placed.has(event.event_id));
    const signature = JSON.stringify(loose);
    if (tools.dataset.paint !== signature) {
      const disclosures = captureToolState(tools);
      tools.innerHTML = toolActivity(loose, escape);
      restoreToolState(tools, disclosures);
      tools.dataset.paint = signature;
    }
    const actions = node.querySelector<HTMLElement>('.agent-turn-actions')!;
    const action = entry.kind === 'agent' && entry.status === 'completed' && entry.messageId ? `<button type="button" class="agent-chat-share" data-project-agent-chat-share="${escape(entry.messageId)}">分享到团队</button>` : '';
    if (actions.innerHTML !== action) actions.innerHTML = action;
  }

  /** 按片段逐个打补丁：已存在的段只更新文字，展开状态和滚动位置不被重绘打断。 */
  private patchFlow(host: HTMLElement, flow: FlowPart[] | null, busy: boolean, { id: entryId, steers, tools }: AgentEntry) {
    host.hidden = !flow?.length;
    const parts = flow || [];
    while (host.children.length > parts.length) host.lastElementChild!.remove();
    parts.forEach((part, index) => {
      let node = host.children[index] as HTMLElement | undefined;
      if (node && node.dataset.kind !== part.kind) { while (host.children.length > index) host.lastElementChild!.remove(); node = undefined; }
      if (!node) {
        if (part.kind === 'thought') {
          const details = document.createElement('details');
          details.className = 'thought agent-turn-thought';
          details.innerHTML = '<summary></summary><div class="thought-body"></div>';
          remember(details, () => `${entryId}:${[...host.children].indexOf(details)}`);
          node = details;
        } else if (part.kind === 'steer') {
          node = document.createElement('div');
          node.className = 'agent-turn-steer agent-user-entry';
          node.innerHTML = '<header class="agent-turn-meta"><span>插话</span><time></time></header><div class="agent-turn-text"></div>';
        } else if (part.kind === 'tool') {
          node = document.createElement('div');
        } else {
          node = document.createElement('div');
          node.className = 'agent-turn-text';
        }
        node.dataset.kind = part.kind;
        host.append(node);
      }
      if (part.kind === 'thought') {
        const details = node as HTMLDetailsElement;
        const live = busy && index === parts.length - 1;
        const chosen = userOpened.get(`${entryId}:${index}`);
        if (chosen !== undefined) { if (details.open !== chosen) details.open = chosen; }
        else if (details.dataset.active !== String(live)) details.open = live;
        details.dataset.active = String(live);
        const summary = details.querySelector('summary')!; const label = live ? '思考中' : '已思考'; if (summary.textContent !== label) summary.textContent = label;
        const body = details.querySelector<HTMLElement>('.thought-body')!;
        if (body.textContent !== part.text) {
          const follow = thoughtFollowing(body);
          body.textContent = part.text;
          if (follow) body.scrollTop = body.scrollHeight;
        }
      } else if (part.kind === 'steer') {
        const { name } = this.format;
        const said = steers![part.text];
        node.querySelector('.agent-turn-meta > span')!.textContent = said.author === 'user' ? '插话' : name(said.author);
        node.querySelector('time')!.textContent = new Intl.DateTimeFormat('zh-CN', { hour: '2-digit', minute: '2-digit' }).format(said.at);
        node.querySelector('.agent-turn-text')!.textContent = attachmentText(said.body);
      } else if (part.kind === 'tool') {
        const html = toolLine(tools.find(event => event.event_id === part.text)!, this.format.escape);
        if (node.dataset.output !== html) { const open = captureToolState(node); node.innerHTML = html; restoreToolState(node, open); node.dataset.output = html; }
      } else {
        const output = attachmentText(part.text);
        if (node.dataset.output !== output) { node.innerHTML = agentMarkdown(output, this.format.escape); node.dataset.output = output; }
      }
    });
  }
}
