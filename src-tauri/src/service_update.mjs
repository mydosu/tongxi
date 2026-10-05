// service_update.mjs — read-only npm metadata check and isolated candidate install for the fixed packages selected by Rust
// Layout (DSH / Codex); no third-party imports, user/global npm config, auth secrets, global installs, or lifecycle scripts.
import { spawn } from 'node:child_process';
import { lstat, mkdir, open, readdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, isAbsolute, join, sep } from 'node:path';
import { createHash } from 'node:crypto';

const PACKAGE = '@deepseek-ai/dsh';
const REGISTRY_HOST = 'registry.npmjs.org';
const MAX_BODY_BYTES = 1024 * 1024;
const TIMEOUT_MS = 20000;
const SEMVER_RE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?$/;
const MAX_VERSION_LENGTH = 160;
const isValidVersion = (value) => typeof value === 'string' && value.length > 0 && value.length <= MAX_VERSION_LENGTH && SEMVER_RE.test(value);
const INTEGRITY_RE = /^sha512-([A-Za-z0-9+/]{86}==)$/;
const isRecord = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const fail = () => { throw new Error('registry_metadata_rejected'); };

function checkIntegrity(value) {
  const match = typeof value === 'string' ? INTEGRITY_RE.exec(value) : null;
  if (match === null) fail();
  const digest = Buffer.from(match[1], 'base64');
  if (digest.length !== 64 || digest.toString('base64') !== match[1]) fail();
  return value;
}

function checkTarball(value, version, pkg) {
  if (typeof value !== 'string' || value.length === 0) fail();
  let url;
  try { url = new URL(value); } catch { fail(); }
  if (url.protocol !== 'https:' || url.hostname !== REGISTRY_HOST || url.port !== '') fail();
  if (url.username !== '' || url.password !== '' || url.search !== '' || url.hash !== '') fail();
  let pathname;
  try { pathname = decodeURIComponent(url.pathname); } catch { fail(); }
  const leaf = pkg.split('/').pop();
  if (pathname !== `/${pkg}/-/${leaf}-${version}.tgz`) fail();
  return value;
}

export function validateMetadata(value, pkg = PACKAGE) {
  if (!isRecord(value) || value.name !== pkg) fail();
  const { name, version, dist } = value;
  if (!isValidVersion(version)) fail();
  if (!isRecord(dist)) fail();
  return { name, version, tarball: checkTarball(dist.tarball, version, pkg), integrity: checkIntegrity(dist.integrity) };
}

function parseSemver(value) {
  if (!isValidVersion(value)) fail();
  const match = SEMVER_RE.exec(value);
  return { major: BigInt(match[1]), minor: BigInt(match[2]), patch: BigInt(match[3]), pre: match[4] === undefined ? null : match[4].split('.') };
}

// Compares two valid SemVer strings; stable ranks above the same version with a prerelease.
export function compareVersions(a, b) {
  const left = parseSemver(a);
  const right = parseSemver(b);
  for (const key of ['major', 'minor', 'patch']) {
    if (left[key] !== right[key]) return left[key] < right[key] ? -1 : 1;
  }
  if (left.pre === null || right.pre === null) return left.pre === right.pre ? 0 : (left.pre === null ? 1 : -1);
  for (let i = 0; i < Math.max(left.pre.length, right.pre.length); i += 1) {
    const one = left.pre[i]; const other = right.pre[i];
    if (one === undefined) return -1;
    if (other === undefined) return 1;
    if (one === other) continue;
    const oneNumeric = /^\d+$/.test(one); const otherNumeric = /^\d+$/.test(other);
    if (oneNumeric && otherNumeric) return BigInt(one) < BigInt(other) ? -1 : 1;
    if (oneNumeric || otherNumeric) return oneNumeric ? -1 : 1;
    return one < other ? -1 : 1;
  }
  return 0;
}

// Returns the four validated metadata fields plus latest_is_newer, which callers must serialize outside metadata.
// `pkg` defaults to DSH. Rust passes the allowlisted Layout package for `check`; prepare consumes that validated
// package metadata and verify checks the resulting generic npm slot tree.
export async function fetchMetadata(installed = null, pkg = PACKAGE) {
  const response = await fetch(`https://registry.npmjs.org/${pkg.replace('/', '%2F')}/latest`, { redirect: 'error', signal: AbortSignal.timeout(TIMEOUT_MS), headers: { accept: 'application/json' } });
  if (!response.ok || response.body === null) fail();
  const chunks = []; let total = 0;
  const reader = response.body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > MAX_BODY_BYTES) fail();
    chunks.push(value);
  }
  let parsed;
  try { parsed = JSON.parse(Buffer.concat(chunks).toString('utf8')); } catch { fail(); }
  const metadata = validateMetadata(parsed, pkg);
  const latest_is_newer = isValidVersion(installed) ? compareVersions(metadata.version, installed) > 0 : null;
  return { ...metadata, latest_is_newer };
}

const SLOT_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const MAX_ARCHIVE_BYTES = 32 * 1024 * 1024;
const DOWNLOAD_TIMEOUT_MS = 60000;
const failDownload = () => { throw new Error('update_download_rejected'); };

async function lstatOrNull(target) {
  try {
    return await lstat(target);
  } catch (error) {
    if (isRecord(error) && error.code === 'ENOENT') return null;
    return failDownload();
  }
}

// Streams the re-validated tarball into root/slots/<slotId>/package.tgz; keeps partial output for diagnostics.
export async function downloadCandidate(root, slotId, meta) {
  try {
    if (typeof root !== 'string' || !isAbsolute(root)) failDownload();
    if (typeof slotId !== 'string' || !SLOT_ID_RE.test(slotId)) failDownload();
    if (!isRecord(meta)) failDownload();
    const { version, tarball, integrity } = validateMetadata({ name: meta.name, version: meta.version, dist: meta }, meta.name);
    const slots = join(root, 'slots');
    const slot = join(slots, slotId);
    if (!slot.startsWith(root.endsWith(sep) ? root : root + sep)) failDownload();
    const root_stats = await lstatOrNull(root);
    if (root_stats === null || root_stats.isSymbolicLink() || !root_stats.isDirectory()) failDownload();
    const slots_stats = await lstatOrNull(slots);
    if (slots_stats === null) await mkdir(slots);
    else if (slots_stats.isSymbolicLink() || !slots_stats.isDirectory()) failDownload();
    if (await lstatOrNull(slot) !== null) failDownload();
    await mkdir(slot);
    const archive = join(slot, 'package.tgz');
    const response = await fetch(tarball, { redirect: 'error', signal: AbortSignal.timeout(DOWNLOAD_TIMEOUT_MS) });
    if (!response.ok || response.body === null) failDownload();
    const hash = createHash('sha512');
    const file = await open(archive, 'wx');
    let total = 0;
    try {
      const reader = response.body.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        total += value.byteLength;
        if (total > MAX_ARCHIVE_BYTES) failDownload();
        hash.update(value);
        let offset = 0;
        while (offset < value.byteLength) {
          const { bytesWritten } = await file.write(value, offset, value.byteLength - offset);
          offset += bytesWritten;
        }
      }
    } finally {
      await file.close();
    }
    if (hash.digest('base64') !== integrity.slice('sha512-'.length)) failDownload();
    return { slot_id: slotId, slot, archive, version, integrity };
  } catch {
    failDownload();
  }
}

const INSTALL_TIMEOUT_MS = 420000;
const NPM_CLI = join(dirname(process.execPath), 'node_modules', 'npm', 'bin', 'npm-cli.js');
const NPM_REGISTRY = 'https://registry.npmjs.org/';
const SLOT_PACKAGE_JSON = { private: true, name: 'agent-hub-dsh-slot', version: '0.0.0' };
// 安装完去哪读 manifest：包名来自本次发布的元数据（check 已按包名校验过），不再写死 DSH。
const manifestSegments = (name) => ['node_modules', ...name.split('/'), 'package.json'];
// Only base Windows variables are forwarded; npm_config and any auth/credential variables are never inherited.
const NPM_ENV_KEYS = [
  'PATH', 'SystemRoot', 'WINDIR', 'TEMP', 'TMP', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA', 'ComSpec',
  'PROCESSOR_ARCHITECTURE', 'PROCESSOR_IDENTIFIER', 'NUMBER_OF_PROCESSORS', 'PATHEXT', 'SystemDrive',
  'ProgramData', 'ProgramFiles', 'ProgramFiles(x86)', 'ProgramW6432', 'CommonProgramFiles', 'ALLUSERSPROFILE', 'OS',
];
const failPrepare = () => { throw new Error('install_prepare_failed'); };

async function lstatPrepare(target) {
  try {
    return await lstat(target);
  } catch (error) {
    if (isRecord(error) && error.code === 'ENOENT') return null;
    return failPrepare();
  }
}

function npmEnv(cache, userconfig, globalconfig) {
  const base = new Map();
  for (const [key, value] of Object.entries(process.env)) base.set(key.toLowerCase(), value);
  const env = { npm_config_cache: cache, npm_config_userconfig: userconfig, npm_config_globalconfig: globalconfig };
  const seen = new Set();
  for (const key of NPM_ENV_KEYS) {
    const lower = key.toLowerCase();
    if (seen.has(lower)) continue;
    const value = base.get(lower);
    if (typeof value === 'string') { seen.add(lower); env[key] = value; }
  }
  return env;
}

function runNpmInstall(slot, archive, env) {
  return new Promise((resolve, reject) => {
    let child;
    try {
      child = spawn(process.execPath, [NPM_CLI, 'install', '--prefix', slot, '--ignore-scripts', '--no-audit', '--no-fund', '--save-exact', '--package-lock=true', `--registry=${NPM_REGISTRY}`, archive], { cwd: slot, windowsHide: true, stdio: 'ignore', env });
    } catch { reject(new Error('install_prepare_failed')); return; }
    let timer = null;
    const failOnce = () => { if (timer !== null) clearTimeout(timer); reject(new Error('install_prepare_failed')); };
    timer = setTimeout(() => { try { child.kill(); } catch { /* process already gone */ } failOnce(); }, INSTALL_TIMEOUT_MS);
    child.once('error', failOnce);
    child.once('exit', (code) => { if (code !== 0) failOnce(); });
    child.once('close', () => { clearTimeout(timer); resolve(); });
  });
}

// Downloads the candidate into a fresh slot, then installs it there via npm without lifecycle scripts or global state.
// Any failure throws install_prepare_failed and leaves the slot directory in place for diagnostics.
export async function prepareCandidate(root, slotId, meta) {
  const { slot, archive, version } = await downloadCandidate(root, slotId, meta);
  const cache = join(root, 'cache');
  const cache_stats = await lstatPrepare(cache);
  if (cache_stats === null) await mkdir(cache);
  else if (cache_stats.isSymbolicLink() || !cache_stats.isDirectory()) failPrepare();
  const npm_cli_stats = await lstatPrepare(NPM_CLI);
  if (npm_cli_stats === null || (!npm_cli_stats.isFile() && !npm_cli_stats.isSymbolicLink())) failPrepare();
  const userconfig = join(slot, 'npm-user.cfg');
  const globalconfig = join(slot, 'npm-global.cfg');
  try {
    await writeFile(join(slot, 'package.json'), `${JSON.stringify(SLOT_PACKAGE_JSON)}\n`, { flag: 'wx' });
    await writeFile(userconfig, '', { flag: 'wx' });
    await writeFile(globalconfig, '', { flag: 'wx' });
  } catch { failPrepare(); }
  await runNpmInstall(slot, archive, npmEnv(cache, userconfig, globalconfig));
  const manifest = join(slot, ...manifestSegments(meta.name));
  const manifest_stats = await lstatPrepare(manifest);
  if (manifest_stats === null || manifest_stats.isSymbolicLink() || !manifest_stats.isFile() || manifest_stats.size > MAX_BODY_BYTES) failPrepare();
  let installed;
  try { installed = JSON.parse(await readFile(manifest, 'utf8')); } catch { failPrepare(); }
  if (!isRecord(installed) || installed.name !== meta.name || installed.version !== meta.version) failPrepare();
  // The slot is complete here, so seal it immediately; later activation re-verifies the same tree hash.
  const fingerprint = await fingerprintCandidate(root, slotId);
  return { slot_id: slotId, version, slot, ...fingerprint };
}

const MAX_FINGERPRINT_FILES = 100000;
const MAX_FINGERPRINT_BYTES = 2 * 1024 * 1024 * 1024;
const FINGERPRINT_MARKER = 'candidate_modified';
const failFingerprint = () => { throw new Error(FINGERPRINT_MARKER); };

async function lstatFingerprint(target) {
  try {
    return await lstat(target);
  } catch (error) {
    if (isRecord(error) && error.code === 'ENOENT') return null;
    return failFingerprint();
  }
}

// Records one entry below slot as a slot-relative forward-slash path; every symlink and stat error aborts the seal.
async function walkFingerprintTree(abs, rel, state) {
  const stats = await lstatFingerprint(abs);
  if (stats === null || stats.isSymbolicLink()) failFingerprint();
  const is_dir = stats.isDirectory();
  if (!is_dir && !stats.isFile()) failFingerprint();
  state.entries.push([rel, is_dir ? null : stats.size]);
  if (state.entries.length > MAX_FINGERPRINT_FILES) failFingerprint();
  if (!is_dir) return;
  let names;
  try { names = await readdir(abs); } catch { failFingerprint(); }
  for (const name of names) await walkFingerprintTree(join(abs, name), `${rel}/${name}`, state);
}

// Seals only slot/node_modules, slot/package.json and slot/package-lock.json into one sha256; read-only and mtime-free.
// Directory entries are sealed as JSON [path, null] and files as [path, sha256hex], each followed by one newline.
export async function fingerprintCandidate(root, slotId) {
  try {
    if (typeof root !== 'string' || !isAbsolute(root)) failFingerprint();
    if (typeof slotId !== 'string' || !SLOT_ID_RE.test(slotId)) failFingerprint();
    const slots = join(root, 'slots');
    const slot = join(slots, slotId);
    if (!slot.startsWith(root.endsWith(sep) ? root : root + sep)) failFingerprint();
    const root_stats = await lstatFingerprint(root);
    if (root_stats === null || root_stats.isSymbolicLink() || !root_stats.isDirectory()) failFingerprint();
    const slots_stats = await lstatFingerprint(slots);
    if (slots_stats === null || slots_stats.isSymbolicLink() || !slots_stats.isDirectory()) failFingerprint();
    const slot_stats = await lstatFingerprint(slot);
    if (slot_stats === null || slot_stats.isSymbolicLink() || !slot_stats.isDirectory()) failFingerprint();
    const state = { entries: [] };
    await walkFingerprintTree(join(slot, 'node_modules'), 'node_modules', state);
    for (const name of ['package.json', 'package-lock.json']) {
      const stats = await lstatFingerprint(join(slot, name));
      if (stats === null) continue;
      if (stats.isSymbolicLink() || !stats.isFile()) failFingerprint();
      state.entries.push([name, stats.size]);
      if (state.entries.length > MAX_FINGERPRINT_FILES) failFingerprint();
    }
    state.entries.sort((left, right) => (left[0] < right[0] ? -1 : (left[0] > right[0] ? 1 : 0)));
    const tree = createHash('sha256');
    let files = 0; let bytes = 0;
    for (const [rel, size] of state.entries) {
      if (size === null) { tree.update(`${JSON.stringify([rel, null])}\n`); continue; }
      if (bytes + size > MAX_FINGERPRINT_BYTES) failFingerprint();
      let data;
      try { data = await readFile(join(slot, ...rel.split('/'))); } catch { failFingerprint(); }
      bytes += data.byteLength;
      files += 1;
      if (files > MAX_FINGERPRINT_FILES || bytes > MAX_FINGERPRINT_BYTES) failFingerprint();
      tree.update(`${JSON.stringify([rel, createHash('sha256').update(data).digest('hex')])}\n`);
    }
    return { tree_sha256: tree.digest('hex'), files, bytes };
  } catch {
    failFingerprint();
  }
}

async function runCli(argv) {
  const action = argv[0] ?? null;
  if (action === null) return;
  if (action === 'check') {
    // argv[1] = 已安装版本（空串视为未知），argv[2] = 包名（缺省 DSH）；两者都当位置参数传，缺一不可。
    const installed = typeof argv[1] === 'string' && argv[1].length > 0 ? argv[1] : null;
    const pkg = typeof argv[2] === 'string' && argv[2].length > 0 ? argv[2] : PACKAGE;
    const { name, version, tarball, integrity, latest_is_newer } = await fetchMetadata(installed, pkg);
    process.stdout.write(`${JSON.stringify({ ok: true, metadata: { name, version, tarball, integrity }, latest_is_newer })}\n`);
    return;
  }
  if (action === 'prepare') {
    const raw = argv[3];
    let meta = null;
    try { meta = typeof raw === 'string' ? JSON.parse(raw) : null; } catch { meta = null; }
    const result = await prepareCandidate(argv[1], argv[2], meta);
    process.stdout.write(`${JSON.stringify({ ok: true, ...result })}\n`);
    return;
  }
  if (action === 'verify') {
    const fingerprint = await fingerprintCandidate(argv[1], argv[2]);
    const expected = argv[3];
    if (expected !== undefined && expected !== fingerprint.tree_sha256) failFingerprint();
    process.stdout.write(`${JSON.stringify({ ok: true, ...fingerprint })}\n`);
    return;
  }
  throw new Error('unsupported_action');
}

runCli(process.argv.slice(1)).catch((error) => {
  const message = error instanceof Error ? error.message : '';
  const action = process.argv.slice(1)[0] ?? null;
  const failed_prepare = message !== 'unsupported_action' && action === 'prepare';
  const failed_verify = message === FINGERPRINT_MARKER && action === 'verify';
  const error_code = message === 'unsupported_action' ? 'unsupported_action' : (failed_verify ? FINGERPRINT_MARKER : (failed_prepare ? 'install_prepare_failed' : 'registry_check_failed'));
  if (failed_prepare) process.exitCode = 1;
  process.stdout.write(`${JSON.stringify({ ok: false, error_code })}\n`);
});
