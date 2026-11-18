import dagre from '@dagrejs/dagre';

/// 计划流程图的排版：卡片的列和上下次序交给 dagre（分层、减交叉、长边留出穿过各列的通道），
/// 连线自己走正交线：只在列间空当里竖着走，每条竖线占一条轨道，轨道左右次序按交叉最少排，空当按轨道数加宽。
/// 一步的出线合成一根总线从右边中点出；进同一步的线在左边各占一个口，不会几条线挤一个点、分不清谁连谁。

export type LayoutNode = { id: string; w: number; h: number };
export type PlanLayout = { width: number; height: number; at: Map<string, { x: number; y: number }>; paths: Map<string, string> };

const TRACK = 10;      // 相邻竖线的间距
const PAD = 18;        // 卡片边到最近一条竖线，留给箭头和拐角
const RADIUS = 6;      // 拐角圆角
const PORT_GAP = 12;   // 进同一步的几条线在左边的间距
const PORT_EDGE = 14;  // 入口离卡片上下边至少这么远
const SEP = 8;         // 同一条轨道上前后两段竖线至少隔开这么远
const EPS = 0.5;

/// 一个空当里的一段竖线：左边从哪些高度进来、右边从哪些高度出去（同一步的出线合成一段，左边只有一个口）。
type Net = { lefts: number[]; rights: number[]; lo: number; hi: number; track: number };
const vertical = (net: Net) => net.hi - net.lo > EPS;

/// a 摆在 b 左边时交叉几次：b 左边进来的横线要穿过 a 的竖线，a 右边出去的横线要穿过 b 的竖线。
/// 擦着拐角过也算，免得看着像连在一起。
function crossings(a: Net, b: Net) {
  const inside = (y: number, net: Net) => vertical(net) && y >= net.lo - EPS && y <= net.hi + EPS;
  return b.lefts.filter(y => inside(y, a)).length + a.rights.filter(y => inside(y, b)).length;
}

/// 空当里各段竖线从左到右的次序：先按跨度从长到短逐个插到交叉最少的位置，再挨个挪动直到挪不出更少的。
function arrange(nets: Net[]) {
  const n = nets.length;
  const cost = nets.map(a => nets.map(b => a === b ? 0 : crossings(a, b)));
  const placed = (seq: number[], item: number) => {
    let best = 0, least = Infinity;
    for (let at = 0; at <= seq.length; at++) {
      let sum = 0;
      for (let k = 0; k < seq.length; k++) sum += k < at ? cost[seq[k]][item] : cost[item][seq[k]];
      if (sum < least) { least = sum; best = at; }
    }
    return [best, least] as const;
  };
  const seq: number[] = [];
  for (const item of [...nets.keys()].sort((a, b) => (nets[b].hi - nets[b].lo) - (nets[a].hi - nets[a].lo))) seq.splice(placed(seq, item)[0], 0, item);
  for (let pass = 0, moved = true; moved && pass < 8; pass++) {
    moved = false;
    for (let k = 0; k < n; k++) {
      const item = seq[k], rest = seq.filter(other => other !== item);
      let now = 0;
      for (let m = 0; m < n; m++) if (m !== k) now += m < k ? cost[seq[m]][item] : cost[item][seq[m]];
      const [at, least] = placed(rest, item);
      if (least < now) { rest.splice(at, 0, item); seq.splice(0, n, ...rest); moved = true; }
    }
  }
  // 按次序发轨道：和前面重叠的竖线往右让一条，不重叠的可以共用一条。
  for (let k = 0; k < n; k++) {
    const net = nets[seq[k]];
    if (!vertical(net)) continue;
    net.track = 0;
    for (let m = 0; m < k; m++) {
      const other = nets[seq[m]];
      if (vertical(other) && net.lo - SEP <= other.hi && other.lo - SEP <= net.hi) net.track = Math.max(net.track, other.track + 1);
    }
  }
  return nets.reduce((most, net) => Math.max(most, net.track + 1), 0);
}

/// 折线转成带小圆角的路径；拐角半径不超过相邻两段的一半。
function rounded(points: [number, number][]) {
  const round = (value: number) => Math.round(value * 10) / 10;
  const kept = points.filter((point, i) => {
    if (i === 0 || i === points.length - 1) return true;
    const [px, py] = points[i - 1], [nx, ny] = points[i + 1];
    if (Math.abs(point[0] - px) < EPS && Math.abs(point[1] - py) < EPS) return false;
    return !((Math.abs(px - point[0]) < EPS && Math.abs(nx - point[0]) < EPS) || (Math.abs(py - point[1]) < EPS && Math.abs(ny - point[1]) < EPS));
  });
  let d = `M${round(kept[0][0])} ${round(kept[0][1])}`;
  for (let i = 1; i < kept.length - 1; i++) {
    const [x0, y0] = kept[i - 1], [x, y] = kept[i], [x1, y1] = kept[i + 1];
    const before = Math.hypot(x - x0, y - y0), after = Math.hypot(x1 - x, y1 - y);
    const r = Math.min(RADIUS, before / 2, after / 2);
    if (r < EPS) { d += ` L${round(x)} ${round(y)}`; continue; }
    d += ` L${round(x + (x0 - x) / before * r)} ${round(y + (y0 - y) / before * r)} Q${round(x)} ${round(y)} ${round(x + (x1 - x) / after * r)} ${round(y + (y1 - y) / after * r)}`;
  }
  const [lx, ly] = kept[kept.length - 1];
  return `${d} L${round(lx)} ${round(ly)}`;
}

export function layoutPlan(nodes: LayoutNode[], edges: [string, string][], gap = { node: 14, rank: 40 }): PlanLayout {
  const graph = new dagre.graphlib.Graph();
  graph.setGraph({ rankdir: 'LR', nodesep: gap.node, ranksep: gap.rank, edgesep: 10 });
  graph.setDefaultEdgeLabel(() => ({}));
  for (const node of nodes) graph.setNode(node.id, { width: node.w, height: node.h });
  for (const [from, to] of edges) graph.setEdge(from, to);
  dagre.layout(graph);
  const size = new Map(nodes.map(node => [node.id, node]));
  const centre = (id: string) => graph.node(id) as { x: number; y: number };
  // 同一列的卡片中心横坐标相同；长边在各列里的通道是 dagre 摆在该列中线上的虚拟点。
  const columns: number[] = [];
  for (const node of nodes) { const x = centre(node.id).x; if (!columns.some(at => Math.abs(at - x) < 1)) columns.push(x); }
  columns.sort((a, b) => a - b);
  const column = (x: number) => columns.findIndex(at => Math.abs(at - x) < 1);
  const columnOf = new Map(nodes.map(node => [node.id, column(centre(node.id).x)]));
  const wide = columns.map((_, c) => Math.max(...nodes.filter(node => columnOf.get(node.id) === c).map(node => node.w)));
  type Route = { from: string; to: string; first: number; last: number; corridor: Map<number, number>; port: number; nets: Net[] };
  const routes: Route[] = [];
  for (const [from, to] of edges) {
    const first = columnOf.get(from)!, last = columnOf.get(to)!;
    if (last <= first) continue;
    const points = (graph.edge(from, to) as { points: { x: number; y: number }[] }).points;
    const corridor = new Map<number, number>();
    for (let c = first + 1; c < last; c++) {
      const hit = points.find(point => Math.abs(point.x - columns[c]) < 1);
      const sy = centre(from).y, ty = centre(to).y;
      corridor.set(c, hit ? hit.y : sy + (ty - sy) * (c - first) / (last - first));
    }
    routes.push({ from, to, first, last, corridor, port: centre(to).y, nets: [] });
  }
  // 从哪个高度进最后一个空当：入口按它从上到下排，进同一步的线不互相绞；
  // 入口尽量就在这个高度（能直着进就不拐），几个入口之间至少隔开一点，挤不下再往上推。
  const arrival = (route: Route) => route.last - 1 === route.first ? centre(route.from).y : route.corridor.get(route.last - 1)!;
  for (const node of nodes) {
    const incoming = routes.filter(route => route.to === node.id).sort((a, b) => arrival(a) - arrival(b) || centre(a.from).y - centre(b.from).y);
    const top = centre(node.id).y - node.h / 2 + PORT_EDGE, bottom = centre(node.id).y + node.h / 2 - PORT_EDGE;
    const step = incoming.length > 1 ? Math.min(PORT_GAP, (bottom - top) / (incoming.length - 1)) : 0;
    const ports = incoming.map(route => Math.min(bottom, Math.max(top, arrival(route))));
    for (let i = 1; i < ports.length; i++) ports[i] = Math.max(ports[i], ports[i - 1] + step);
    for (let i = ports.length - 1; i >= 0; i--) ports[i] = Math.min(ports[i], i === ports.length - 1 ? bottom : ports[i + 1] - step);
    incoming.forEach((route, i) => { route.port = ports[i]; });
  }
  // 每个空当里的竖线段：同一步的出线在它右边那个空当里合成一段总线。
  const gutters = columns.slice(0, -1).map(() => [] as Net[]);
  const buses = new Map<string, Net>();
  for (const route of routes) {
    for (let g = route.first; g < route.last; g++) {
      const left = g === route.first ? centre(route.from).y : route.corridor.get(g)!;
      const right = g === route.last - 1 ? route.port : route.corridor.get(g + 1)!;
      let net = g === route.first ? buses.get(route.from) : undefined;
      if (!net) {
        net = { lefts: [left], rights: [], lo: left, hi: left, track: -1 };
        gutters[g].push(net);
        if (g === route.first) buses.set(route.from, net);
      }
      net.rights.push(right);
      net.lo = Math.min(net.lo, right);
      net.hi = Math.max(net.hi, right);
      route.nets.push(net);
    }
  }
  const widths = gutters.map(nets => { const tracks = arrange(nets); return tracks ? Math.max(gap.rank, 2 * PAD + (tracks - 1) * TRACK) : gap.rank; });
  const lefts: number[] = [];
  columns.forEach((_, c) => lefts.push(c ? lefts[c - 1] + wide[c - 1] + widths[c - 1] : 0));
  const trackX = (g: number, net: Net) => {
    const tracks = gutters[g].reduce((most, other) => Math.max(most, other.track + 1), 0);
    const start = lefts[g] + wide[g] + (widths[g] - (tracks - 1) * TRACK) / 2;
    return start + Math.max(net.track, 0) * TRACK;
  };
  const at = new Map(nodes.map(node => {
    const c = columnOf.get(node.id)!;
    return [node.id, { x: lefts[c] + (wide[c] - node.w) / 2, y: centre(node.id).y - node.h / 2 }] as const;
  }));
  const paths = new Map<string, string>();
  let bottom = Math.max(0, ...nodes.map(node => at.get(node.id)!.y + node.h));
  for (const route of routes) {
    const source = at.get(route.from)!, target = at.get(route.to)!;
    const points: [number, number][] = [[source.x + size.get(route.from)!.w, centre(route.from).y]];
    route.nets.forEach((net, k) => {
      const g = route.first + k;
      const left = k === 0 ? centre(route.from).y : route.corridor.get(g)!;
      const right = g === route.last - 1 ? route.port : route.corridor.get(g + 1)!;
      if (!vertical(net)) return;
      const x = trackX(g, net);
      points.push([x, left], [x, right]);
    });
    points.push([target.x - 1, route.port]);
    for (const [, y] of points) bottom = Math.max(bottom, y);
    paths.set(`${route.from}>${route.to}`, rounded(points));
  }
  const width = lefts.length ? lefts[lefts.length - 1] + wide[wide.length - 1] : 0;
  return { width, height: Math.ceil(bottom + 4), at, paths };
}
