import { execFileSync } from 'node:child_process';
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
if (!existsSync(bin)) throw new Error(`ios binary not found: ${bin}`);

const root = mkdtempSync(join(tmpdir(), 'rust-ios-fileservice-'));
const home = join(root, 'home');
const serverPort = await freePort();
const common = ['--server-home', home, '--server-addr', `127.0.0.1:${serverPort}`];
const env = {
  ...process.env,
  RUST_IOS_BACKEND: 'mock',
  RUST_IOS_MOCK_DEVICES: 'mock-phone',
};

function run(args, expectedStatus = 0) {
  try {
    const stdout = execFileSync(bin, [...args, ...common], {
      cwd: root,
      env,
      encoding: 'utf8',
      timeout: 15000,
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
    if (expectedStatus !== 0) throw new Error(`command unexpectedly succeeded: ${args.join(' ')}`);
    return stdout ? JSON.parse(stdout) : null;
  } catch (error) {
    if (error.status !== expectedStatus) throw error;
    return JSON.parse(String(error.stderr).trim());
  }
}

function delay(ms) {
  return new Promise(resolveDelay => setTimeout(resolveDelay, ms));
}

async function waitTask(id, state) {
  const deadline = Date.now() + 8000;
  let task;
  while (Date.now() < deadline) {
    task = run(['task', 'show', id]).value.task;
    if (task.runtime_state === state) return task;
    await delay(75);
  }
  throw new Error(`task ${id} did not reach ${state}: ${JSON.stringify(task)}`);
}

try {
  const missingTunnel = run(['file', 'ls', '--temp', '--udid', 'mock-phone'], 3);
  if (missingTunnel.error?.code !== 'device_unavailable'
      || !missingTunnel.error?.message.includes('tunnel start --userspace')) {
    throw new Error(`missing tunnel was not explicit: ${JSON.stringify(missingTunnel)}`);
  }

  const legacy = run(['file', 'ls', '--app=com.example.Legacy', '--udid', 'mock-phone']);
  if (legacy.domain !== 'app_data' || legacy.serviceTransport !== 'mock') {
    throw new Error(`legacy app fallback changed: ${JSON.stringify(legacy)}`);
  }

  let task = run([
    'tunnel', 'start', '--userspace', '--userspace-port=0', '--udid', 'mock-phone',
  ]).value.task;
  task = await waitTask(task.id, 'running');

  const domains = [
    [['--app=com.example.App'], 'app_data', 'com.example.App'],
    [['--app-group=group.example.shared'], 'app_group', 'group.example.shared'],
    [['--crash'], 'system_crash_logs', null],
    [['--temp'], 'temporary', null],
  ];
  for (const [flags, domain, identifier] of domains) {
    const result = run(['file', 'ls', ...flags, '--udid', 'mock-phone']);
    if (result.domain !== domain
        || result.identifier !== identifier
        || result.serviceTransport !== 'shared_rsd'
        || result.count !== 1) {
      throw new Error(`unexpected ${domain} result: ${JSON.stringify(result)}`);
    }
  }

  const ambiguous = run([
    'file', 'ls', '--app=com.example.App', '--temp', '--udid', 'mock-phone',
  ], 3);
  if (ambiguous.error?.code !== 'invalid_request') {
    throw new Error(`ambiguous domains were accepted: ${JSON.stringify(ambiguous)}`);
  }

  writeFileSync(join(root, 'small.bin'), Buffer.from('small-file'));
  writeFileSync(join(root, 'large.bin'), Buffer.alloc(2048, 0x5a));
  for (const name of ['small.bin', 'large.bin']) {
    const result = run([
      'file', 'push', '--temp', `--local=${name}`, `--remote=${name}`, '--udid', 'mock-phone',
    ]);
    if (result.domain !== 'temporary'
        || result.size !== readFileSync(join(root, name)).length
        || result.serviceTransport !== 'shared_rsd') {
      throw new Error(`unexpected push result for ${name}: ${JSON.stringify(result)}`);
    }
  }

  const pulled = run([
    'file', 'pull', '--crash', '--remote=panic.ips', '--local=downloaded.ips',
    '--udid', 'mock-phone',
  ]);
  const downloaded = readFileSync(join(root, 'downloaded.ips'), 'utf8');
  if (pulled.domain !== 'system_crash_logs'
      || pulled.size !== Buffer.byteLength(downloaded)
      || !downloaded.includes('panic.ips')) {
    throw new Error(`unexpected pull result: ${JSON.stringify(pulled)}`);
  }
  const leakedParts = readdirSync(root).filter(name => name.includes('.rust-ios-') && name.endsWith('.part'));
  if (leakedParts.length !== 0) throw new Error(`temporary pull files leaked: ${leakedParts}`);

  run(['task', 'stop', task.id]);
  if (run(['task', 'list']).value.tasks.length !== 0) throw new Error('tunnel task leaked');
  run(['kill-server']);
  await assertPortAvailable(serverPort);
  console.log(`FILESERVICE_DOMAINS_OK count=${domains.length} task=${task.id}`);
  console.log('FILESERVICE_PROCESS_SMOKE_OK');
} finally {
  if (existsSync(join(home, 'server.json'))) {
    try { run(['kill-server']); } catch {}
  }
  rmSync(root, { recursive: true, force: true });
}

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

function assertPortAvailable(port) {
  return new Promise((resolveAvailable, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => {
      server.close(error => error ? reject(error) : resolveAvailable());
    });
  });
}
