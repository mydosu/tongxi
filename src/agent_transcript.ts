import { entryBusy, type AgentEntry } from './agent_timeline';
import { visibleQuestionReply } from './chat_questions';
import { agentMarkdown } from './agent_markdown';

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
        if (body.dataset.follow !== 'false') body.scrollTop = body.scrollHeight;
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
      this.patch(node, entry);
      if (node !== cursor) this.list.insertBefore(node, cursor);
      cursor = node.nextElementSibling;
    }
    if (!entries.length) this.list.innerHTML = this.format.empty;
    if (anchor && anchor.isConnected) this.viewport.scrollTop += anchor.getBoundingClientRect().top - offset;
    this.follow();
  }

  private create(entry: AgentEntry) {
    const node = document.createElement('article');
    node.dataset.entryId = entry.id;
    node.className = `agent-transcript-entry ${entry.kind === 'user' ? 'agent-user-entry' : entry.kind === 'peer' ? 'agent-peer-entry' : 'agent-output-entry'}`;
    node.innerHTML = '<header class="agent-turn-meta"><span data-entry-label></span><time></time><span data-entry-live></span><span data-entry-control></span></header><details class="thought agent-turn-thought"><summary></summary><div class="thought-body"></div></details><div class="agent-turn-tools"></div><div class="agent-turn-text"></div><details class="agent-protocol" hidden><summary></summary><pre></pre></details><p class="agent-turn-error" hidden></p><div class="agent-turn-actions"></div>';
    const thought = node.querySelector<HTMLDetailsElement>('.thought')!;
    thought.querySelector('summary')!.addEventListener('click', () => requestAnimationFrame(() => { thought.dataset.userOpen = String(thought.open); }));
    const body = thought.querySelector<HTMLElement>('.thought-body')!;
    body.addEventListener('scroll', () => { body.dataset.follow = String(body.scrollHeight - body.scrollTop - body.clientHeight < 28); }, { passive: true });
    return node;
  }

  private patch(node: HTMLElement, entry: AgentEntry) {
    const { escape, name } = this.format;
    const busy = entryBusy(entry.status);
    if (entry.attemptId) { node.dataset.attemptId = entry.attemptId; node.dataset.attemptStatus = entry.status; }
    if (entry.messageId) node.dataset.messageId = entry.messageId;
    if (entry.workflowId) node.dataset.sourceWorkflow = entry.workflowId;
    if (entry.kind === 'peer') node.dataset.agentDirectMessageId = entry.id.slice(5);
    const label = entry.kind === 'user' ? entry.source === 'project' ? '你 · 项目目标' : '你' : entry.kind === 'peer' ? `${name(entry.author)} · 成员直发` : name(entry.author);
    node.querySelector('[data-entry-label]')!.textContent = label;
    node.querySelector('time')!.textContent = new Intl.DateTimeFormat('zh-CN', { hour: '2-digit', minute: '2-digit' }).format(entry.at);
    node.querySelector<HTMLElement>('[data-entry-label]')!.title = [entry.model, entry.effort].filter(Boolean).join(' · ');
    node.querySelector('[data-entry-live]')!.textContent = busy ? entry.status === 'cancelling' ? '正在停止…' : '正在处理…' : entry.status === 'interrupted' ? '已中断' : '';
    const controls = node.querySelector<HTMLElement>('[data-entry-control]')!;
    const control = busy && entry.workflowId ? `<button type="button" class="text-button" data-stop-project-agent="${escape(entry.author)}" data-stop-project-workflow="${escape(entry.workflowId)}" ${entry.status === 'cancelling' ? 'disabled' : ''}>中断</button>` : '';
    if (controls.innerHTML !== control) controls.innerHTML = control;
    const thought = node.querySelector<HTMLDetailsElement>('.thought')!;
    const previousBusy = thought.dataset.active === 'true';
    const wasHidden = thought.hidden;
    thought.hidden = !entry.thought;
    if (entry.attemptId) thought.dataset.thoughtAttemptId = entry.attemptId;
    thought.dataset.active = String(busy);
    if (thought.dataset.userOpen === undefined && (wasHidden || !thought.dataset.initialized || previousBusy !== busy)) thought.open = busy;
    thought.dataset.initialized = 'true';
    thought.querySelector('summary')!.textContent = busy ? '思考中' : '已思考';
    const body = thought.querySelector<HTMLElement>('.thought-body')!;
    if (body.textContent !== entry.thought) {
      const follow = body.dataset.follow !== 'false';
      body.textContent = entry.thought;
      if (follow) body.scrollTop = body.scrollHeight;
    }
    const text = node.querySelector<HTMLElement>('.agent-turn-text')!;
    const output = (entry.kind==='user'?entry.body:visibleQuestionReply(entry.body,busy)) || (busy && !entry.thought && !entry.tools.length ? '正在等待原生输出…' : '');
    if (text.dataset.output !== output) {
      if (entry.kind === 'agent') text.innerHTML = agentMarkdown(output, escape); else text.textContent = output;
      text.dataset.output = output;
    }
    text.hidden = !output;
    const protocol = node.querySelector<HTMLDetailsElement>('.agent-protocol')!;
    protocol.hidden = !entry.protocol;
    protocol.querySelector('summary')!.textContent = entry.protocol?.label || '';
    if (protocol.querySelector('pre')!.textContent !== (entry.protocol?.raw || '')) protocol.querySelector('pre')!.textContent = entry.protocol?.raw || '';
    const error = node.querySelector<HTMLElement>('.agent-turn-error')!;
    error.textContent = entry.error || (entry.status === 'failed' ? '本次运行失败，已有内容已保留。' : '');
    error.hidden = !error.textContent;
    const tools = node.querySelector<HTMLElement>('.agent-turn-tools')!;
    const signature = JSON.stringify(entry.tools);
    if (tools.dataset.paint !== signature) {
      const disclosures = new Map([...tools.querySelectorAll<HTMLDetailsElement>('details')].map(detail => [detail.dataset.toolKey!, detail.open]));
      const last = entry.tools.at(-1);
      tools.innerHTML = last ? `<details class="agent-tools-summary" data-tool-key="all"><summary>${escape(entry.tools.some(tool => tool.status === 'running') ? '正在运行' : '已运行')} ${escape(last.title || last.kind)}${entry.tools.length > 1 ? ` · ${entry.tools.length} 项工具` : ''}</summary>${entry.tools.map(tool => `<details class="agent-tool-detail" data-tool-key="${escape(tool.event_id)}"><summary>${escape(tool.title || tool.kind)}<small>${escape(tool.status === 'running' ? '进行中' : tool.status === 'failed' ? '失败' : '完成')}</small></summary>${tool.input ? `<pre>${escape(tool.input)}</pre>` : ''}${tool.output ? `<pre>${escape(tool.output)}</pre>` : ''}</details>`).join('')}</details>` : '';
      tools.querySelectorAll<HTMLDetailsElement>('details').forEach(detail => { detail.open = disclosures.get(detail.dataset.toolKey!) || false; });
      tools.dataset.paint = signature;
    }
    const actions = node.querySelector<HTMLElement>('.agent-turn-actions')!;
    const action = entry.source === 'private' && entry.kind === 'agent' && entry.status === 'completed' && entry.messageId ? `<button type="button" class="agent-chat-share" data-project-agent-chat-share="${escape(entry.messageId)}">分享到团队</button>` : '';
    if (actions.innerHTML !== action) actions.innerHTML = action;
  }
}
