declare module 'katex' {
  const katex: { renderToString(tex: string, options?: { displayMode?: boolean; throwOnError?: boolean; output?: 'html' | 'mathml' | 'htmlAndMathml'; strict?: boolean | string; trust?: boolean }): string };
  export default katex;
}
declare module 'katex/dist/katex.min.css';
