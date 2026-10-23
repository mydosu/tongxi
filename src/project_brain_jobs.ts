import type { BrainJob, BrainOverview, PlanDoc, PlanStep, UsageSum } from './types';
import { agentMarkdown } from './agent_markdown';
import dagre from '@dagrejs/dagre';

type Escape = (value: string | number) => string;

/// 「4 分钟」「2 小时 5 分」：一件活做了多久（没做完就算到现在）。
function span(from: number, to: number) {
  const minutes = Math.max(0, Math.round((to - from) / 60000));
  if (minutes < 1) return '不到 1 分钟';
  if (minutes < 60) return `${minutes} 分钟`;
  const hours = Math.floor(minutes / 60), rest = minutes % 60;
  return hours < 24 ? `${hours} 小时${rest ? ` ${rest} 分` : ''}` : `${Math.round(hours / 24)} 天`;
}

function when(at: number) {
  return new Date(at).toLocaleString('zh-CN', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}

/// 「8.6 万」：token 数按万显示。
const tokens = (count: number) => count < 10000 ? `${count}` : `${(count / 10000).toFixed(1)} 万`;

/// 一段用量拆成新写入、缓存写、缓存读、输出：缓存读几乎不花钱，新写入和缓存写才是大头。
function usageText(sum: UsageSum) {
  const fresh = Math.max(0, sum.input - sum.cache_read - sum.cache_write);
  return `新写入 ${tokens(fresh)} · 缓存写 ${tokens(sum.cache_write)} · 缓存读 ${tokens(sum.cache_read)} · 输出 ${tokens(sum.output)}`;
}

function usageSection(usage: BrainOverview['usage']) {
  const row = (label: string, sum: UsageSum) => `<p><strong>${label}</strong><span>${sum.turns ? `${sum.turns} 回合 · ${usageText(sum)}` : '还没有记录'}</span></p>`;
  return `<section class="brain-usage">${row('主脑今天', usage.brain_today)}${row('主脑累计', usage.brain_total)}${usage.context ? `<p><strong>主脑上下文</strong><span>${tokens(usage.context)}（最近一回合结束时）</span></p>` : ''}</section>`;
}

export const jobLive = (job: BrainJob) => job.status === 'pending' || job.status === 'running';

function state(job: BrainJob): [string, string] {
  if (job.status === 'pending') return ['排队', 'pending'];
  if (job.status === 'running') return ['在做', 'running'];
  if (job.status === 'done') return ['做完了', 'done'];
  if (job.run_status === 'interrupted' || job.error === '你在派活页取消了') return ['被停下了', 'stopped'];
  return ['失败', 'failed'];
}

/// DSH 的模型存成 JSON 数组（线路 + 模型），拆开显示。
export function modelText(model: string | null) {
  if (!model) return '会话默认模型';
  try { const parts = JSON.parse(model); if (Array.isArray(parts)) return parts.join(' / '); } catch { /* 普通模型名 */ }
  return model;
}

function card(job: BrainJob, escape: Escape, avatar: (seat: string) => string, open: Set<string>) {
  const [label, tone] = state(job);
  const title = job.content.trim().split('\n')[0].slice(0, 120);
  const long = job.content.trim() !== title;
  const timing = jobLive(job) ? `${when(job.created_at)} 派出 · 已 ${span(job.created_at, Date.now())}` : `${when(job.created_at)} 派出 · 用时 ${span(job.created_at, job.finished_at || job.created_at)}`;
  const tree = job.branch ? `<span title="${escape(job.worktree || '')}">工作树 ${escape(job.branch)}${job.worktree ? '' : '（已合并或丢弃）'}</span>` : '<span>在项目目录里做</span>';
  // 任务原文照原样显示；交回的结果是成员的回复，按 Markdown 排版。
  const fold = (key: string, summary: string, body: string) => `<details data-job-fold="${escape(`${job.id}|${key}`)}" ${open.has(`${job.id}|${key}`) ? 'open' : ''}><summary>${summary}</summary><div class="brain-job-text${key === 'result' ? ' markdown' : ''}">${key === 'result' ? agentMarkdown(body, escape) : escape(body)}</div></details>`;
  return `<article class="brain-job ${tone}" data-job-id="${escape(job.id)}">
    <header>${avatar(job.seat)}<strong>${escape(job.name)}</strong><span class="brain-job-state ${tone}">${label}</span><small>${escape(timing)}</small></header>
    <p class="brain-job-title">${escape(title)}</p>
    ${long ? fold('task', '任务全文', job.content.trim()) : ''}
    <p class="brain-job-meta"><span>${escape(modelText(job.model))}${job.effort ? ` · ${escape(job.effort)}` : ''}</span>${tree}<span>任务 ${escape(job.id.slice(0, 8))}</span></p>
    ${job.error && job.error !== '你在派活页取消了' ? `<p class="brain-job-error">${escape(job.error)}</p>` : ''}
    ${job.result ? fold('result', '交回的结果', job.result) : ''}
    <div class="brain-job-actions">${jobLive(job) ? `<button type="button" class="secondary" data-stop-job="${escape(job.id)}">${job.status === 'pending' ? '取消' : '停止'}</button>` : ''}<button type="button" class="text-button" data-open-seat="${escape(job.seat)}">打开 ${escape(job.name)} 的会话</button></div>
  </article>`;
}

/// 任务书里的勾选框：数出做了几步，显示时换成看得懂的符号。
const STEP = /^(\s*(?:[-*+]|\d+\.)\s+)\[([ xX])\]/;
function planDoc(doc: PlanDoc, escape: Escape, open: Set<string>) {
  const lines = doc.text.split('\n');
  const steps = lines.map(line => line.match(STEP)?.[2]).filter(Boolean);
  const done = steps.filter(mark => mark !== ' ').length;
  const progress = steps.length ? `<span class="brain-plan-progress${done === steps.length ? ' done' : ''}">${done}/${steps.length}</span>` : '';
  const text = lines.map(line => line.replace(STEP, (_, head, mark) => `${head}${mark === ' ' ? '☐' : '☑'}`)).join('\n');
  const key = `plan|${doc.brief ? 'brief' : 'plan'}|${doc.name}`;
  return `<details class="brain-plan" data-job-fold="${escape(key)}" ${open.has(key) ? 'open' : ''}><summary><strong>${doc.brief ? `任务书 · ${escape(doc.name)}` : '长期计划'}</strong>${progress}<small>${escape(when(doc.modified))} 更新</small></summary><div class="brain-job-text markdown">${agentMarkdown(text, escape)}</div></details>`;
}

function plansSection(plans: PlanDoc[], escape: Escape, open: Set<string>) {
  const body = plans.length ? plans.map(doc => planDoc(doc, escape, open)).join('')
    : '<p class="inquiry-empty">主脑还没写计划。长或难的活它会先在 .tongxi/任务书/ 写任务书、边做边勾，要长期记住的写进 .tongxi/计划.md，都会显示在这里。</p>';
  return `<section class="brain-plans"><h2>计划与任务书</h2>${body}</section>`;
}

/// 计划流程图：列按先后（前面还连着几步），卡片颜色是状态，箭头是依赖。
const NODE = { w: 170, h: 84, gx: 40, gy: 14 };
const STEP_LOOK: Record<string, [string, string]> = { doing: ['●', '在做'], review: ['◐', '待审'], you: ['⏸', '等你'], waiting: ['⏸', '在等'], todo: ['○', '没做'], done: ['✓', '做完'] };
/// 等用户答问题、或负责人就是用户（手测这类）的等待，都算「等你」。
const stepKind = (step: PlanStep) => step.status === 'waiting' && (step.wait_key || step.owner === '你') ? 'you' : step.status;

function planFlow(all: PlanStep[], jobs: BrainJob[], escape: Escape, filter: string, pick: number | null) {
  if (!all.length) return '';
  // 做完又不卡任何没做完步骤的收起来，点「做完」再展开；全做完了就都显示。
  const feeds = (step: PlanStep) => all.some(next => next.depends.includes(step.id) && next.status !== 'done');
  const folded = filter === 'done' || all.every(step => step.status === 'done') ? [] : all.filter(step => step.status === 'done' && !feeds(step));
  const steps = all.filter(step => !folded.includes(step));
  const byId = new Map(steps.map(step => [step.id, step]));
  // dagre 分层排版：跨列的长边经虚拟节点绕开卡片，逐层扫描减少交叉。
  const graph = new dagre.graphlib.Graph();
  graph.setGraph({ rankdir: 'LR', nodesep: NODE.gy, ranksep: NODE.gx, edgesep: 8 });
  graph.setDefaultEdgeLabel(() => ({}));
  for (const step of steps) graph.setNode(String(step.id), { width: NODE.w, height: NODE.h });
  for (const step of steps) for (const id of step.depends) if (byId.has(id)) graph.setEdge(String(id), String(step.id));
  dagre.layout(graph);
  const at = new Map(steps.map(step => { const node = graph.node(String(step.id)); return [step.id, [node.x - NODE.w / 2, node.y - NODE.h / 2]] as const; }));
  // 「拖住 N 件」：后面还没做完的步骤，全都要等它。
  const blocked = (id: number, seen = new Set<number>()): Set<number> => {
    for (const next of steps) if (next.depends.includes(id) && next.status !== 'done' && !seen.has(next.id)) { seen.add(next.id); blocked(next.id, seen); }
    return seen;
  };
  const dim = (step: PlanStep) => filter && stepKind(step) !== filter ? ' dim' : '';
  const width = graph.graph().width!, height = graph.graph().height!;
  // 连线从上游卡片右边中点出、进下游卡片左边中点，中间照 dagre 给的折点（列间空当）走，每段用水平切线的三次曲线连起来。
  const edges = steps.flatMap(step => step.depends.filter(id => at.has(id)).map(id => {
    const [x0, y0] = at.get(id)!, [x1, y1] = at.get(step.id)!;
    const points = [{ x: x0 + NODE.w, y: y0 + NODE.h / 2 }, ...graph.edge(String(id), String(step.id)).points.slice(1, -1), { x: x1 - 1, y: y1 + NODE.h / 2 }];
    const d = points.map((q, i) => {
      if (!i) return `M${q.x} ${q.y}`;
      const p = points[i - 1], half = (q.x - p.x) / 2;
      return `C${p.x + half} ${p.y} ${q.x - half} ${q.y} ${q.x} ${q.y}`;
    }).join(' ');
    // 通向在做那一步的线流动起来：活正往那儿走。
    return `<path class="${byId.get(id)!.status === 'done' ? 'done' : ''}${stepKind(step) === 'doing' ? ' flow' : ''}${dim(step) && dim(byId.get(id)!)}" data-from="${id}" data-to="${step.id}" d="${d}" marker-end="url(#plan-arrow)"/>`;
  })).join('');
  const nodes = steps.map(step => {
    const [x, y] = at.get(step.id)!, kind = stepKind(step), [icon, word] = STEP_LOOK[kind];
    const job = jobs.find(item => item.id === step.job);
    const who = step.name || step.owner || '';
    const line = kind === 'doing' ? `${who}${job ? ` · ${span(job.created_at, Date.now())}` : ''}`
      : kind === 'review' ? `${who} 交回了，主脑在审`
      : kind === 'you' ? (step.wait_key ? `等你在问答面板回答${step.note ? `：${step.note}` : ''}` : `等你：${step.note || '—'}`)
      : kind === 'waiting' ? `等：${step.note || '—'}`
      : step.note || who;
    const held = step.status === 'done' ? 0 : blocked(step.id).size;
    return `<button type="button" class="plan-node ${kind}${dim(step)}${pick === step.id ? ' picked' : ''}" data-plan-pick="${step.id}" aria-pressed="${pick === step.id}" style="left:${x}px;top:${y}px;width:${NODE.w}px;height:${NODE.h}px" aria-label="${escape(`#${step.id} ${step.title}，${word}`)}">
      <strong><span class="plan-icon" aria-hidden="true">${icon}</span>${escape(step.title)}</strong>
      <span class="plan-foot"><small>${escape(line)}</small>${held ? `<em>拖住 ${held} 件</em>` : ''}</span></button>`;
  }).join('');
  const picked = all.find(step => step.id === pick);
  const counts = Object.keys(STEP_LOOK).map(kind => [kind, all.filter(step => stepKind(step) === kind).length] as const).filter(([, count]) => count);
  const bar = counts.map(([kind, count]) => `<button type="button" class="plan-filter ${kind}${filter === kind ? ' on' : ''}" data-plan-filter="${kind}">${STEP_LOOK[kind][1]} ${count}</button>`).join('');
  const hint = folded.length ? `<small class="plan-folded">${folded.length} 步做完、不卡别的，已收起，点「做完」展开</small>` : '';
  return `<section class="brain-plans plan-flow-section"><h2>计划</h2><div class="plan-filters">${bar}${filter ? '<button type="button" class="text-button" data-plan-filter="">看全部</button>' : ''}${hint}</div>
    <div class="plan-flow"><div class="plan-canvas" style="width:${width}px;height:${height}px">
      <svg width="${width}" height="${height}"><defs><marker id="plan-arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="8" markerHeight="8" orient="auto"><path d="M1.5 1.5 8 5 1.5 8.5"/></marker></defs>${edges}</svg>${nodes}</div></div>${picked ? stepDetail(picked, all, jobs, escape, picked.status === 'done' ? 0 : blocked(picked.id).size) : ''}</section>`;
}

/// 点开的一步：全文、谁负责、先要等哪几步、卡住几件；有派出去的活就能跳过去看、打开那位成员的会话。
function stepDetail(step: PlanStep, all: PlanStep[], jobs: BrainJob[], escape: Escape, held: number) {
  const kind = stepKind(step), [icon, word] = STEP_LOOK[kind];
  const job = jobs.find(item => item.id === step.job);
  const title = (id: number) => all.find(item => item.id === id)?.title || '';
  const rows = [
    step.name || step.owner ? `<p><strong>负责</strong><span>${escape(step.name || step.owner!)}</span></p>` : '',
    step.depends.length ? `<p><strong>先做完</strong><span>${step.depends.map(id => `<button type="button" class="plan-ref" data-plan-pick="${id}">#${id} ${escape(title(id))}</button>`).join('')}</span></p>` : '',
    held ? `<p><strong>卡住</strong><span>后面 ${held} 件没做完的都在等它</span></p>` : '',
  ].join('');
  const actions = job ? `<div class="brain-job-actions"><button type="button" class="secondary" data-show-job="${escape(job.id)}">看这件活</button><button type="button" class="text-button" data-open-seat="${escape(job.seat)}">打开 ${escape(job.name)} 的会话</button></div>` : '';
  return `<article class="plan-detail ${kind}"><header><span class="plan-chip"><span aria-hidden="true">${icon}</span>${word}</span><strong>#${step.id} ${escape(step.title)}</strong><button type="button" class="icon-button" data-plan-pick="${step.id}" aria-label="收起">×</button></header>
    ${step.note ? `<p class="plan-detail-note">${escape(step.note)}</p>` : ''}${rows ? `<div class="plan-detail-rows">${rows}</div>` : ''}${actions}</article>`;
}

/// 指到一步就亮出它前面要等的、后面被它卡住的整条链，其余淡下去；空白处按住能拖着看。
export function bindPlanFlow(root: ParentNode) {
  const flow = root.querySelector<HTMLElement>('.plan-flow'), canvas = flow?.querySelector<HTMLElement>('.plan-canvas');
  if (!flow || !canvas) return;
  const edges = [...canvas.querySelectorAll<SVGPathElement>('path[data-from]')];
  const chain = (start: string) => {
    const nodes = new Set([start]), paths = new Set<SVGPathElement>();
    for (const up of [true, false]) for (const queue = [start]; queue.length;) {
      const id = queue.pop()!;
      for (const edge of edges) if ((up ? edge.dataset.to : edge.dataset.from) === id) {
        const next = up ? edge.dataset.from! : edge.dataset.to!;
        paths.add(edge);
        if (!nodes.has(next)) { nodes.add(next); queue.push(next); }
      }
    }
    return { nodes, paths };
  };
  let traced = '';
  const trace = (node: HTMLElement | null) => {
    const id = node?.dataset.planPick || '';
    if (id === traced) return;
    traced = id;
    const lit = id ? chain(id) : null;
    canvas.classList.toggle('tracing', !!lit);
    canvas.querySelectorAll<HTMLElement>('.plan-node').forEach(item => item.classList.toggle('lit', !!lit?.nodes.has(item.dataset.planPick!)));
    for (const edge of edges) edge.classList.toggle('lit', !!lit?.paths.has(edge));
  };
  canvas.addEventListener('pointerover', event => trace((event.target as Element).closest<HTMLElement>('.plan-node')));
  canvas.addEventListener('pointerleave', () => trace(null));
  canvas.addEventListener('focusin', event => trace((event.target as Element).closest<HTMLElement>('.plan-node')));
  canvas.addEventListener('focusout', () => trace(null));
  flow.addEventListener('pointerdown', event => {
    if (event.button !== 0 || (event.target as Element).closest('.plan-node')) return;
    const from = { x: event.clientX, y: event.clientY, left: flow.scrollLeft, top: flow.scrollTop };
    flow.setPointerCapture(event.pointerId);
    flow.classList.add('panning');
    const move = (next: PointerEvent) => { flow.scrollLeft = from.left - (next.clientX - from.x); flow.scrollTop = from.top - (next.clientY - from.y); };
    const stop = () => { flow.classList.remove('panning'); flow.removeEventListener('pointermove', move); };
    flow.addEventListener('pointermove', move);
    flow.addEventListener('pointerup', stop, { once: true });
    flow.addEventListener('pointercancel', stop, { once: true });
  });
}

/// 主脑派活：主脑现在在干嘛、派出去的每件活。在做的在上面，做完的收在下面。
export function brainJobsPage(project: string, overview: BrainOverview | null, brainBusy: boolean, escape: Escape, avatar: (seat: string) => string, open: Set<string>, planFilter = '', planPick: number | null = null) {
  const head = `<header class="chat-header"><div><h1>主脑派活</h1><p>${escape(project)} · 主脑的计划、交给成员的每件活</p></div><div class="header-actions"><button class="secondary" data-project-return-team>团队对话</button>${overview?.chat ? `<button class="secondary" data-open-seat="${escape(overview.seat)}">主脑会话</button>` : ''}</div></header>`;
  if (!overview) return `${head}<div class="project-progress-content brain-jobs-content"><p class="inquiry-empty">正在读取…</p></div>`;
  const live = overview.jobs.filter(jobLive), ended = overview.jobs.filter(job => !jobLive(job));
  const status = `<section class="brain-status"><span class="brain-status-dot ${brainBusy ? 'busy' : ''}"></span><strong>主脑${brainBusy ? '正在回复' : '空闲'}</strong><small>${live.length ? `${live.length} 件活在外面` : '没有在外面的活'}${overview.waiting ? ` · ${overview.waiting} 条结果或回答等着送进主脑` : ''}</small></section>`;
  const body = live.length
    ? live.map(job => card(job, escape, avatar, open)).join('')
    : `<p class="inquiry-empty">${ended.length ? '现在没有在做的活。' : '主脑还没派过活。在团队对话里说要做的事，主脑会把活派给成员，派出去的都列在这里。'}</p>`;
  const history = ended.length ? `<details class="brain-jobs-ended" data-job-fold="ended" ${open.has('ended') ? 'open' : ''}><summary>已结束（${ended.length}）</summary>${ended.map(job => card(job, escape, avatar, open)).join('')}</details>` : '';
  return `${head}<div class="project-progress-content brain-jobs-content">${status}${planFlow(overview.steps, overview.jobs, escape, planFilter, planPick)}${usageSection(overview.usage)}${plansSection(overview.plans, escape, open)}${body}${history}</div>`;
}
