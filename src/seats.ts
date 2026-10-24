// 项目席位：同一成员在项目里多开的会话，id 形如 `codex-win-2`。运行状态、原生连接都跟底层成员走。
// 主脑也是一个席位（`claude-win-brain` 这类），项目里只有一条。
const SEAT = /^(codex-win|hermes-win|dsh-win|claude-win|opencode-win)-([2-4]|brain)$/;
export const baseAgent = (id: string) => SEAT.exec(id)?.[1] || id;
export const isBrain = (id: string) => SEAT.exec(id)?.[2] === 'brain';
export const seatSlot = (id: string) => isBrain(id) ? '' : SEAT.exec(id)?.[2] || '';
export const isSeat = (id: string) => SEAT.test(id);
