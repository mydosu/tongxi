// 计划流程图走线自检：随机造几十步的计划，检查连线只走横竖、不穿卡片、竖线只在列间空当、
// 不同步骤出来的线不叠在同一条线上。跑法：node --experimental-strip-types scripts/check_plan_layout.mjs
import assert from 'node:assert/strict';
import { layoutPlan } from '../src/plan_layout.ts';

const W = 170, H = 84;
let seed = 7;
const random = () => ((seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648);

/// 路径里的折点：起点、每个圆角的控制点（就是原折线的拐点）、终点。
function polyline(d) {
  const numbers = d.match(/[MLQ][^MLQ]*/g).map(part => [part[0], part.slice(1).trim().split(/\s+/).map(Number)]);
  const points = [numbers[0][1]];
  for (const [kind, values] of numbers.slice(1)) if (kind === 'Q') points.push(values.slice(0, 2));
  points.push(numbers[numbers.length - 1][1]);
  return points;
}

let total = 0;
for (let round = 0; round < 60; round++) {
  const count = 8 + Math.floor(random() * 45);
  const nodes = [], edges = [];
  for (let id = 1; id <= count; id++) {
    nodes.push({ id: String(id), w: W, h: H });
    const deps = id === 1 ? 0 : Math.floor(random() * 4);
    const seen = new Set();
    for (let k = 0; k < deps; k++) {
      // 多数依赖前面几步，少数连到很早的步骤，造出跨好几列的长边。
      const from = random() < 0.75 ? Math.max(1, id - 1 - Math.floor(random() * 4)) : 1 + Math.floor(random() * (id - 1));
      if (!seen.has(from)) { seen.add(from); edges.push([String(from), String(id)]); }
    }
  }
  const layout = layoutPlan(nodes, edges);
  assert.equal(layout.paths.size, edges.length, '每条依赖都有连线');
  const cards = nodes.map(node => ({ id: node.id, ...layout.at.get(node.id) }));
  const columns = [...new Set(cards.map(card => card.x))];
  const segments = [];
  for (const [key, d] of layout.paths) {
    const [from, to] = key.split('>');
    const points = polyline(d);
    const source = layout.at.get(from), target = layout.at.get(to);
    assert.ok(Math.abs(points[0][0] - (source.x + W)) < 0.2, `${key} 从上游卡片右边出`);
    assert.ok(Math.abs(points[points.length - 1][0] - (target.x - 1)) < 0.2, `${key} 进下游卡片左边`);
    assert.ok(points[points.length - 1][1] > target.y + 4 && points[points.length - 1][1] < target.y + H - 4, `${key} 入口在卡片高度里`);
    for (let i = 1; i < points.length; i++) {
      const [x0, y0] = points[i - 1], [x1, y1] = points[i];
      assert.ok(Math.abs(x0 - x1) < 0.2 || Math.abs(y0 - y1) < 0.2, `${key} 只走横竖`);
      segments.push({ key, from, x0: Math.min(x0, x1), x1: Math.max(x0, x1), y0: Math.min(y0, y1), y1: Math.max(y0, y1), upright: Math.abs(x0 - x1) < 0.2 });
      for (const card of cards) {
        const inside = Math.max(x0, x1) > card.x + 1 && Math.min(x0, x1) < card.x + W - 1 && Math.max(y0, y1) > card.y + 1 && Math.min(y0, y1) < card.y + H - 1;
        assert.ok(!inside, `${key} 穿过了第 ${card.id} 步的卡片`);
      }
    }
  }
  for (const segment of segments.filter(item => item.upright && item.y1 - item.y0 > 0.2)) {
    assert.ok(!columns.some(x => segment.x0 > x + 0.5 && segment.x0 < x + W - 0.5), `${segment.key} 的竖线落在卡片列里`);
  }
  // 不同步骤出来的线不能叠在一起（同一步的总线本来就共用）。
  for (let i = 0; i < segments.length; i++) for (let j = i + 1; j < segments.length; j++) {
    const a = segments[i], b = segments[j];
    if (a.from === b.from || a.upright !== b.upright) continue;
    const overlap = a.upright
      ? Math.abs(a.x0 - b.x0) < 0.5 && Math.min(a.y1, b.y1) - Math.max(a.y0, b.y0) > 1
      : Math.abs(a.y0 - b.y0) < 0.5 && Math.min(a.x1, b.x1) - Math.max(a.x0, b.x0) > 1;
    assert.ok(!overlap, `${a.key} 和 ${b.key} 叠在同一条线上`);
  }
  total += edges.length;
}
console.log(`plan layout ok: 60 plans, ${total} edges`);
