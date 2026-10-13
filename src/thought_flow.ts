/**
 * 后端在思考文本里用 `\x1e<正文 UTF-16 偏移>\x1f` 标出每段思考出现在正文的哪里，
 * 这里把“思考 / 正文”拆回交替的片段，像 Hermes、Codex CLI、Claude Code 那样一段想、一段说。
 */
export interface FlowPart { kind: 'thought' | 'text'; text: string }

const MARK = /\x1e(\d+)\x1f/g;

/** 不穿插显示时（例如带提问卡片的回复），去掉标记、按段落连起来。 */
export function plainThought(thought: string) {
  return thought.replace(MARK, '\n\n').trim();
}

/** 没有标记（思考只出现在正文之前）时返回 null，沿用“思考块 + 正文”的老布局。 */
export function thoughtFlow(thought: string, body: string): FlowPart[] | null {
  if (!thought.includes('\x1e')) return null;
  const segments: { at: number; text: string }[] = [];
  let at = 0;
  let start = 0;
  for (const match of thought.matchAll(MARK)) {
    segments.push({ at, text: thought.slice(start, match.index) });
    at = Number(match[1]);
    start = match.index! + match[0].length;
  }
  segments.push({ at, text: thought.slice(start) });
  const parts: FlowPart[] = [];
  let cursor = 0;
  for (const segment of segments) {
    const end = Math.min(Math.max(segment.at, cursor), body.length);
    const said = body.slice(cursor, end).trim();
    if (said) parts.push({ kind: 'text', text: said });
    cursor = end;
    const thinking = segment.text.trim();
    if (thinking) parts.push({ kind: 'thought', text: thinking });
  }
  const rest = body.slice(cursor).trim();
  if (rest) parts.push({ kind: 'text', text: rest });
  return parts;
}
