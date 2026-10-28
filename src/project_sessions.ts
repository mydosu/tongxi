import { invoke } from './transport';
import type { NativeSession } from './types';
import { baseAgent, isBrain } from './seats';

type Escape = (value: string | number) => string;
export const PROJECT_AGENTS = ['codex-win', 'hermes-win', 'dsh-win', 'claude-win', 'opencode-win'];
export interface SessionBinding { agent_id: string; target: 'member'; native_session_id: string | null; native_cwd: string | null; busy: boolean; }
/// target=seat：新建项目时给同一成员再开的席位（agentId 是底层成员，保存时先开席位再绑会话）。
export interface BindingChoice { agentId: string; target: 'member' | 'seat'; nativeSessionId: string | null; nativeCwd: string | null; }
interface Row { agent: string; target: 'member' | 'seat'; current: string | null; busy: boolean; key?: string; }
export interface PickerHost { escape: Escape; name: (id: string) => string; avatar: (id: string) => string; connect: (id: string) => Promise<void>; }

const roleText = (row: Row) => isBrain(row.agent) ? '主脑 · 接收你的需求，向成员分配任务' : row.target === 'seat' ? '同一成员的另一个会话' : '项目成员';
const short = (id: string) => id.length > 14 ? `${id.slice(0, 8)}…${id.slice(-4)}` : id;
const when = (value?: string | null) => value ? new Date(value).toLocaleString('zh-CN', { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }) : '时间未知';

/// 给每位成员（含主脑、额外席位）挑一个原生会话：第一项永远是「新建会话」，其余是该成员自己已有的会话。
/// 同一成员的会话列表只读一次，各行共用；席位读的是底层成员的会话。
/// `allowSeats` 时成员行带「＋」，可以给同一成员再开席位（最多 3 个）。
/// `compact` 只画下拉框（嵌进成员卡片用）；多个选择器传同一个 `cache`，同一成员的会话只读一次。
export const sessionCache = () => ({ lists: new Map<string, Promise<NativeSession[]>>(), cwd: new Map<string, string>() });
export function mountSessionPicker(container: HTMLElement, initial: Row[], host: PickerHost, allowSeats = false, compact = false, cache = sessionCache()) {
  const { escape } = host;
  let serial = 0;
  const keyed = (list: Row[]) => list.map(row => ({ ...row, key: row.key || `${row.target}|${row.agent}|${serial++}` }));
  let rows = keyed(initial);
  const values = new Map<string, string>();
  const { lists, cwd } = cache;
  const load = (agent: string) => {
    const base = baseAgent(agent);
    if (!lists.has(base)) lists.set(base, invoke<NativeSession[]>('native_sessions', { agentId: base, conversationId: null }).then(list => { list.forEach(item => { if (item.cwd) cwd.set(`${base}|${item.id}`, item.cwd); }); return list; }).catch(error => { lists.delete(base); throw error; }));
    return lists.get(base)!;
  };
  const tail = (row: Row) => allowSeats && row.target === 'member' && !isBrain(row.agent)
    ? '<button type="button" class="seat-plus" data-seat-plus title="同一成员再开一个平行的会话" aria-label="再开一个会话">＋</button>'
    : row.target === 'seat' ? '<button type="button" class="seat-plus" data-seat-drop title="去掉这个会话" aria-label="去掉这个会话">×</button>' : '';
  const rowHtml = (row: Row) => `<div class="session-bind-row${row.busy ? ' busy' : ''}" data-bind-key="${escape(row.key!)}" data-bind-agent="${escape(row.agent)}" data-bind-target="${row.target}">${compact ? '' : `${host.avatar(row.agent)}<div class="session-bind-who"><strong>${escape(host.name(row.agent))}</strong><small>${roleText(row)}${row.busy ? ' · 正在执行，结束后可切换' : ''}</small></div>`}<div class="session-bind-choice"><select data-bind-session aria-label="${escape(host.name(row.agent))}${roleText(row)}的会话" ${row.busy ? 'disabled' : ''}><option value="">＋ 新建会话（不绑定）</option>${row.current ? `<option value="${escape(row.current)}" selected>当前绑定 · ${escape(short(row.current))}</option>` : ''}</select><small class="session-bind-note">正在读取已有会话…</small></div>${tail(row)}</div>`;
  const fill = async (element: HTMLElement) => {
    const row = rows.find(item => item.key === element.dataset.bindKey)!;
    const select = element.querySelector<HTMLSelectElement>('[data-bind-session]')!, note = element.querySelector<HTMLElement>('.session-bind-note')!;
    try {
      const sessions = await load(row.agent);
      if (!element.isConnected) return;
      select.innerHTML = `<option value="">＋ 新建会话（不绑定）</option>${sessions.map(session => {
        const mine = session.id === row.current;
        const taken = !mine && !!session.occupied_by;
        const title = session.title?.trim().replace(/\s+/g, ' ') || '未命名会话';
        return `<option value="${escape(session.id)}" ${taken ? 'disabled' : ''} title="${escape(`${session.id}${session.cwd ? `\n${session.cwd}` : ''}`)}">${mine ? '● ' : ''}${escape(title.slice(0, 48))} · ${escape(when(session.updated_at))}${taken ? ` · 已被「${escape(session.occupied_by!)}」占用` : ''}</option>`;
      }).join('')}${row.current && !sessions.some(session => session.id === row.current) ? `<option value="${escape(row.current)}">当前绑定 · ${escape(short(row.current))}（列表里未找到）</option>` : ''}`;
      select.value = values.get(row.key!) ?? row.current ?? '';
      note.textContent = sessions.length ? `${sessions.length} 个已有会话 · 只续接，不改动原会话设置` : '这位成员还没有可列出的会话，将新建';
    } catch (error) {
      if (!element.isConnected) return;
      note.innerHTML = `<span>${escape(String(error))}</span> <button type="button" class="text-button" data-bind-connect>连接后读取</button>`;
      note.querySelector('[data-bind-connect]')!.addEventListener('click', async () => { note.textContent = '正在连接…'; await host.connect(baseAgent(row.agent)); void fill(element); });
    }
  };
  const paint = (next: Row[]) => {
    rows = keyed(next);
    container.innerHTML = rows.length ? rows.map(rowHtml).join('') : '<p class="model-setting-note">群里还没有可执行项目任务的成员。</p>';
    container.querySelectorAll<HTMLElement>('[data-bind-key]').forEach(element => void fill(element));
  };
  container.addEventListener('change', event => {
    const element = (event.target as HTMLElement).closest<HTMLElement>('[data-bind-key]');
    if (element) values.set(element.dataset.bindKey!, (event.target as HTMLSelectElement).value);
  });
  container.addEventListener('click', event => {
    const target = event.target as HTMLElement;
    const element = target.closest<HTMLElement>('[data-bind-key]');
    if (!element) return;
    const row = rows.find(item => item.key === element.dataset.bindKey)!;
    if (target.closest('[data-seat-plus]')) {
      if (rows.filter(item => item.target === 'seat' && item.agent === row.agent).length >= 3) return;
      // 新席位插在同一成员已有席位的后面。
      let at = rows.indexOf(row) + 1;
      while (at < rows.length && rows[at].target === 'seat' && rows[at].agent === row.agent) at++;
      paint([...rows.slice(0, at), { agent: row.agent, target: 'seat', current: null, busy: false }, ...rows.slice(at)]);
    } else if (target.closest('[data-seat-drop]')) {
      values.delete(row.key!);
      paint(rows.filter(item => item !== row));
    }
  });
  paint(rows);
  return {
    /// 换一批成员/主脑行；已经加上的席位行保留（底层成员仍在时）。
    render(next: Row[]) {
      const seats = rows.filter(row => row.target === 'seat' && next.some(item => item.agent === row.agent));
      paint(next.flatMap(row => [row, ...seats.filter(seat => seat.agent === row.agent)]));
    },
    /// 只回传改动过的行；`all` 为真时回传全部（新建项目时一次性落定）。
    choices(all = false): BindingChoice[] {
      return [...container.querySelectorAll<HTMLElement>('[data-bind-key]')].flatMap(element => {
        const row = rows.find(item => item.key === element.dataset.bindKey);
        const value = element.querySelector<HTMLSelectElement>('[data-bind-session]')!.value || null;
        if (!row || row.busy || (!all && value === row.current)) return [];
        return [{ agentId: row.agent, target: row.target, nativeSessionId: value, nativeCwd: value ? cwd.get(`${baseAgent(row.agent)}|${value}`) || null : null }];
      });
    },
  };
}

export const defaultCoordinator = (members: string[], saved?: string | null) => saved && members.includes(saved) ? saved : ['claude-win', 'codex-win', 'hermes-win', 'dsh-win', 'opencode-win'].find(id => members.includes(id)) || members[0];
