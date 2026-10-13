import { renderMath, withMath } from './math';
/** Small, escaped text renderer. Native output cannot insert HTML or scripts. */
export function agentMarkdown(text: string, escape: (value: string | number) => string): string {
  const inline = (line: string) => line.split(/(`[^`]*`)/g).map(piece => piece.startsWith('`') && piece.endsWith('`')
    ? `<code>${escape(piece.slice(1, -1))}</code>`
    : withMath(piece, plain => escape(plain).replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>'))).join('');
  const output: string[] = [];
  let paragraph: string[] = [];
  let list: 'ul' | 'ol' | null = null;
  let fence: string | null = null;
  let code: string[] = [];
  const flush = () => { if (paragraph.length) output.push(`<p>${paragraph.map(inline).join('<br>')}</p>`); paragraph = []; };
  const closeList = () => { if (list) output.push(`</${list}>`); list = null; };
  // 跨行的公式块（`$$` 或 `\[` 独占一行开头、后面几行才闭合）整体交给 KaTeX。
  let math: { close: string; lines: string[] } | null = null;
  for (const line of text.split('\n')) {
    if (math) {
      const end = line.indexOf(math.close);
      if (end < 0) { math.lines.push(line); continue; }
      math.lines.push(line.slice(0, end));
      output.push(renderMath(math.lines.join('\n'), true) || `<pre><code>${escape(math.lines.join('\n'))}</code></pre>`);
      math = null;
      continue;
    }
    const open = !fence && line.trim().match(/^(\$\$|\\\[)(.*)$/);
    if (open && !open[2].includes(open[1] === '$$' ? '$$' : '\\]')) { flush(); closeList(); math = { close: open[1] === '$$' ? '$$' : '\\]', lines: [open[2]] }; continue; }
    const marker = line.match(/^\s*(`{3,}|~{3,})/);
    if (fence) {
      if (marker && marker[1][0] === fence[0] && marker[1].length >= fence.length) {
        output.push(`<pre><code>${escape(code.join('\n'))}</code></pre>`); fence = null; code = [];
      } else code.push(line);
      continue;
    }
    if (marker) { flush(); closeList(); fence = marker[1]; continue; }
    const item = line.match(/^\s*(?:([-*+])|\d+[.)])\s+(.+)$/);
    if (item) {
      flush(); const kind = item[1] ? 'ul' : 'ol';
      if (list !== kind) { closeList(); list = kind; output.push(`<${kind}>`); }
      output.push(`<li>${inline(item[2])}</li>`); continue;
    }
    closeList();
    if (!line.trim()) { flush(); continue; }
    const heading = line.match(/^#{1,6}\s+(.+)$/);
    if (heading) { flush(); output.push(`<h3>${inline(heading[1])}</h3>`); continue; }
    if (/^>\s?/.test(line)) { flush(); output.push(`<blockquote>${inline(line.replace(/^>\s?/, ''))}</blockquote>`); continue; }
    paragraph.push(line);
  }
  flush(); closeList();
  if (fence) output.push(`<pre><code>${escape(code.join('\n'))}</code></pre>`);
  // 流式中还没闭合的公式块先按原文显示，闭合后再渲染。
  if (math) output.push(`<p>${escape(math.close === '$$' ? '$$' : '\\[')}${math.lines.map(line => escape(line)).join('<br>')}</p>`);
  return output.join('');
}
