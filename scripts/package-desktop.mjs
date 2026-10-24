import { mkdir, copyFile, readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { signManifest } from './update-key.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const destination = resolve(root, 'release', 'Agent Hub.exe');
const version = JSON.parse(await readFile(resolve(root, 'package.json'), 'utf8')).version;
const archive = resolve(root, 'release', `v${version}`);
const source = resolve(root, 'src-tauri', 'target', 'release', 'local-agent-hub.exe');
const binary = await readFile(source);
const manifest = {
  version,
  file: 'Agent Hub.exe',
  bytes: binary.length,
  sha256: createHash('sha256').update(binary).digest('hex'),
  requires: ['Windows', 'Microsoft Edge WebView2 Runtime'],
};
// 同席自更新只认这个签名，没私钥就别打包（不然这一版谁都更新不上来）。
manifest.signature = signManifest(manifest);
await mkdir(archive, { recursive: true });
const archivedExe = resolve(archive, 'Agent Hub.exe');
let archiveExists = false;
try {
  const existing = await readFile(archivedExe);
  const existingHash = createHash('sha256').update(existing).digest('hex');
  if (existingHash !== manifest.sha256) {
    const archiveManifest = JSON.parse(await readFile(resolve(archive, 'manifest.json'), 'utf8'));
    if (archiveManifest.version !== version) {
      throw new Error(`版本备份已存在且版本不同，拒绝覆盖：${archivedExe}`);
    }
    await copyFile(source, archivedExe);
  }
  archiveExists = true;
} catch (error) {
  if (error?.code !== 'ENOENT') throw error;
}
if (!archiveExists) await copyFile(source, archivedExe);
const manifestText = JSON.stringify(manifest, null, 2) + '\n';
await writeFile(resolve(archive, 'manifest.json'), manifestText);
await mkdir(dirname(destination), { recursive: true });
let rootUpdated = false;
try {
  await copyFile(source, destination);
  await writeFile(resolve(root, 'release', 'manifest.json'), manifestText);
  rootUpdated = true;
} catch (error) {
  if (!['EBUSY', 'EPERM'].includes(error?.code)) throw error;
}
const size = (binary.length / 1024 / 1024).toFixed(2);
if (rootUpdated) {
  console.log(`已发布 v${version}：${destination}，版本备份：${archive} (${size} MiB)`);
} else {
  console.log(`已生成 v${version} 备份：${archive} (${size} MiB)；主程序正被运行中的同席锁定，关闭应用后再执行 npm run postdesktop:build 更新主文件。`);
}
