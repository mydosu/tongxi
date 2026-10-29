// 前端到后端只有这一条通道：桌面窗口里走 Tauri 的 invoke / events，
// 手机浏览器里走桌面同席起的 HTTP 网关（POST /rpc/<command>、SSE /events）。
// 调用处一律从 './transport' 取 invoke / listen / emit，签名与 @tauri-apps/api 对齐，一字不改。

import { invoke as tauriInvoke, isTauri as tauriPresent, type InvokeArgs } from '@tauri-apps/api/core';
import { emit as tauriEmit, listen as tauriListen, type UnlistenFn } from '@tauri-apps/api/event';


/// 远程模式下和 Tauri 的 Event<T> 同形：处理函数只认 event 名和 payload。
export type TransportEvent<T> = { event: string; payload: T };
export type TransportHandler<T> = (event: TransportEvent<T>) => void;

/// 远程 = 页面不是 Tauri 窗口里打开的（手机浏览器、局域网里的别的电脑、薄壳 App 跳转后的页面）。
// 薄壳 APK 会整页跳到 http://100.x:47680，Tauri 可能仍把 __TAURI_INTERNALS__ 注进这个远程页面：
// 光看 isTauri() 会误走 IPC 而失败，所以页面不在本地 origin（桌面 dev 的 localhost、正式包的 tauri.localhost）也算远程。
const localOrigin = /^(localhost|127\.0\.0\.1|\[::1\]|tauri\.localhost)$/.test(location.hostname) || location.protocol === 'tauri:';
export const isRemote = !tauriPresent() || !localOrigin;
/// 真正在桌面 Tauri 窗口里（和 isRemote 互斥）；调用处用它判断能不能用窗口、网关这类桌面能力。
export const isTauri = () => !isRemote;

// 手机经 http://100.x.y.z 打开不是安全上下文，浏览器没有 crypto.randomUUID（getRandomValues 还在）：补一个 v4。
if (isRemote && typeof crypto.randomUUID !== 'function') {
  (crypto as { randomUUID: () => string }).randomUUID = () => {
    const bytes = crypto.getRandomValues(new Uint8Array(16));
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    const hex = [...bytes].map(byte => byte.toString(16).padStart(2, '0')).join('');
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  };
}

/// 地址栏带进来的令牌存这里，之后刷新页面靠它继续认人。
const TOKEN_KEY = 'tongxi-remote-token';
let incomingToken = '';

/// 地址栏里的 token 只在进门那一次有效：存下来，再把它从地址栏抹掉（截图、分享都不带它）。
function adoptTokenFromUrl() {
  const params = new URLSearchParams(location.search);
  const token = params.get('token');
  if (!token) return;
  incomingToken = token;
  try { localStorage.setItem(TOKEN_KEY, token); } catch { /* 存不住（隐私模式）就这次用 */ }
  params.delete('token');
  const query = params.toString();
  try {
    history.replaceState(history.state, '', `${location.pathname}${query ? `?${query}` : ''}${location.hash}`);
  } catch { /* 改不了地址也不影响使用 */ }
}

adoptTokenFromUrl();

function remoteToken() {
  if (incomingToken) return incomingToken;
  try { return localStorage.getItem(TOKEN_KEY) ?? ''; } catch { return ''; }
}

/// boot 用它判断要不要放行这个页面：远程模式下没令牌就是空手进来的。
export function hasRemoteToken(): boolean { return remoteToken() !== ''; }

export async function invoke<T>(cmd: string, args?: InvokeArgs): Promise<T> {
  if (isRemote) return rpc<T>(cmd, args);
  return tauriInvoke<T>(cmd, args);
}

async function rpc<T>(cmd: string, args?: InvokeArgs): Promise<T> {
  let response: Response;
  try {
    response = await fetch(`/rpc/${cmd}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${remoteToken()}` },
      body: JSON.stringify(args ?? {}),
    });
  } catch {
    // Tauri invoke 失败时抛的是字符串，调用处的 errorText 直接拿去显示，这里也保持字符串。
    throw '连不上同席，请检查手机和电脑是否在同一个网络';
  }
  if (!response.ok) {
    let detail = '';
    try { detail = (await response.text()).trim(); } catch { /* 读不出 body 就只报状态码 */ }
    throw detail || `HTTP ${response.status}`;
  }
  const body = await response.text();
  return (body ? JSON.parse(body) : undefined) as T;
}

// 整个页面共用一条 SSE 流：第一次 listen 时才建，之后所有事件都挂在它上面。
let source: EventSource | null = null;
const groups = new Map<string, { dispatch: (message: MessageEvent) => void; handlers: Set<TransportHandler<unknown>> }>();

function connect(): EventSource {
  if (source) return source;
  const stream = new EventSource(`/events?token=${encodeURIComponent(remoteToken())}`);
  source = stream;
  // SSE 断了浏览器自己会重连；只在掉线、连上各提示一次，免得手机上一直刷屏。
  let warned = false;
  stream.addEventListener('open', () => { warned = false; });
  stream.addEventListener('error', () => {
    if (warned) return;
    warned = true;
    console.warn('[同席] 实时事件流断开，正在重连；重连后接着收更新。');
  });
  return stream;
}

function subscribe<T>(event: string, handler: TransportHandler<T>): UnlistenFn {
  const stream = connect();
  let group = groups.get(event);
  if (!group) {
    const handlers = new Set<TransportHandler<unknown>>();
    const dispatch = (message: MessageEvent) => {
      let payload: unknown;
      try { payload = JSON.parse(message.data); } catch { return; } // 网关只发 JSON，坏包直接丢
      for (const target of [...handlers]) {
        try { target({ event, payload }); } catch (error) { console.error('[同席] 事件处理出错', event, error); }
      }
    };
    group = { dispatch, handlers };
    groups.set(event, group);
    stream.addEventListener(event, dispatch as EventListener);
  }
  group.handlers.add(handler as TransportHandler<unknown>);
  return () => {
    const current = groups.get(event);
    if (!current) return;
    current.handlers.delete(handler as TransportHandler<unknown>);
    if (current.handlers.size) return;
    // 流本身留着不关：重连一次要等一两秒，页面活着就接着用同一条。
    source?.removeEventListener(event, current.dispatch as EventListener);
    groups.delete(event);
  };
}

export async function listen<T>(event: string, handler: TransportHandler<T>): Promise<UnlistenFn> {
  if (isRemote) return subscribe(event, handler);
  return tauriListen<T>(event, handler);
}

export async function emit(event: string, payload?: unknown): Promise<void> {
  // 远程没有第二个窗口，emit 只用于桌面 HUD 窗口之间同步，直接当空操作。
  if (isRemote) return;
  await tauriEmit(event, payload);
}
