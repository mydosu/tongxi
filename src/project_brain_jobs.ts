import type { BrainJob, BrainOverview, PlanDoc, PlanStep, UsageSum } from './types';
import { agentMarkdown } from './agent_markdown';
import dagre from '@dagrejs/dagre';
import { still } from './motion';

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
  const row = (label: string, sum: UsageSum) => `<p><strong>${label}</strong><span>${sum.turns ? `${sum.turns} 回合 · ${usageText(sum)}` : '暂无记录'}</span></p>`;
  return `<section class="brain-usage">${row('主脑今日', usage.brain_today)}${row('主脑累计', usage.brain_total)}${usage.context ? `<p><strong>主脑上下文</strong><span>${tokens(usage.context)}（截至最近一回合结束）</span></p>` : ''}</section>`;
}

export const jobLive = (job: BrainJob) => job.status === 'pending' || job.status === 'running';

function state(job: BrainJob): [string, string] {
  if (job.status === 'pending') return ['排队中', 'pending'];
  if (job.status === 'running') return ['进行中', 'running'];
  if (job.status === 'done') return ['已完成', 'done'];
  if (job.run_status === 'interrupted' || job.error === '你在派活页取消了') return ['已停止', 'stopped'];
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
  const timing = jobLive(job) ? `${when(job.created_at)} 派发 · 已进行 ${span(job.created_at, Date.now())}` : `${when(job.created_at)} 派发 · 用时 ${span(job.created_at, job.finished_at || job.created_at)}`;
  const tree = job.branch ? `<span title="${escape(job.worktree || '')}">工作树 ${escape(job.branch)}${job.worktree ? '' : '（已合并或丢弃）'}</span>` : '<span>在项目目录中执行</span>';
  // 任务原文照原样显示；交回的结果是成员的回复，按 Markdown 排版。
  const fold = (key: string, summary: string, body: string) => `<details data-job-fold="${escape(`${job.id}|${key}`)}" ${open.has(`${job.id}|${key}`) ? 'open' : ''}><summary>${summary}</summary><div class="brain-job-text${key === 'result' ? ' markdown' : ''}">${key === 'result' ? agentMarkdown(body, escape) : escape(body)}</div></details>`;
  return `<article class="brain-job ${tone}" data-job-id="${escape(job.id)}">
    <header>${avatar(job.seat)}<strong>${escape(job.name)}</strong><span class="brain-job-state ${tone}">${label}</span><small>${escape(timing)}</small></header>
    <p class="brain-job-title">${escape(title)}</p>
    ${long ? fold('task', '任务全文', job.content.trim()) : ''}
    <p class="brain-job-meta"><span>${escape(modelText(job.model))}${job.effort ? ` · ${escape(job.effort)}` : ''}</span>${tree}<span>任务 ${escape(job.id.slice(0, 8))}</span></p>
    ${job.error && job.error !== '你在派活页取消了' ? `<p class="brain-job-error">${escape(job.error)}</p>` : ''}
    ${job.result ? fold('result', '提交的结果', job.result) : ''}
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
    : '<p class="inquiry-empty">主脑尚未制定计划。较长或较复杂的任务，主脑会先在 .tongxi/任务书/ 中编写任务书并随进度勾选，需要长期保留的内容写入 .tongxi/计划.md，均显示在此处。</p>';
  return `<section class="brain-plans"><h2>计划与任务书</h2>${body}</section>`;
}

/// 计划流程图：列按先后（前面还连着几步），卡片颜色是状态，箭头是依赖。
const NODE = { w: 170, h: 84, gx: 40, gy: 14 };
const STEP_LOOK: Record<string, [string, string]> = { doing: ['●', '进行中'], review: ['◐', '待审核'], you: ['⏸', '待你处理'], waiting: ['⏸', '等待中'], todo: ['○', '未开始'], done: ['✓', '已完成'] };
/// 等用户答问题、或负责人就是用户（手测这类）的等待，都算「等你」。
const stepKind = (step: PlanStep) => step.status === 'waiting' && (step.wait_key || step.owner === '你') ? 'you' : step.status;

function planFlow(all: PlanStep[], jobs: BrainJob[], escape: Escape, filter: string, pick: number | null) {
  if (!all.length) return '';
  // 不选分类时，做完又不卡任何没做完步骤的收起来；点「全部」或「做完」都摆出来；全做完了就都显示。
  const feeds = (step: PlanStep) => all.some(next => next.depends.includes(step.id) && next.status !== 'done');
  const folded = filter === 'all' || filter === 'done' || all.every(step => step.status === 'done') ? [] : all.filter(step => step.status === 'done' && !feeds(step));
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
  const dim = (step: PlanStep) => filter && filter !== 'all' && stepKind(step) !== filter ? ' dim' : '';
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
      : kind === 'review' ? `${who} 已提交，主脑审核中`
      : kind === 'you' ? (step.wait_key ? `待你在协作问答中回答${step.note ? `：${step.note}` : ''}` : `待你处理：${step.note || '—'}`)
      : kind === 'waiting' ? `等待：${step.note || '—'}`
      : step.note || who;
    const held = step.status === 'done' ? 0 : blocked(step.id).size;
    return `<button type="button" class="plan-node ${kind}${dim(step)}${pick === step.id ? ' picked' : ''}" data-plan-pick="${step.id}" aria-pressed="${pick === step.id}" style="left:${x}px;top:${y}px;width:${NODE.w}px;height:${NODE.h}px" aria-label="${escape(`#${step.id} ${step.title}，${word}`)}">
      <strong><span class="plan-icon" aria-hidden="true">${icon}</span><span class="plan-name">${escape(step.title)}</span></strong>
      <span class="plan-foot"><small>${escape(line)}</small>${held ? `<em>阻塞 ${held} 项</em>` : ''}</span></button>`;
  }).join('');
  const picked = all.find(step => step.id === pick);
  const counts = Object.keys(STEP_LOOK).map(kind => [kind, all.filter(step => stepKind(step) === kind).length] as const).filter(([, count]) => count);
  // 「全部」排第一个，每一步都摆出来；其余按状态只亮那一类。再点一次亮着的回到默认（收起做完又不卡别的）。
  const bar = [['all', all.length] as const, ...counts].map(([kind, count]) => `<button type="button" class="plan-filter ${kind}${filter === kind ? ' on' : ''}" data-plan-filter="${kind}" aria-pressed="${filter === kind}">${kind === 'all' ? '全部' : STEP_LOOK[kind][1]} ${count}</button>`).join('');
  const hint = folded.length ? `<button type="button" class="plan-folded" data-plan-filter="all">已折叠 ${folded.length} 个不影响后续的已完成步骤 ▸</button>` : '';
  const zoom = `<div class="plan-zoom" role="group" aria-label="缩放"><button type="button" data-zoom="out" aria-label="缩小" title="缩小（也可在图上滚动滚轮）">−</button><button type="button" class="plan-zoom-level" data-zoom="reset" title="恢复为 100%">100%</button><button type="button" data-zoom="in" aria-label="放大" title="放大（也可在图上滚动滚轮）">+</button><button type="button" data-zoom="fit" aria-label="适应窗口" title="适应窗口（也可双击空白处）"><svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true"><path d="M1.5 5V1.5H5M9 1.5h3.5V5m0 4v3.5H9m-4 0H1.5V9"/></svg></button></div>`;
  return `<section class="brain-plans plan-flow-section"><h2>计划</h2><div class="plan-filters">${bar}${hint}${zoom}</div>
    <div class="plan-stage"><div class="plan-flow"><div class="plan-canvas" data-w="${width}" data-h="${height}" style="width:${width}px;height:${height}px">
      <svg width="${width}" height="${height}"><defs><marker id="plan-arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="8" markerHeight="8" orient="auto"><path d="M1.5 1.5 8 5 1.5 8.5"/></marker></defs>${edges}</svg>${nodes}</div></div><div class="plan-tip" hidden></div></div>${picked ? stepDetail(picked, all, jobs, escape, picked.status === 'done' ? 0 : blocked(picked.id).size) : ''}</section>`;
}

/// 点开的一步：全文、谁负责、先要等哪几步、卡住几件；有派出去的活就能跳过去看、打开那位成员的会话。
function stepDetail(step: PlanStep, all: PlanStep[], jobs: BrainJob[], escape: Escape, held: number) {
  const kind = stepKind(step), [icon, word] = STEP_LOOK[kind];
  const job = jobs.find(item => item.id === step.job);
  const title = (id: number) => all.find(item => item.id === id)?.title || '';
  const rows = [
    step.name || step.owner ? `<p><strong>负责人</strong><span>${escape(step.name || step.owner!)}</span></p>` : '',
    step.depends.length ? `<p><strong>前置步骤</strong><span>${step.depends.map(id => `<button type="button" class="plan-ref" data-plan-pick="${id}">#${id} ${escape(title(id))}</button>`).join('')}</span></p>` : '',
    held ? `<p><strong>阻塞</strong><span>${held} 个未完成的后续步骤依赖此步骤</span></p>` : '',
  ].join('');
  const actions = job ? `<div class="brain-job-actions"><button type="button" class="secondary" data-show-job="${escape(job.id)}">查看任务</button><button type="button" class="text-button" data-open-seat="${escape(job.seat)}">打开 ${escape(job.name)} 的会话</button></div>` : '';
  return `<article class="plan-detail ${kind}"><header><span class="plan-chip"><span aria-hidden="true">${icon}</span>${word}</span><strong>#${step.id} ${escape(step.title)}</strong><button type="button" class="icon-button" data-plan-pick="${step.id}" aria-label="收起">×</button></header>
    ${step.note ? `<p class="plan-detail-note">${escape(step.note)}</p>` : ''}${rows ? `<div class="plan-detail-rows">${rows}</div>` : ''}${actions}</article>`;
}

/// 流程图的视野按项目记着：活一有变化整页就重画，缩放和拖到的位置都留着，换项目才重新摆。
/// auto 跟着窗口宽度自己缩到放得下（最小 60%，再小字就挤了）；fit 是点了「适应窗口」；manual 是手动缩放过。
const ZOOM_STEPS = [0.4, 0.5, 0.6, 0.75, 0.9, 1, 1.25, 1.5, 2];
const MIN = ZOOM_STEPS[0], MAX = ZOOM_STEPS[ZOOM_STEPS.length - 1];
const view = { key: '', mode: 'auto' as 'auto' | 'fit' | 'manual', scale: 1, left: 0, top: 0 };
let viewWatch: ResizeObserver | undefined, viewFrame = 0;

/// 点开一步：它要是在画布视野外就把画布挪过去（只在点开时，重绘不乱挪）。
export function revealPlanNode(root: ParentNode, id: number, scroll: boolean) {
  const node = root.querySelector<HTMLElement>(`.plan-node[data-plan-pick="${id}"]`);
  node?.focus({ preventScroll: true });
  if (scroll) node?.scrollIntoView({ behavior: still() ? 'auto' : 'smooth', block: 'nearest', inline: 'nearest' });
}

/// 指到一步就亮出它前面要等的、后面被它卡住的整条链，其余淡下去；字被截掉的浮出全文。
/// 按住拖着看（卡片上也能拖，挪过几像素才算拖，不然算点），滚轮或右上角按钮缩放，双击空白处适应窗口。
export function bindPlanFlow(root: ParentNode, key: string) {
  viewWatch?.disconnect(); cancelAnimationFrame(viewFrame);
  const stage = root.querySelector<HTMLElement>('.plan-stage'), flow = stage?.querySelector<HTMLElement>('.plan-flow'), canvas = flow?.querySelector<HTMLElement>('.plan-canvas');
  if (!stage || !flow || !canvas) return;
  const tip = stage.querySelector<HTMLElement>('.plan-tip')!, tools = root.querySelector<HTMLElement>('.plan-zoom')!, level = tools.querySelector<HTMLButtonElement>('[data-zoom="reset"]')!;
  const zoomOut = tools.querySelector<HTMLButtonElement>('[data-zoom="out"]')!, zoomIn = tools.querySelector<HTMLButtonElement>('[data-zoom="in"]')!;
  const w = Number(canvas.dataset.w), h = Number(canvas.dataset.h), pad = 24, tall = 520;
  const fits = () => Math.min(1, (flow.clientWidth - pad) / w, (tall - pad) / h);
  const fitScale = () => Math.max(MIN, fits());
  // 缩小了字会挤：卡片里的标题按比例反着放大，屏幕上看着差不多大；再小就只留标题，其余的指上去看。
  const paint = (scale: number) => {
    view.scale = scale;
    stage.style.setProperty('--zoom', String(scale));
    canvas.classList.toggle('far', scale < 0.85);
    canvas.classList.toggle('tiny', scale < 0.6);
    level.textContent = `${Math.round(scale * 100)}%`;
    zoomOut.disabled = scale <= MIN + 1e-3; zoomIn.disabled = scale >= MAX - 1e-3;
  };
  // 没有滚动条了，四边还有内容的就淡出一截，看得出能往那边拖。
  const edge = () => {
    const x = flow.scrollWidth - flow.clientWidth, y = flow.scrollHeight - flow.clientHeight;
    flow.style.setProperty('--fl', flow.scrollLeft > 1 ? '28px' : '0px');
    flow.style.setProperty('--fr', flow.scrollLeft < x - 1 ? '28px' : '0px');
    flow.style.setProperty('--ft', flow.scrollTop > 1 ? '24px' : '0px');
    flow.style.setProperty('--fb', flow.scrollTop < y - 1 ? '24px' : '0px');
  };
  // 画布高度只跟 auto 的比例走：手动缩放时外框不跟着忽高忽低，页面下面的东西不跳。
  const layout = () => {
    const auto = Math.max(0.6, fits());
    flow.style.height = `${Math.min(tall, Math.max(150, h * auto + pad))}px`;
    paint(view.mode === 'auto' ? auto : view.mode === 'fit' ? fitScale() : view.scale);
    edge();
  };
  // 缩放时鼠标底下那一点不动。
  const zoomAt = (scale: number, x: number, y: number) => {
    hideTip();
    const before = canvas.getBoundingClientRect(), fx = (x - before.left) / before.width, fy = (y - before.top) / before.height;
    paint(Math.min(MAX, Math.max(MIN, scale)));
    const after = canvas.getBoundingClientRect();
    flow.scrollLeft += after.left + fx * after.width - x;
    flow.scrollTop += after.top + fy * after.height - y;
    edge();
  };
  const glide = (to: number, mode: typeof view.mode = 'manual') => {
    cancelAnimationFrame(viewFrame);
    view.mode = mode;
    const box = flow.getBoundingClientRect(), x = box.left + box.width / 2, y = box.top + box.height / 2, from = view.scale, start = performance.now();
    if (still()) return zoomAt(to, x, y);
    const step = (now: number) => {
      const t = Math.min(1, (now - start) / 220);
      zoomAt(from + (to - from) * (1 - (1 - t) ** 3), x, y);
      if (t < 1) viewFrame = requestAnimationFrame(step);
    };
    viewFrame = requestAnimationFrame(step);
  };

  let tipTimer = 0;
  const hideTip = () => { clearTimeout(tipTimer); tip.hidden = true; };
  const showTip = (node: HTMLElement) => {
    const title = node.querySelector('strong')!, line = node.querySelector('small')!;
    if (!canvas.classList.contains('far') && title.scrollHeight <= title.clientHeight + 1 && line.scrollWidth <= line.clientWidth + 1) return;
    const foot = [...node.querySelector('.plan-foot')!.children].map(item => item.textContent).filter(Boolean).join(' · ');
    tip.replaceChildren(Object.assign(document.createElement('strong'), { textContent: `#${node.dataset.planPick} ${node.querySelector('.plan-name')!.textContent}` }),
      ...(foot ? [Object.assign(document.createElement('small'), { textContent: foot })] : []));
    tip.hidden = false;
    const box = stage.getBoundingClientRect(), at = node.getBoundingClientRect();
    const above = at.top - box.top - tip.offsetHeight - 8;
    tip.style.left = `${Math.max(8, Math.min(at.left - box.left + at.width / 2 - tip.offsetWidth / 2, box.width - tip.offsetWidth - 8))}px`;
    tip.style.top = `${above >= 4 ? above : at.bottom - box.top + 8}px`;
  };

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
  // 扫过去不亮：在一张卡片上停一下才亮出它的链；移到别的卡片、路过卡片间的空当时原来那条先留着，免得整张图一路明暗闪。
  let hovered: HTMLElement | null = null, traceTimer = 0;
  const settle = (node: HTMLElement | null, wait: number) => { clearTimeout(traceTimer); traceTimer = window.setTimeout(() => trace(node), wait); };
  canvas.addEventListener('pointerover', event => {
    const node = (event.target as Element).closest<HTMLElement>('.plan-node');
    if (node === hovered) return;
    hovered = node; hideTip();
    settle(node, node ? 160 : 240);
    if (node) tipTimer = window.setTimeout(() => showTip(node), 350);
  });
  canvas.addEventListener('pointerleave', () => { hovered = null; settle(null, 120); hideTip(); });
  canvas.addEventListener('focusin', event => {
    const node = (event.target as Element).closest<HTMLElement>('.plan-node');
    clearTimeout(traceTimer); trace(node);
    if (node?.matches(':focus-visible')) showTip(node);
  });
  canvas.addEventListener('focusout', () => { trace(null); hideTip(); });

  flow.addEventListener('pointerdown', event => {
    if (event.button !== 0) return;
    hideTip();
    const from = { x: event.clientX, y: event.clientY, left: flow.scrollLeft, top: flow.scrollTop };
    let panning = false;
    const move = (next: PointerEvent) => {
      const dx = next.clientX - from.x, dy = next.clientY - from.y;
      if (!panning) {
        if (Math.hypot(dx, dy) < 4) return;
        panning = true;
        flow.setPointerCapture(event.pointerId);
        flow.classList.add('panning');
      }
      flow.scrollLeft = from.left - dx; flow.scrollTop = from.top - dy;
    };
    const stop = () => {
      window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', stop); window.removeEventListener('pointercancel', stop);
      if (!panning) return;
      flow.classList.remove('panning');
      // 拖完松手不算点了卡片。
      const swallow = (click: Event) => { click.stopPropagation(); click.preventDefault(); };
      flow.addEventListener('click', swallow, { capture: true, once: true });
      setTimeout(() => flow.removeEventListener('click', swallow, { capture: true }));
    };
    window.addEventListener('pointermove', move); window.addEventListener('pointerup', stop); window.addEventListener('pointercancel', stop);
  });
  // 滚轮在画布上就是缩放（移动靠拖）；要翻页把鼠标挪出画布再滚。
  flow.addEventListener('wheel', event => {
    event.preventDefault();
    cancelAnimationFrame(viewFrame);
    view.mode = 'manual';
    zoomAt(view.scale * Math.exp(-event.deltaY * (event.deltaMode ? 0.05 : 0.002)), event.clientX, event.clientY);
  }, { passive: false });
  flow.addEventListener('dblclick', event => { if (!(event.target as Element).closest('.plan-node')) glide(fitScale(), 'fit'); });
  tools.addEventListener('click', event => {
    const what = (event.target as Element).closest<HTMLElement>('[data-zoom]')?.dataset.zoom;
    if (what === 'in') glide(ZOOM_STEPS.find(scale => scale > view.scale + 0.01) ?? MAX);
    if (what === 'out') glide([...ZOOM_STEPS].reverse().find(scale => scale < view.scale - 0.01) ?? MIN);
    if (what === 'reset') glide(1);
    if (what === 'fit') glide(fitScale(), 'fit');
  });

  if (view.key !== key) Object.assign(view, { key, mode: 'auto', left: 0, top: 0 });
  layout();
  flow.scrollLeft = view.left; flow.scrollTop = view.top;
  edge();
  flow.addEventListener('scroll', () => { view.left = flow.scrollLeft; view.top = flow.scrollTop; edge(); hideTip(); }, { passive: true });
  let width = flow.clientWidth;
  viewWatch = new ResizeObserver(() => {
    if (!flow.isConnected) return viewWatch?.disconnect();
    if (flow.clientWidth !== width) { width = flow.clientWidth; layout(); }
  });
  viewWatch.observe(flow);
}

/// 主脑派活：主脑现在在干嘛、派出去的每件活。在做的在上面，做完的收在下面。
export function brainJobsPage(project: string, overview: BrainOverview | null, brainBusy: boolean, escape: Escape, avatar: (seat: string) => string, open: Set<string>, planFilter = '', planPick: number | null = null) {
  const head = `<header class="chat-header"><div><h1>主脑任务</h1><p>${escape(project)} · 主脑的计划与分配给成员的任务</p></div><div class="header-actions"><button class="secondary" data-project-return-team>团队对话</button>${overview?.chat ? `<button class="secondary" data-open-seat="${escape(overview.seat)}">主脑会话</button>` : ''}</div></header>`;
  if (!overview) return `${head}<div class="project-progress-content brain-jobs-content"><p class="inquiry-empty">正在读取…</p></div>`;
  const live = overview.jobs.filter(jobLive), ended = overview.jobs.filter(job => !jobLive(job));
  const status = `<section class="brain-status"><span class="brain-status-dot ${brainBusy ? 'busy' : ''}"></span><strong>主脑${brainBusy ? '正在回复' : '空闲'}</strong><small>${live.length ? `${live.length} 项任务进行中` : '暂无进行中的任务'}${overview.waiting ? ` · ${overview.waiting} 条结果或回答待送达主脑` : ''}</small></section>`;
  const body = live.length
    ? live.map(job => card(job, escape, avatar, open)).join('')
    : `<p class="inquiry-empty">${ended.length ? '暂无进行中的任务。' : '主脑尚未分配任务。在团队对话中提出需求后，主脑会将任务分配给成员，已分配的任务列在此处。'}</p>`;
  const history = ended.length ? `<details class="brain-jobs-ended" data-job-fold="ended" ${open.has('ended') ? 'open' : ''}><summary>已结束（${ended.length}）</summary>${ended.map(job => card(job, escape, avatar, open)).join('')}</details>` : '';
  return `${head}<div class="project-progress-content brain-jobs-content">${status}${planFlow(overview.steps, overview.jobs, escape, planFilter, planPick)}${usageSection(overview.usage)}${plansSection(overview.plans, escape, open)}${body}${history}</div>`;
}
