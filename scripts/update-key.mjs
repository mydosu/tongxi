// 发版签名密钥：私钥只放在本机（默认 ~/.tongxi/update-signing-key.pem，可用 TONGXI_SIGNING_KEY 指定），
// 公钥写进 src-tauri/src/updater.rs 的 PUBLIC_KEY。私钥丢了以后的版本就没法自动更新，务必另外备份。
import { generateKeyPairSync, createPrivateKey, createPublicKey, sign } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';

export const keyPath = process.env.TONGXI_SIGNING_KEY || join(homedir(), '.tongxi', 'update-signing-key.pem');

/// 签的是这串文字，不是整份 JSON：格式化变了也不影响。updater.rs 拼的必须一模一样。
export const signedText = ({ version, bytes, sha256 }) => `tongxi-update\n${version}\n${bytes}\n${sha256}`;

export function signManifest(manifest) {
  if (!existsSync(keyPath)) throw new Error(`找不到发版签名私钥：${keyPath}（node scripts/update-key.mjs 生成，或设置 TONGXI_SIGNING_KEY）`);
  const key = createPrivateKey(readFileSync(keyPath));
  return sign(null, Buffer.from(signedText(manifest)), key).toString('base64');
}

const publicKey = pem => createPublicKey(pem).export({ format: 'der', type: 'spki' }).subarray(-32).toString('base64');

if (process.argv[1]?.endsWith('update-key.mjs')) {
  if (existsSync(keyPath)) {
    console.log(`私钥已存在：${keyPath}\n公钥：${publicKey(readFileSync(keyPath))}`);
  } else {
    const { privateKey } = generateKeyPairSync('ed25519');
    mkdirSync(dirname(keyPath), { recursive: true });
    writeFileSync(keyPath, privateKey.export({ format: 'pem', type: 'pkcs8' }), { mode: 0o600 });
    console.log(`已生成私钥：${keyPath}（请另外备份）\n公钥：${publicKey(privateKey)}`);
  }
}
