import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { createServer as createHttpServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import net from 'node:net';

const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
if (!existsSync(bin)) throw new Error(`ios binary not found: ${bin}`);

const testHome = mkdtempSync(join(tmpdir(), 'rust-ios-ui-stream-'));
const serverPort = await freePort();
const common = ['--server-home', testHome, '--server-addr', `127.0.0.1:${serverPort}`];
const outputPath = join(testHome, 'capture.mjpeg');
const source = createHttpServer((request, response) => {
  if (request.url === '/status') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end('{"value":{"ready":true}}');
    return;
  }
  if (request.url !== '/mjpeg?fps=30&quality=80') {
    response.writeHead(404);
    response.end();
    return;
  }
  response.writeHead(200, {
    'content-type': 'multipart/x-mixed-replace; boundary=frame',
    'cache-control': 'no-cache',
  });
  const timer = setInterval(() => {
    response.write('--frame\r\nContent-Type: image/jpeg\r\n\r\nUI_STREAM_FRAME\r\n');
  }, 20);
  request.once('close', () => clearInterval(timer));
});
await new Promise((resolveListen, reject) => {
  source.once('error', reject);
  source.listen(0, '127.0.0.1', resolveListen);
});
const sourcePort = source.address().port;
const sourceUrl = `http://127.0.0.1:${sourcePort}`;

function invoke(args, timeoutMs = 15000) {
  return new Promise((resolveInvoke, reject) => {
    const child = spawn(bin, args, {
      env: { ...process.env, RUST_IOS_BACKEND: 'mock' },
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
      throw new Error(`UI stream failed: ${JSON.stringify(task)}`);
    }
    await new Promise(resolveWait => setTimeout(resolveWait, 100));
  }
  throw new Error(`UI stream did not reach expected state: ${JSON.stringify(task)}`);
}

try {
  const started = await expectJson([
    'ui', 'stream', 'mjpeg',
    '--driver=wda', `--wda-url=${sourceUrl}`,
    '--fps=30', '--quality=80', `--output=${outputPath}`,
    ...common,
  ]);
  const taskId = started.value.task.id;
  const running = await waitTask(
    taskId,
    task => task.runtime_state === 'running' && task.runtime.bytesWritten > 0,
  );
  if (running.kind !== 'ui_stream' || running.runtime.resolvedDriver !== 'wda') {
    throw new Error(`unexpected UI stream runtime: ${JSON.stringify(running)}`);
  }
  const firstBytes = running.runtime.bytesWritten;
  const output = (await expectJson(['task', 'output', taskId, '--limit=65536', ...common])).value;
  const decoded = Buffer.from(output.bytes_base64, 'base64').toString('binary');
  if (!decoded.includes('UI_STREAM_FRAME')) {
    throw new Error('task output did not contain the MJPEG marker');
  }
  if (!output.content_type.startsWith('multipart/x-mixed-replace')) {
    throw new Error(`unexpected content type: ${output.content_type}`);
  }

  await expectJson(['kill-server', ...common]);
  const restored = await waitTask(
    taskId,
    task => task.generation >= 2 && task.runtime_state === 'running' && task.runtime.bytesWritten > firstBytes,
  );
  console.log(
    `UI_STREAM_RESTORED task=${taskId} generation=${restored.generation} bytes=${restored.runtime.bytesWritten}`,
  );

  await expectJson(['task', 'stop', taskId, ...common]);
  const tasks = (await expectJson(['task', 'list', ...common])).value.tasks;
  if (tasks.length !== 0) throw new Error(`UI stream task leaked: ${JSON.stringify(tasks)}`);
  await expectJson(['kill-server', ...common]);
  console.log('UI_STREAM_PROCESS_SMOKE_OK');
} finally {
  if (existsSync(join(testHome, 'server.json'))) {
    await invoke(['kill-server', ...common]).catch(() => {});
  }
  await new Promise(resolveClose => source.close(resolveClose));
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
