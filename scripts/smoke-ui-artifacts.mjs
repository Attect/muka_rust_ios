import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import net from 'node:net';

const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
const root = mkdtempSync(join(tmpdir(), 'rust-ios-ui-artifacts-'));
const home = join(root, 'home');
const output = join(root, 'downloads');
const payload = Buffer.from('mock-devicekit-ipa-artifact');
const serverPort = await freePort();
const artifactServer = createServer((request, response) => {
  if (request.url !== '/devicekit-ios-runner.ipa') {
    response.writeHead(404).end();
    return;
  }
  response.writeHead(200, {
    'content-type': 'application/octet-stream',
    'content-length': payload.length,
  });
  response.end(payload);
});
await new Promise((accept, reject) => {
  artifactServer.once('error', reject);
  artifactServer.listen(0, '127.0.0.1', accept);
});
const artifactPort = artifactServer.address().port;
const common = ['--server-home', home, '--server-addr', `127.0.0.1:${serverPort}`];
const env = {
  ...process.env,
  RUST_IOS_BACKEND: 'mock',
  RUST_IOS_MOCK_DEVICES: 'mock-phone',
  RUST_IOS_DEVICEKIT_ARTIFACT_URL: `http://127.0.0.1:${artifactPort}/devicekit-ios-runner.ipa`,
};

function invoke(args) {
  return new Promise((accept, reject) => {
    const child = spawn(bin, [...args, ...common], { env, windowsHide: true });
    let stdout = '';
    let stderr = '';
    const timer = setTimeout(() => child.kill(), 30000);
    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');
    child.stdout.on('data', chunk => { stdout += chunk; });
    child.stderr.on('data', chunk => { stderr += chunk; });
    child.once('error', error => {
      clearTimeout(timer);
      reject(error);
    });
    child.once('exit', (status, signal) => {
      clearTimeout(timer);
      accept({ status, signal, stdout, stderr });
    });
  });
}

function freePort() {
  return new Promise((accept, reject) => {
    const listener = net.createServer();
    listener.once('error', reject);
    listener.listen(0, '127.0.0.1', () => {
      const port = listener.address().port;
      listener.close(error => error ? reject(error) : accept(port));
    });
  });
}

try {
  const download = await invoke(['ui', 'download', 'devicekit', '--output', output]);
  if (download.status !== 0) {
    throw new Error(`ui download failed: ${download.stderr}`);
  }
  const value = JSON.parse(download.stdout);
  if (value.downloaded?.length !== 1 || value.downloaded[0].target !== 'devicekit') {
    throw new Error(`unexpected download result: ${download.stdout}`);
  }
  const artifact = value.downloaded[0].artifactPath;
  if (!existsSync(artifact) || !readFileSync(artifact).equals(payload)) {
    throw new Error(`downloaded artifact mismatch: ${artifact}`);
  }

  const install = await invoke([
    'ui', 'install', 'devicekit',
    `--path=${artifact}`,
    '--p12file=mock.p12',
    '--profile=mock.mobileprovision',
    '--bundleid=com.example.MockDeviceKit',
  ]);
  if (install.status !== 0) {
    throw new Error(`mock ui install failed: ${install.stderr}`);
  }
  const installed = JSON.parse(install.stdout);
  if (installed.status !== 'complete' || installed.target !== 'devicekit' || installed.mock !== true) {
    throw new Error(`unexpected install result: ${install.stdout}`);
  }
  const tasks = await invoke(['task', 'list']);
  const taskPayload = JSON.parse(tasks.stdout);
  const taskList = Array.isArray(taskPayload)
    ? taskPayload
    : taskPayload.value?.tasks ?? taskPayload.tasks;
  if (tasks.status !== 0 || !Array.isArray(taskList) || taskList.length !== 0) {
    throw new Error(`ui artifact workflow leaked a task: ${tasks.stdout} ${tasks.stderr}`);
  }
  console.log(`UI_ARTIFACT_BYTES=${payload.length}`);
  console.log('UI_ARTIFACT_PROCESS_SMOKE_OK');
} finally {
  await invoke(['kill-server']).catch(() => {});
  await new Promise(resolveClose => artifactServer.close(resolveClose));
  rmSync(root, { recursive: true, force: true });
}
