#!/usr/bin/env node
// Real-module verification for src-tauri/src/service_update.mjs (metadata validation + semver).
// No network, no installs: global fetch is replaced by a counting thrower.
import assert from 'node:assert/strict';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const REPORT = resolve(ROOT, 'artifacts/service-metadata-verification.json');

// Import the real module without letting it run its own CLI entrypoint.
const argv = process.argv;
process.argv = [process.execPath];
let validateMetadata;
let compareVersions;
try {
  ({ validateMetadata, compareVersions } = await import('../src-tauri/src/service_update.mjs'));
} finally {
  process.argv = argv;
}

let networkCalls = 0;
globalThis.fetch = () => {
  networkCalls += 1;
  throw new Error('network disabled');
};

const checks = [];
const check = (name, fn) => {
  try {
    fn();
    checks.push({ name, ok: true });
  } catch (err) {
    checks.push({ name, ok: false, error: String(err?.message ?? err).slice(0, 120) });
  }
};

const PKG = '@deepseek-ai/dsh';
const INTEGRITY = `sha512-${Buffer.alloc(64).toString('base64')}`;
const tarball = (v) => `https://registry.npmjs.org/${PKG}/-/dsh-${v}.tgz`;
const meta = (version, dist = {}) => ({
  name: PKG,
  version,
  dist: { tarball: tarball(version), integrity: INTEGRITY, ...dist },
});
const rejected = (obj) => assert.throws(() => validateMetadata(obj));

for (const version of ['1.2.3', '0.2.0-rc.2']) {
  check(`accepts valid metadata (${version})`, () => {
    const out = validateMetadata(meta(version));
    assert.deepEqual({ ...out }, { name: PKG, version, tarball: tarball(version), integrity: INTEGRITY });
  });
}
check('rejects wrong package name', () => rejected({ ...meta('1.2.3'), name: '@deepseek-ai/dsh-cli' }));
check('rejects http tarball', () => rejected(meta('1.2.3', { tarball: tarball('1.2.3').replace('https://', 'http://') })));
check('rejects non-registry host', () => rejected(meta('1.2.3', { tarball: 'https://evil.example.com/@deepseek-ai/dsh/-/dsh-1.2.3.tgz' })));
check('rejects credentials in url', () => rejected(meta('1.2.3', { tarball: tarball('1.2.3').replace('https://', 'https://user@') })));
check('rejects query string', () => rejected(meta('1.2.3', { tarball: `${tarball('1.2.3')}?cache=1` })));
check('rejects url fragment', () => rejected(meta('1.2.3', { tarball: `${tarball('1.2.3')}#hash` })));
check('rejects wrong version path', () => rejected(meta('1.2.3', { tarball: tarball('1.2.4') })));
check('rejects invalid base64 integrity', () => rejected(meta('1.2.3', { integrity: 'sha512-not*base64!' })));
check('rejects 63-byte integrity digest', () => rejected(meta('1.2.3', { integrity: `sha512-${Buffer.alloc(63).toString('base64')}` })));
check('rejects leading-zero version', () => rejected(meta('01.2.3')));
check('rejects traversal version', () => rejected(meta('1.2.3/../evil')));

const compared = (name, a, b, expected) => check(name, () => assert.equal(compareVersions(a, b), expected));
compared('stable ranks above prerelease', '1.2.3', '1.2.3-rc.2', 1);
compared('prerelease ranks below stable', '1.2.3-rc.2', '1.2.3', -1);
compared('numeric prerelease compares numerically', '1.2.3-rc.10', '1.2.3-rc.2', 1);
compared('major ordering', '2.0.0', '1.99.99', 1);
compared('minor ordering', '1.3.0', '1.2.99', 1);
compared('patch ordering', '1.2.4', '1.2.3', 1);
compared('numeric prerelease below alphanumeric', '1.0.0-1', '1.0.0-alpha', -1);
compared('shorter alphanumeric prefix below longer', '1.0.0-alpha', '1.0.0-alpha.1', -1);
compared('equal versions compare 0', '1.2.3', '1.2.3', 0);
compared('adjacent big integers above 2^53', '1.0.0-9007199254740993', '1.0.0-9007199254740992', 1);
compared('adjacent big integers above 2^53 (reverse)', '1.0.0-9007199254740992', '1.0.0-9007199254740993', -1);
check('invalid version input throws', () => assert.throws(() => compareVersions('1.2', '1.2.3')));

const passed = checks.filter((c) => c.ok).length;
const failed = checks.length - passed;
mkdirSync(dirname(REPORT), { recursive: true });
writeFileSync(
  REPORT,
  `${JSON.stringify({ ok: failed === 0 && networkCalls === 0, network_calls: networkCalls, passed, total: checks.length, checks }, null, 2)}\n`,
);
process.stdout.write(`${JSON.stringify({ passed, network_calls: networkCalls })}\n`);
if (failed > 0 || networkCalls > 0) process.exit(1);
