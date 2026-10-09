/** Small, escaped text renderer. Native output cannot insert HTML or scripts. */
export function agentMarkdown(text: string, escape: (value: string | number) => string): string {
  const inline = (line: string) => line.split(/(`[^`]*`)/g).map(piece => piece.startsWith('`') && piece.endsWith('`')
    ? `<code>${escape(piece.slice(1, -1))}</code>`
    : escape(piece).replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>')).join('');
  const output: string[] = [];
  let paragraph: string[] = [];
  let list: 'ul' | 'ol' | null = null;
  let fence: string | null = null;
  let code: string[] = [];
  const flush = () => { if (paragraph.length) output.push(`<p>${paragraph.map(inline).join('<br>')}</p>`); paragraph = []; };
  const closeList = () => { if (list) output.push(`</${list}>`); list = null; };
  for (const line of text.split('\n')) {
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
  return output.join('');
}
