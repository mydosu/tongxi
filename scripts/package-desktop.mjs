import { mkdir, copyFile, readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const destination = resolve(root, 'release', 'Agent Hub.exe');
const version = JSON.parse(await readFile(resolve(root, 'package.json'), 'utf8')).version;
const archive = resolve(root, 'release', `v${version}`);
await mkdir(dirname(destination), { recursive: true });
await copyFile(resolve(root, 'src-tauri', 'target', 'release', 'local-agent-hub.exe'), destination);
const binary = await readFile(destination);
const manifest = {
  version,
  file: 'Agent Hub.exe',
  bytes: binary.length,
  sha256: createHash('sha256').update(binary).digest('hex'),
  requires: ['Windows', 'Microsoft Edge WebView2 Runtime'],
};
await writeFile(resolve(root, 'release', 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
await mkdir(archive, { recursive: true });
const archivedExe = resolve(archive, 'Agent Hub.exe');
try {
  const existing = await readFile(archivedExe);
  const existingHash = createHash('sha256').update(existing).digest('hex');
  if (existingHash !== manifest.sha256) {
    throw new Error(`版本备份已存在且内容不同，拒绝覆盖：${archivedExe}`);
  }
} catch (error) {
  if (error?.code !== 'ENOENT') throw error;
  await copyFile(destination, archivedExe);
}
await writeFile(resolve(archive, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
console.log(`已发布 v${version}：${destination}，版本备份：${archive} (${(binary.length / 1024 / 1024).toFixed(2)} MiB)`);
