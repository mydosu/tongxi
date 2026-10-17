/**
 * 后端在思考文本里用几种标记记下正文的分段位置（偏移都是正文的 UTF-16 长度）：
 * `\x1e<偏移>\x1f` 下面这段思考出现在这里；`\x1d<偏移>\x1f` 正文在这里另起一段（新的回复条目）；
 * `\x1c<偏移>:<消息 id>\x1f` 用户在这里插话；`\x1b<偏移>:<工具事件 id>\x1f` 这里调用了一次工具。
 * 这里把它们拆回按时间排列的片段，像 Hermes、Codex CLI、Claude Code 那样一段想、一次调用、一段说，
 * 插话之后的回复另起一条。
 */
export interface FlowPart { kind: 'thought' | 'text' | 'steer' | 'tool'; text: string }

const MARK = /\x1e(\d+)\x1f|\x1d(\d+)\x1f|\x1c(\d+):([^\x1f]*)\x1f|\x1b(\d+):([^\x1f]*)\x1f/g;

/**
 * 思考框是纯文本：Codex 的思考摘要是一串加粗小标题（`**Checking X**`），前后紧挨着拼在一起，
 * 这里每个标题另起一行并去掉加粗记号，不然满屏都是 `****`。
 */
export function tidyThought(text: string) {
  return text.replace(/\*\*\*\*/g, '**\n**').replace(/\*\*([^*\n]+?)\*\*/g, '$1');
}

/** 不穿插显示时（例如带提问卡片的回复），去掉标记、按段落连起来。 */
export function plainThought(thought: string) {
  return tidyThought(thought.replace(MARK, (_, at) => at === undefined ? '' : '\n\n')).trim();
}

/** 回复里插过话的消息 id：这些消息跟着回复显示，不再按自己的时间单独排。 */
export function steerIds(thought: string | undefined) {
  return [...(thought || '').matchAll(MARK)].flatMap(match => match[4] === undefined ? [] : [match[4]]);
}

/** 没有标记（思考只出现在正文之前、正文只有一段）时返回 null，沿用“思考块 + 正文”的老布局。 */
export function thoughtFlow(thought: string, body: string): FlowPart[] | null {
  if (!/[\x1b-\x1e]/.test(thought)) return null;
  const parts: FlowPart[] = [];
  let cursor = 0;
  let start = 0;
  const say = (at: number) => {
    const end = Math.min(Math.max(at, cursor), body.length);
    const said = body.slice(cursor, end).trim();
    if (said) parts.push({ kind: 'text', text: said });
    cursor = end;
  };
  const think = (text: string) => { if (text.trim()) parts.push({ kind: 'thought', text: tidyThought(text).trim() }); };
  for (const match of thought.matchAll(MARK)) {
    think(thought.slice(start, match.index));
    start = match.index! + match[0].length;
    say(Number(match[1] ?? match[2] ?? match[3] ?? match[5]));
    if (match[4] !== undefined) parts.push({ kind: 'steer', text: match[4] });
    if (match[6] !== undefined) parts.push({ kind: 'tool', text: match[6] });
  }
  think(thought.slice(start));
  say(body.length);
  return parts;
}
