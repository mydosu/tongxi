// 同席手机薄壳的两块屏（同一页里，靠 <html data-screen> 切）：
//   加载页——存过地址就直接跳，先把「正在连接…」画出来，不让用户干瞪着配对表单；
//   配对页——没存过地址、或带 ?repair=1 才出来，拿到粘贴的桌面同席网关完整地址后存下来。
// 跳过去之后跑的是桌面那份前端（同一份 src/），薄壳不再插手。

/// 存的就是桌面「远程访问」卡片里那条带令牌的 url，薄壳自己不解令牌、不发请求。
/// index.html 的内联脚本读同一个键决定首帧画哪块屏，改键名要两边一起改。
const STORAGE_KEY = 'tongxi.pair.url';
/// 加载页停留这么久还没跳走，就把「重新配对」的出口显出来。
const SLOW_AFTER = 8000;
/// 探活单次超时 / 失败后重探间隔。
const PROBE_TIMEOUT = 4000;
const PROBE_RETRY = 3000;
const NOT_PAIRING_CODE = '这不是同席的访问地址，请粘贴桌面「复制访问地址」得到的那条';

const input = document.querySelector<HTMLInputElement>('#address')!;
const errorLine = document.querySelector<HTMLElement>('#error')!;
const pasteForm = document.querySelector<HTMLFormElement>('#paste-form')!;

/// 只认薄壳能直接打开的那种地址：http（网关没有证书）、主机落在 Tailscale 的
/// 100.64.0.0/10 或 *.ts.net、并且带 token 参数。别的一律当粘错了。
export function pairingUrl(raw: string): string | null {
  let url: URL;
  try {
    url = new URL(raw.trim());
  } catch {
    return null;
  }
  if (url.protocol !== 'http:') return null;
  const host = url.hostname.toLowerCase();
  const tailnetIp = /^100\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(host);
  // 100.64.0.0/10 = 100.64.0.0 ~ 100.127.255.255
  const inTailnetRange = tailnetIp !== null && Number(tailnetIp[1]) >= 64 && Number(tailnetIp[1]) <= 127;
  const tailnetDns = host.endsWith('.ts.net');
  if (!inTailnetRange && !tailnetDns) return null;
  if (!(url.searchParams.get('token') ?? '').trim()) return null;
  return url.toString();
}

function showError(message: string) {
  errorLine.textContent = message;
  errorLine.hidden = false;
}

function clearError() {
  errorLine.hidden = true;
  errorLine.textContent = '';
}

function store(raw: string): string | null {
  const url = pairingUrl(raw);
  if (!url) {
    showError(NOT_PAIRING_CODE);
    return null;
  }
  try {
    localStorage.setItem(STORAGE_KEY, url);
  } catch {
    // 存不住（隐私模式/存储满）也照样跳，只是下次要重新配对。
  }
  return url;
}

/// 整页跳过去。地址在应用自己的 webview 里打开（tauri.conf.json 里没有把导航
/// 交给系统浏览器，lib.rs 的 on_navigation 也是全部放行），不会甩到 Chrome。
function connect(url: string) {
  location.href = url;
}

function savedUrl(): string {
  try {
    return localStorage.getItem(STORAGE_KEY) ?? '';
  } catch {
    return '';
  }
}

pasteForm.addEventListener('submit', event => {
  event.preventDefault();
  clearError();
  const url = store(input.value);
  if (url) connect(url);
});

/// 回来路上带 ?repair=1 就是要换一台电脑/换了令牌：停在配对页，旧地址只用来预填。
/// 顺便把存的那条清掉，免得下次冷启动又跳回已经 401 的旧地址、在配对页和页面之间来回弹。
function boot() {
  const repairing = new URLSearchParams(location.search).get('repair') === '1';
  const previous = savedUrl();

  if (repairing) {
    if (previous) {
      input.value = previous;
      try { localStorage.removeItem(STORAGE_KEY); } catch { /* 清不掉也不影响下面 */ }
    }
    input.focus();
    return;
  }

  if (previous) {
    // Tailscale 没开时 WebView 直接导航会落到系统错误页、只能杀进程：先探活再跳。
    // 地址仍可能失效（令牌换了）：8 秒还没跳走就显出「换一台电脑」出口。
    setTimeout(() => { document.documentElement.dataset.slow = '1'; }, SLOW_AFTER);
    void probeAndConnect(previous);
  }
}

/// 对地址的 origin 发一次 no-cors 请求：能 resolve 就说明网关可达（响应不透明、不读内容），
/// reject/超时当连不上——显示提示、立刻亮出口，每 3 秒自动再探，通了自动跳。
/// 薄壳页是 http://tauri.localhost（非安全上下文），只用 fetch / AbortController。
async function probeAndConnect(url: string) {
  const root = document.documentElement;
  for (;;) {
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), PROBE_TIMEOUT);
    try {
      await fetch(new URL(url).origin + '/', { mode: 'no-cors', cache: 'no-store', signal: abort.signal });
      connect(url);
      return;
    } catch {
      root.dataset.offline = '1';
    } finally {
      clearTimeout(timer);
    }
    await new Promise(resolve => setTimeout(resolve, PROBE_RETRY));
  }
}

if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot, { once: true });
else boot();
