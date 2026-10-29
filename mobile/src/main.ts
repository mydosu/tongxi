// 同席手机薄壳的「配对」页：这一页只做一件事——拿到用户粘贴的桌面同席网关完整地址，
// 存下来，然后整页跳过去。跳过去之后跑的是桌面那份前端（同一份 src/），薄壳不再插手。

/// 存的就是桌面「远程访问」卡片里那条带令牌的 url，薄壳自己不解令牌、不发请求。
const STORAGE_KEY = 'tongxi.pair.url';
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
    connect(previous);
    return;
  }
}

if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot, { once: true });
else boot();
