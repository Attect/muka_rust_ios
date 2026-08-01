import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { isAbsolute, join, resolve } from 'node:path';
import net from 'node:net';

const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
const rootArgument = process.argv[3];
if (!rootArgument || !isAbsolute(rootArgument)) {
  throw new Error('an unused absolute evidence directory is required as the second argument');
}
if (!existsSync(bin)) throw new Error(`ios binary not found: ${bin}`);
if (existsSync(rootArgument)) {
  throw new Error(`evidence directory already exists: ${rootArgument}`);
}

const testRoot = rootArgument;
mkdirSync(testRoot);
const serverPort = await freePort();
const uiRunnerPort = await freePort();
const resultPath = join(testRoot, 'smoke-result.json');
const xctestrunPath = join(testRoot, 'SharedRsd.xctestrun');
const wdaLogPath = join(testRoot, 'wda-events.ndjson');
const uiRunnerLogPath = join(testRoot, 'ui-runner-events.ndjson');
const common = ['--server-home', testRoot, '--server-addr', `127.0.0.1:${serverPort}`];
const env = inheritedRuntimeEnvironment();
env.RUST_IOS_BACKEND = 'mock';
env.RUST_IOS_MOCK_DEVICES = 'mock-phone';

writeFileSync(xctestrunPath, `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>__xctestrun_metadata__</key><dict><key>FormatVersion</key><integer>2</integer></dict>
<key>TestConfigurations</key><array><dict>
<key>Name</key><string>SharedRsdScheme</string>
<key>TestTargets</key><array><dict>
<key>TestHostBundleIdentifier</key><string>com.example.SharedRsdTests.xctrunner</string>
<key>TestBundlePath</key><string>__TESTHOST__/PlugIns/SharedRsdTests.xctest</string>
<key>IsUITestBundle</key><true/>
<key>UITargetAppPath</key><string>__TESTROOT__/Debug-iphoneos/Demo.app</string>
</dict></array></dict></array>
</dict></plist>`);

function inheritedRuntimeEnvironment() {
  const result = {};
  for (const name of [
    'Path', 'PATH', 'SystemRoot', 'WINDIR', 'TEMP', 'TMP', 'ComSpec', 'PATHEXT',
  ]) {
    if (process.env[name] !== undefined) result[name] = process.env[name];
  }
  return result;
}

function invoke(args, timeoutMs = 20_000) {
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

async function expectJson(args, timeoutMs) {
  const result = await invoke(args, timeoutMs);
  if (result.status !== 0) {
    throw new Error(`command failed (${result.status}): ${args.join(' ')}\n${result.stderr}`);
  }
  return result.stdout.trim() ? JSON.parse(result.stdout) : null;
}

async function waitTask(taskId, predicate, description) {
  const deadline = Date.now() + 20_000;
  let task;
  while (Date.now() < deadline) {
    task = (await expectJson(['task', 'show', taskId, ...common])).value.task;
    if (predicate(task)) return task;
    if (task.runtime_state === 'failed') {
      throw new Error(`${description} failed: ${JSON.stringify(task)}`);
    }
    await delay(100);
  }
  throw new Error(`${description} timed out: ${JSON.stringify(task)}`);
}

function assertSharedRsd(task, description) {
  if (task.runtime.serviceTransport !== 'shared_rsd') {
    throw new Error(`${description} did not reuse shared RSD: ${JSON.stringify(task)}`);
  }
}

function delay(milliseconds) {
  return new Promise(resolveDelay => setTimeout(resolveDelay, milliseconds));
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

let cleanupComplete = false;
let summary;
try {
  let tunnel = (await expectJson([
    'tunnel', 'start', '--userspace', '--userspace-port=0', '--udid', 'mock-phone', ...common,
  ])).value.task;
  tunnel = await waitTask(
    tunnel.id,
    task => task.runtime_state === 'running',
    'userspace tunnel',
  );

  let wda = (await expectJson([
    'runwda',
    '--bundleid=com.example.WebDriverAgentRunner.xctrunner',
    `--log-output=${wdaLogPath}`,
    '--udid', 'mock-phone',
    ...common,
  ])).value.task;
  wda = await waitTask(
    wda.id,
    task => task.runtime_state === 'running' && task.runtime.eventsWritten > 0,
    'WDA',
  );
  assertSharedRsd(wda, 'WDA');

  await expectJson(['kill-server', ...common]);
  const tunnelAfterWdaRestart = await waitTask(
    tunnel.id,
    task => task.runtime_state === 'running' && task.generation >= 2,
    'userspace tunnel after WDA server restart',
  );
  const restoredWda = await waitTask(
    wda.id,
    task => task.runtime_state === 'running' && task.generation >= 2,
    'restored WDA',
  );
  assertSharedRsd(restoredWda, 'restored WDA');
  await expectJson(['task', 'stop', wda.id, ...common]);

  let uiRunner = (await expectJson([
    'ui', 'run', 'wda',
    `--host-port=${uiRunnerPort}`,
    `--log-output=${uiRunnerLogPath}`,
    '--udid', 'mock-phone',
    ...common,
  ])).value.task;
  uiRunner = await waitTask(
    uiRunner.id,
    task => task.runtime_state === 'running' && task.runtime.eventsWritten > 0,
    'UI runner',
  );
  assertSharedRsd(uiRunner, 'UI runner');

  await expectJson(['kill-server', ...common]);
  const tunnelAfterUiRunnerRestart = await waitTask(
    tunnel.id,
    task => task.runtime_state === 'running' && task.generation >= 3,
    'userspace tunnel after UI runner server restart',
  );
  const restoredUiRunner = await waitTask(
    uiRunner.id,
    task => task.runtime_state === 'running' && task.generation >= 2,
    'restored UI runner',
  );
  assertSharedRsd(restoredUiRunner, 'restored UI runner');
  await expectJson(['task', 'stop', uiRunner.id, ...common]);

  let xctest = (await expectJson([
    'runxctest',
    `--xctestrun-file-path=${xctestrunPath}`,
    '--udid', 'mock-phone',
    ...common,
  ])).value.task;
  xctest = await waitTask(
    xctest.id,
    task => task.runtime_state === 'completed',
    'XCTest',
  );
  assertSharedRsd(xctest, 'XCTest');
  const firstOutput = (await expectJson([
    'task', 'output', xctest.id, '--limit=65536', ...common,
  ])).value;
  const firstOutputText = Buffer.from(firstOutput.bytes_base64, 'base64').toString('utf8');
  if (!firstOutputText.includes('"case_finished"') || xctest.generation !== 1) {
    throw new Error(`unexpected XCTest output: ${JSON.stringify({ xctest, firstOutput })}`);
  }

  await expectJson(['kill-server', ...common]);
  const tunnelAfterXCTestRestart = await waitTask(
    tunnel.id,
    task => task.runtime_state === 'running' && task.generation >= 4,
    'userspace tunnel after XCTest server restart',
  );
  const unchangedXCTest = await waitTask(
    xctest.id,
    task => task.runtime_state === 'completed',
    'completed XCTest after server restart',
  );
  if (unchangedXCTest.generation !== 1) {
    throw new Error(`server restart reran finite XCTest: ${JSON.stringify(unchangedXCTest)}`);
  }

  await expectJson(['task', 'restart', xctest.id, ...common]);
  const restartedXCTest = await waitTask(
    xctest.id,
    task => task.runtime_state === 'completed' && task.generation === 2,
    'explicitly restarted XCTest',
  );
  assertSharedRsd(restartedXCTest, 'explicitly restarted XCTest');

  summary = {
    marker: 'SHARED_XCTEST_PROCESS_SMOKE_OK',
    serverPort,
    uiRunnerPort,
    tunnel: {
      id: tunnel.id,
      afterWdaGeneration: tunnelAfterWdaRestart.generation,
      afterUiRunnerGeneration: tunnelAfterUiRunnerRestart.generation,
      afterXCTestGeneration: tunnelAfterXCTestRestart.generation,
    },
    wda: { id: wda.id, restoredGeneration: restoredWda.generation },
    uiRunner: { id: uiRunner.id, restoredGeneration: restoredUiRunner.generation },
    xctest: {
      id: xctest.id,
      generation: restartedXCTest.generation,
      outputBytes: firstOutput.total_bytes,
    },
  };
  writeFileSync(resultPath, JSON.stringify(summary, null, 2) + '\n');

  await expectJson(['task', 'stop', xctest.id, ...common]);
  await expectJson(['task', 'stop', tunnel.id, ...common]);
  const remaining = (await expectJson(['task', 'list', ...common])).value.tasks;
  if (remaining.length !== 0) {
    throw new Error(`shared XCTest smoke leaked tasks: ${JSON.stringify(remaining)}`);
  }
  await expectJson(['kill-server', ...common]);
  cleanupComplete = true;
  console.log(JSON.stringify(summary));
  console.log('SHARED_XCTEST_PROCESS_SMOKE_OK');
} catch (error) {
  writeFileSync(resultPath, JSON.stringify({
    marker: 'SHARED_XCTEST_PROCESS_SMOKE_FAILED',
    error: String(error?.stack ?? error),
  }, null, 2) + '\n');
  throw error;
} finally {
  if (!cleanupComplete && existsSync(join(testRoot, 'server.json'))) {
    await invoke(['kill-server', ...common]).catch(() => {});
  }
}
