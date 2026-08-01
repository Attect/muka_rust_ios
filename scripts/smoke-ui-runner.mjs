import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { request } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import net from 'node:net';

const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
if (!existsSync(bin)) throw new Error(`ios binary not found: ${bin}`);

const testHome = mkdtempSync(join(tmpdir(), 'rust-ios-ui-runner-'));
const serverPort = await freePort();
const wdaPort = await freePort();
const deviceKitPort = await freePort();
const logPath = join(testHome, 'wda-runner.jsonl');
const common = ['--server-home', testHome, '--server-addr', `127.0.0.1:${serverPort}`];
const env = {
  ...process.env,
  RUST_IOS_BACKEND: 'mock',
  RUST_IOS_MOCK_DEVICES: 'mock-phone',
};

function invoke(args, timeoutMs = 20000) {
  return new Promise((resolveInvoke, reject) => {
    const child = spawn(bin, args, {
      env,
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');
    child.stdout.on('data', chunk => { stdout += chunk; });
    child.stderr.on('data', chunk => { stderr += chunk; });
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error(`command timed out: ${args.join(' ')}`));
    }, timeoutMs);
    child.once('error', error => {
      clearTimeout(timer);
      reject(error);
    });
    child.once('exit', status => {
      clearTimeout(timer);
      resolveInvoke({ status, stdout, stderr });
    });
  });
}

async function expectJson(args) {
  const result = await invoke(args);
  if (result.status !== 0) {
    throw new Error(`command failed (${result.status}): ${args.join(' ')}\n${result.stderr}`);
  }
  return JSON.parse(result.stdout);
}

async function waitTask(taskId, predicate) {
  const deadline = Date.now() + 15000;
  let task;
  while (Date.now() < deadline) {
    task = (await expectJson(['task', 'show', taskId, ...common])).value.task;
    if (predicate(task)) return task;
    if (task.runtime_state === 'failed') {
      throw new Error(`UI runner failed: ${JSON.stringify(task)}`);
    }
    await new Promise(resolveWait => setTimeout(resolveWait, 100));
  }
  throw new Error(`UI runner did not reach expected state: ${JSON.stringify(task)}`);
}

function httpGet(port, path) {
  return new Promise((resolveGet, reject) => {
    const req = request({ host: '127.0.0.1', port, path, method: 'GET' }, response => {
      const chunks = [];
      response.on('data', chunk => chunks.push(chunk));
      response.once('end', () => resolveGet({
        status: response.statusCode,
        body: Buffer.concat(chunks).toString('utf8'),
      }));
    });
    req.once('error', reject);
    req.end();
  });
}

function echo(port, payload) {
  return new Promise((resolveEcho, reject) => {
    const socket = net.createConnection({ host: '127.0.0.1', port });
    const chunks = [];
    socket.once('error', reject);
    socket.once('connect', () => socket.write(payload));
    socket.on('data', chunk => {
      chunks.push(chunk);
      if (Buffer.concat(chunks).length >= payload.length) {
        socket.end();
      }
    });
    socket.once('close', () => resolveEcho(Buffer.concat(chunks)));
  });
}

try {
  const started = await expectJson([
    'ui', 'run', 'wda',
    `--host-port=${wdaPort}`,
    `--log-output=${logPath}`,
    ...common,
  ]);
  const taskId = started.value.task.id;
  const running = await waitTask(
    taskId,
    task => task.runtime_state === 'running' && task.runtime.eventsWritten > 0,
  );
  if (running.kind !== 'ui_runner' || running.runtime.target !== 'wda') {
    throw new Error(`unexpected WDA runner runtime: ${JSON.stringify(running)}`);
  }
  const health = await httpGet(wdaPort, '/status');
  if (health.status !== 200 || JSON.parse(health.body).ready !== true) {
    throw new Error(`unexpected WDA health response: ${JSON.stringify(health)}`);
  }
  const payload = Buffer.from('ui-runner-process-echo');
  const echoed = await echo(wdaPort, payload);
  if (!echoed.equals(payload)) throw new Error(`UI runner echo mismatch: ${echoed}`);
  const output = (await expectJson([
    'task', 'output', taskId, '--limit=65536', ...common,
  ])).value;
  const log = Buffer.from(output.bytes_base64, 'base64').toString('utf8');
  if (!log.includes('mock UI runner started') || output.content_type !== 'application/x-ndjson') {
    throw new Error(`unexpected UI runner output: ${JSON.stringify(output)}`);
  }

  await expectJson(['kill-server', ...common]);
  const restored = await waitTask(
    taskId,
    task => task.generation >= 2 && task.runtime_state === 'running',
  );
  if (restored.runtime.hostPort !== wdaPort) {
    throw new Error(`restored endpoint changed: ${JSON.stringify(restored.runtime)}`);
  }
  await expectJson(['task', 'stop', taskId, ...common]);
  await assertPortAvailable(wdaPort);

  const deviceKit = await expectJson([
    'ui', 'run', 'devicekit',
    `--host-port=${deviceKitPort}`,
    ...common,
  ]);
  const deviceKitId = deviceKit.value.task.id;
  const deviceKitRunning = await waitTask(
    deviceKitId,
    task => task.runtime_state === 'running',
  );
  if (deviceKitRunning.runtime.devicePort !== 12004 || deviceKitRunning.runtime.healthPath !== '/health') {
    throw new Error(`unexpected DeviceKit runtime: ${JSON.stringify(deviceKitRunning.runtime)}`);
  }
  const deviceKitHealth = await httpGet(deviceKitPort, '/health');
  if (deviceKitHealth.status !== 200) {
    throw new Error(`unexpected DeviceKit health response: ${JSON.stringify(deviceKitHealth)}`);
  }
  await expectJson(['task', 'stop', deviceKitId, ...common]);
  const tasks = (await expectJson(['task', 'list', ...common])).value.tasks;
  if (tasks.length !== 0) throw new Error(`UI runner task leaked: ${JSON.stringify(tasks)}`);
  await assertPortAvailable(deviceKitPort);
  await expectJson(['kill-server', ...common]);
  console.log(`UI_RUNNER_RESTORED task=${taskId} generation=${restored.generation} port=${wdaPort}`);
  console.log('UI_RUNNER_PROCESS_SMOKE_OK');
} finally {
  if (existsSync(join(testHome, 'server.json'))) {
    await invoke(['kill-server', ...common]).catch(() => {});
  }
  rmSync(testHome, { recursive: true, force: true });
}

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const selected = server.address().port;
      server.close(error => error ? reject(error) : resolvePort(selected));
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
