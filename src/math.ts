import katex from 'katex';
import 'katex/dist/katex.min.css';

/**
 * 回复里的 LaTeX：`$$…$$`、`\[…\]` 按公式块显示，`$…$`、`\(…\)` 行内显示。
 * 其余文字仍交给调用方转义；KaTeX 自己转义公式内容且不信任 \href 之类的命令，不会注入 HTML。
 * 行内 `$` 要求紧贴内容（`$5 和 $10` 这类金额不会被当成公式）。
 */
const MATH = /\$\$([\s\S]+?)\$\$|\\\[([\s\S]+?)\\\]|\\\(([\s\S]+?)\\\)|(?<![\\$\w])\$(?![\s$])([^$\n]+?)(?<![\s\\])\$(?![\w$])/g;

export function renderMath(tex: string, display: boolean) {
  try { return katex.renderToString(tex.trim(), { displayMode: display, throwOnError: false, output: 'html', strict: false, trust: false }); }
  catch { return ''; }
}

/** 把文字里的公式渲染出来，公式之外的部分用 `plain` 处理（通常是转义）。 */
export function withMath(text: string, plain: (value: string) => string) {
  if (!/\$|\\[[(]/.test(text)) return plain(text);
  let html = '';
  let cursor = 0;
  for (const match of text.matchAll(MATH)) {
    const display = match[1] ?? match[2];
    const tex = display ?? match[3] ?? match[4];
    const rendered = renderMath(tex, display !== undefined);
    if (!rendered) continue;
    html += plain(text.slice(cursor, match.index)) + rendered;
    cursor = match.index! + match[0].length;
  }
  return html + plain(text.slice(cursor));
}
