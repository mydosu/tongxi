import { invoke, isRemote } from './transport';

/// 目录输入框旁的「选择…」按钮，点了弹系统的选文件夹窗口，选中的路径填回同一行的输入框（仍可手动改）。
/// 手机远程打开时看不到电脑的资源管理器，不给按钮。
export const folderButton = (title: string) => isRemote ? '' : `<button type="button" class="secondary folder-pick" data-pick-folder="${title}">选择…</button>`;

document.addEventListener('click', async event => {
  const button = (event.target as Element | null)?.closest<HTMLButtonElement>('[data-pick-folder]');
  const input = button?.parentElement?.querySelector<HTMLInputElement>('input');
  if (!button || !input) return;
  event.preventDefault(); // 按钮在 <label> 里时别把焦点抢回输入框
  button.disabled = true;
  try {
    const path = await invoke<string | null>('pick_folder', { title: button.dataset.pickFolder, start: input.value.trim() || null });
    if (path) { input.value = path; input.dispatchEvent(new Event('input', { bubbles: true })); }
  } catch (error) {
    input.setCustomValidity(String(error)); input.reportValidity(); input.setCustomValidity('');
  } finally { button.disabled = false; }
});
