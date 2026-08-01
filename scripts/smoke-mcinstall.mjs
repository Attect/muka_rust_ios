import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import net from 'node:net';

const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const port = server.address().port;
      server.close(error => error ? reject(error) : resolvePort(port));
    });
  });
}

function run(args, common, env, cwd = undefined) {
  const output = execFileSync(bin, [...args, ...common], {
    cwd,
    env,
    encoding: 'utf8',
    timeout: 20000,
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
  return output ? JSON.parse(output) : null;
}

function safeRemove(path, prefix) {
  const resolved = resolve(path);
  const temp = resolve(tmpdir());
  if (!resolved.startsWith(temp) || !resolved.split(/[\\/]/).at(-1).startsWith(prefix)) {
    throw new Error('refusing to remove unexpected path ' + resolved);
  }
  rmSync(resolved, { recursive: true, force: true });
}

const mockRoot = mkdtempSync(join(tmpdir(), 'rust-ios-mcinstall-smoke-'));
const mockPort = await freePort();
const mockCommon = ['--server-home', mockRoot, '--server-addr', '127.0.0.1:' + mockPort];
const recordPath = join(mockRoot, 'record.jsonl');
const mockEnv = {
  ...process.env,
  RUST_IOS_BACKEND: 'mock',
  RUST_IOS_MOCK_DEVICES: 'mock-phone',
  RUST_IOS_RECORD_FILE: recordPath,
};

try {
  const skips = run(['prepare', 'printskip'], mockCommon, mockEnv);
  if (!Array.isArray(skips) || skips.length < 60 || !skips.includes('WiFi')) {
    throw new Error('prepare printskip returned an incomplete list');
  }
  const cloud = run(['prepare', 'cloudconfig', '--udid', 'mock-phone'], mockCommon, mockEnv);
  if (cloud.IsSupervised !== false) throw new Error('unexpected mock cloud config');

  const wifi = run([
    'wifi', '--ssid', 'Lab Network', '--password', 'wifi-secret', '--enc-type', 'WPA2',
    '--udid', 'mock-phone'
  ], mockCommon, mockEnv);
  if (wifi.status !== 'installed' || wifi.ssid !== 'Lab Network') {
    throw new Error('wifi install shape mismatch');
  }
  const wifiRemove = run([
    'wifi', '--ssid', 'Lab Network', '--remove', '--udid', 'mock-phone'
  ], mockCommon, mockEnv);
  if (wifiRemove.status !== 'removed') throw new Error('wifi remove failed');

  const profile = run([
    'profile', 'add', 'profile.mobileconfig', '--p12file', 'identity.p12',
    '--password', 'profile-p12-secret', '--udid', 'mock-phone'
  ], mockCommon, mockEnv);
  if (profile.status !== 'installed' || profile.silent !== true) {
    throw new Error('supervised profile shape mismatch');
  }

  const proxy = run([
    'httpproxy', 'proxy.example', '8080', 'user', 'proxy-secret',
    '--p12file', 'identity.p12', '--password', 'proxy-p12-secret', '--udid', 'mock-phone'
  ], mockCommon, mockEnv);
  if (proxy.status !== 'installed' || proxy.authenticated !== true) {
    throw new Error('httpproxy install shape mismatch');
  }
  const proxyRemove = run(['httpproxy', 'remove', '--udid', 'mock-phone'], mockCommon, mockEnv);
  if (proxyRemove.status !== 'removed') throw new Error('httpproxy remove failed');

  const prepared = run([
    'prepare', '--skip', 'WiFi', '--locale', 'en_US', '--lang', 'en', '--udid', 'mock-phone'
  ], mockCommon, mockEnv);
  if (prepared.status !== 'prepared') throw new Error('prepare mock failed');

  run(['kill-server'], mockCommon, mockEnv);
  const recording = readFileSync(recordPath, 'utf8');
  for (const secret of ['wifi-secret', 'profile-p12-secret', 'proxy-secret', 'proxy-p12-secret']) {
    if (recording.includes(secret)) throw new Error('recording leaked ' + secret);
  }
  if (!recording.includes('<redacted>')) throw new Error('recording has no redaction marker');
  console.log('MCINSTALL_MOCK_OK commands=8 redaction=ok');
} finally {
  if (existsSync(join(mockRoot, 'server.json'))) {
    try { run(['kill-server'], mockCommon, mockEnv); } catch {}
  }
  safeRemove(mockRoot, 'rust-ios-mcinstall-smoke-');
}

const hostRoot = mkdtempSync(join(tmpdir(), 'rust-ios-host-tools-smoke-'));
const hostHome = join(hostRoot, 'server-home');
const outputDir = join(hostRoot, 'output');
mkdirSync(outputDir);
const hostPort = await freePort();
const hostCommon = ['--server-home', hostHome, '--server-addr', '127.0.0.1:' + hostPort];
const hostEnv = {
  ...process.env,
  RUST_IOS_BACKEND: 'idevice',
  USBMUXD_SOCKET_ADDRESS: '127.0.0.1:1',
};

try {
  const result = run(
    ['prepare', 'create-cert', '--p12password', 'host-tool-secret'],
    hostCommon,
    hostEnv,
    outputDir,
  );
  if (result.status !== 'created' || result.pkcs12PasswordProtected !== true) {
    throw new Error('prepare create-cert result mismatch');
  }
  const expected = [
    'supervision-cert.der',
    'supervision-cert.pem',
    'supervision-private-key.key',
    'supervision-private-key.pem',
    'supervision-csr.csr',
    'supervision-identity.p12',
  ];
  for (const name of expected) {
    const path = join(outputDir, name);
    if (!existsSync(path) || statSync(path).size === 0) throw new Error('missing host output ' + name);
  }
  if (!readFileSync(join(outputDir, 'supervision-cert.pem'), 'utf8').startsWith('-----BEGIN CERTIFICATE-----')) {
    throw new Error('certificate PEM marker missing');
  }
  run(['kill-server'], hostCommon, hostEnv, outputDir);
  console.log('SUPERVISION_HOST_OK files=' + expected.length);
  console.log('MCINSTALL_SMOKE_OK');
} finally {
  if (existsSync(join(hostHome, 'server.json'))) {
    try { run(['kill-server'], hostCommon, hostEnv, outputDir); } catch {}
  }
  safeRemove(hostRoot, 'rust-ios-host-tools-smoke-');
}
