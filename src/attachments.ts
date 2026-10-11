import { invoke } from '@tauri-apps/api/core';

export interface Attachment { id: string; name: string; mime: string; bytes: number; path: string; }
type Escape = (value: string | number) => string;
const opening = '```tongxi-attachments\n';
const key = (room: string) => `hub.attachments.${room}`;
const pending = new Map<string, number>();
const previews = new Map<string, Promise<string>>();
const mounts = new Map<HTMLTextAreaElement, () => void>();
const read = (room: string): Attachment[] => {
  try { const value = JSON.parse(localStorage.getItem(key(room)) || '[]'); return Array.isArray(value) ? value.filter(item => typeof item.id === 'string' && typeof item.name === 'string' && typeof item.path === 'string') : []; } catch { return []; }
};
const write = (room: string, items: Attachment[]) => { localStorage.setItem(key(room), JSON.stringify(items)); refresh(room); };
function refresh(room: string) { for (const [input, paint] of mounts) { if (!input.isConnected) mounts.delete(input); else if (input.dataset.attachmentRoom === room) paint(); } }
function preview(room: string, id: string) {
  const cacheKey = `${room}:${id}`;
  if (!previews.has(cacheKey)) previews.set(cacheKey, invoke<string>('attachment_data', { conversationId: room, attachmentId: id }).catch(error => { previews.delete(cacheKey); throw error; }));
  return previews.get(cacheKey)!;
}
export function attachmentItems(content: string): Attachment[] {
  const at = content.lastIndexOf(opening);
  if (at < 0 || !content.endsWith('```')) return [];
  try { const items = JSON.parse(content.slice(at + opening.length, -3).trim()); return Array.isArray(items) ? items.filter(item => typeof item.id === 'string' && typeof item.name === 'string') : []; } catch { return []; }
}
export function attachmentText(content: string) { return attachmentItems(content).length ? content.slice(0, content.lastIndexOf(opening)).trimEnd() : content; }
export function attachmentsBusy(room: string) { return !!pending.get(room); }
export function composeMessage(room: string, text: string) {
  const files = read(room);
  return files.length ? `${text.trim() || '请查看附件。'}\n\n${opening}${JSON.stringify(files)}\n\`\`\`` : text.trim();
}
export function submittedAttachments(room: string, content: string) {
  const sent = new Set(attachmentItems(content).map(item => item.id));
  write(room, read(room).filter(item => !sent.has(item.id)));
}
export function attachmentHtml(content: string, room: string, escape: Escape) {
  const items = attachmentItems(content);
  return items.length ? `<div class="message-attachments">${items.map(item => `<button type="button" class="attachment-file" data-attachment-download="${escape(item.id)}" data-attachment-conversation="${escape(room)}" data-attachment-name="${escape(item.name)}">${item.mime?.startsWith('image/') ? `<img data-attachment-preview="${escape(item.id)}" data-attachment-conversation="${escape(room)}" alt="${escape(item.name)}">` : '<span class="attachment-file-icon">▤</span>'}<span>${escape(item.name)}</span><small>${Math.ceil((item.bytes || 0) / 1024)} KB</small></button>`).join('')}</div>` : '';
}
export function hydrateAttachments(root: Element) {
  for (const image of root.querySelectorAll<HTMLImageElement>('img[data-attachment-preview]:not([data-loading])')) {
    image.dataset.loading = 'true';
    void preview(image.dataset.attachmentConversation!, image.dataset.attachmentPreview!).then(src => { if (image.isConnected) image.src = src; }).catch(() => { image.alt = '附件已不可用'; });
  }
}
export function mountAttachments(input: HTMLTextAreaElement, room: string, escape: Escape, error: (message: string) => void) {
  if (mounts.has(input)) return;
  input.dataset.attachmentRoom = room;
  const form = input.closest('form')!;
  const tray = document.createElement('div'); tray.className = 'attachment-tray'; tray.dataset.attachmentTray = room;
  form.insertBefore(tray, input);
  const picker = document.createElement('label'); picker.className = 'attachment-picker'; picker.title = '添加图片或附件';
  picker.innerHTML = '<span aria-hidden="true">＋</span><span class="sr-only">添加图片或附件</span><input type="file" multiple hidden data-attachment-picker>';
  const actions = form.querySelector('.composer-actions,.project-chat-composer-bottom'); (actions || form).prepend(picker);
  const paint = () => {
    const files = read(room); tray.hidden = !files.length && !pending.get(room);
    tray.innerHTML = `${files.map(item => `<div class="draft-attachment">${attachmentHtml(`${opening}${JSON.stringify([item])}\n\`\`\``, room, escape)}<button type="button" class="attachment-remove" data-attachment-remove="${escape(item.id)}" aria-label="移除 ${escape(item.name)}">×</button></div>`).join('')}${pending.get(room) ? '<small class="attachment-saving">正在保存附件…</small>' : ''}`;
    hydrateAttachments(tray);
  };
  mounts.set(input, paint);
  const receive = async (files: File[]) => {
    if (read(room).length + files.length > 20) { error('一条消息最多20个附件'); return; }
    pending.set(room, (pending.get(room) || 0) + 1); refresh(room);
    try {
      for (const file of files) {
        if (file.size > 32 * 1024 * 1024) throw new Error('单个附件最大32 MiB');
        const data = await new Promise<string>((resolve, reject) => { const reader = new FileReader(); reader.onerror = () => reject(new Error('无法读取附件')); reader.onload = () => resolve(String(reader.result).split(',')[1]); reader.readAsDataURL(file); });
        const item = await invoke<Attachment>('stage_attachment', { conversationId: room, name: file.name || '粘贴图片.png', mime: file.type || 'application/octet-stream', data });
        write(room, [...read(room), item]);
      }
    } catch (failure) { error(String(failure)); }
    finally { pending.set(room, Math.max(0, (pending.get(room) || 1) - 1)); refresh(room); }
  };
  picker.querySelector('input')!.addEventListener('change', event => { const target = event.target as HTMLInputElement; void receive([...target.files || []]); target.value = ''; });
  input.addEventListener('paste', event => {
    const files = [...event.clipboardData?.files || []];
    if (files.length) { event.preventDefault(); void receive(files); }
    else if (!event.clipboardData?.getData('text/plain')) {
      // Explorer's copied files use native CF_HDROP, absent from WebView's FileList.
      pending.set(room, (pending.get(room) || 0) + 1);
      void invoke<Attachment[]>('paste_clipboard_files', { conversationId: room }).then(items => { if (read(room).length + items.length > 20) throw new Error('一条消息最多20个附件'); if (items.length) write(room, [...read(room), ...items]); }).catch(failure => error(String(failure))).finally(() => { pending.set(room, Math.max(0, (pending.get(room) || 1) - 1)); refresh(room); });
    }
  });
  input.addEventListener('dragover', event => { if (event.dataTransfer?.types.includes('Files')) event.preventDefault(); });
  input.addEventListener('drop', event => { const files = [...event.dataTransfer?.files || []]; if (files.length) { event.preventDefault(); void receive(files); } });
  tray.addEventListener('click', event => { const button = (event.target as Element).closest<HTMLElement>('[data-attachment-remove]'); if (button) write(room, read(room).filter(item => item.id !== button.dataset.attachmentRemove)); });
  paint();
}

document.addEventListener('click', event => {
  const button = (event.target as Element).closest<HTMLElement>('[data-attachment-download]');
  if (!button) return;
  void preview(button.dataset.attachmentConversation!, button.dataset.attachmentDownload!).then(url => { const link = document.createElement('a'); link.href = url; link.download = button.dataset.attachmentName || '附件'; link.click(); }).catch(() => {});
});
window.addEventListener('storage', event => { if (event.key?.startsWith('hub.attachments.')) refresh(event.key.slice('hub.attachments.'.length)); });
