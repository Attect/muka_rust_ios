import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import net from 'node:net';

const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
const reference = resolve(process.argv[3] ?? 'C:/Users/<user>/bin/ios.exe');
if (!existsSync(reference)) throw new Error(`reference go-ios binary not found: ${reference}`);

const home = mkdtempSync(join(tmpdir(), 'rust-ios-compat-'));
const port = await freePort();
const common = ['--server-home', home, '--server-addr', `127.0.0.1:${port}`];
const env = {
  ...process.env,
  RUST_IOS_BACKEND: 'mock',
  RUST_IOS_MOCK_DEVICES: 'mock-phone',
};

function invoke(executable, args, options = {}) {
  return spawnSync(executable, args, {
    env: options.env ?? process.env,
    encoding: 'utf8',
    timeout: 15000,
    windowsHide: true,
    maxBuffer: 16 * 1024 * 1024,
    input: options.input,
  });
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

function referenceCommands(help) {
  const commands = [];
  let inCommands = false;
  for (const line of help.split(/\r?\n/)) {
    if (line === 'Commands:') {
      inCommands = true;
      continue;
    }
    if (!inCommands) continue;
    if (line.startsWith("Run 'ios help")) break;
    const match = line.match(/^  ([a-z][a-z0-9-]*(?: [a-z][a-z0-9-]*)*)\s{2,}/);
    if (match) commands.push(match[1]);
  }
  return commands;
}

try {
  const referenceHelp = invoke(reference, ['--help']);
  if (referenceHelp.status !== 0) {
    throw new Error(`reference --help failed: ${referenceHelp.stderr}`);
  }
  const commands = referenceCommands(referenceHelp.stdout);
  if (commands.length < 50) throw new Error(`parsed too few reference commands: ${commands.length}`);

  for (const command of commands) {
    const result = invoke(bin, [...command.split(' '), '--help', ...common], { env });
    if (result.status !== 0) {
      throw new Error(
        `Rust help failed for ${command}: status=${result.status}\nstdout=${result.stdout}\nstderr=${result.stderr}`
      );
    }
    if (existsSync(join(home, 'server.json'))) {
      throw new Error(`help unexpectedly started server for ${command}`);
    }
  }
  console.log(`REFERENCE_HELP_COMMANDS=${commands.length}`);

  const version = invoke(bin, ['--version']);
  const versionValue = JSON.parse(version.stdout);
  if (version.status !== 0 || typeof versionValue.version !== 'string') {
    throw new Error(`invalid Rust version output: ${version.stdout} ${version.stderr}`);
  }
  console.log(`VERSION_SHAPE=ok rust=${versionValue.version}`);

  const list = invoke(bin, ['list', ...common], { env });
  const listValue = JSON.parse(list.stdout);
  // mock backend 通过 RUST_IOS_MOCK_DEVICES 附加 mock-phone，后续测试依赖它；
  // 空设备列表形状由 mock/in-process 单元测试覆盖（docs/testing.md）。
  if (list.status !== 0 || !Array.isArray(listValue.deviceList) || listValue.deviceList.length === 0) {
    throw new Error(`invalid mock list output: ${list.stdout} ${list.stderr}`);
  }
  console.log('MOCK_DEVICE_LIST=ok');

  const gestaltJson = invoke(bin, ['mobilegestalt', 'MockGestalt', ...common], { env });
  const gestaltJsonValue = JSON.parse(gestaltJson.stdout);
  if (
    gestaltJson.status !== 0 ||
    gestaltJsonValue.Status !== 'Success' ||
    gestaltJsonValue.Diagnostics?.MobileGestalt?.MockGestalt !== true
  ) {
    throw new Error(
      `invalid mock MobileGestalt JSON: status=${gestaltJson.status} stdout=${gestaltJson.stdout} stderr=${gestaltJson.stderr}`
    );
  }
  const gestaltPlist = invoke(
    bin,
    ['mobilegestalt', 'MockGestalt', '--plist', '--pretty', '--nojson', ...common],
    { env }
  );
  if (
    gestaltPlist.status !== 0 ||
    !gestaltPlist.stdout.startsWith('<?xml') ||
    !gestaltPlist.stdout.includes('<key>Diagnostics</key>') ||
    !gestaltPlist.stdout.includes('<key>MockGestalt</key>') ||
    !gestaltPlist.stdout.includes('<true/>')
  ) {
    throw new Error(
      `invalid mock MobileGestalt plist: status=${gestaltPlist.status} stdout=${gestaltPlist.stdout} stderr=${gestaltPlist.stderr}`
    );
  }
  console.log('MOBILEGESTALT_JSON_PLIST=ok');

  const audit = invoke(bin, ['ax', 'audit', ...common], { env });
  const auditValue = JSON.parse(audit.stdout);
  if (audit.status !== 0 || auditValue.issueCount !== 1 || auditValue.apiVersion !== 21) {
    throw new Error(`invalid mock ax audit result: status=${audit.status} stdout=${audit.stdout} stderr=${audit.stderr}`);
  }
  console.log('AX_AUDIT_SHAPE=ok');

  const reset = invoke(bin, ['resetax', ...common], { env });
  const resetValue = JSON.parse(reset.stdout);
  if (reset.status !== 0 || resetValue.status !== 'reset') {
    throw new Error(`invalid mock resetax result: status=${reset.status} stdout=${reset.stdout} stderr=${reset.stderr}`);
  }
  console.log('RESETAX_SHAPE=ok');

  const ui = invoke(bin, ['ui', 'status', '--driver=wda', ...common], { env });
  const uiValue = JSON.parse(ui.stdout);
  if (ui.status !== 0 || uiValue.driver !== 'wda' || uiValue.ready !== true) {
    throw new Error(`invalid mock ui status result: status=${ui.status} stdout=${ui.stdout} stderr=${ui.stderr}`);
  }
  console.log('UI_STATUS_SHAPE=ok');

  const uiScreenshotPath = join(home, 'mock-ui.png');
  const uiScreenshot = invoke(bin, ['ui', 'screenshot', `--output=${uiScreenshotPath}`, ...common], { env });
  const uiScreenshotValue = JSON.parse(uiScreenshot.stdout);
  if (
    uiScreenshot.status !== 0 ||
    uiScreenshotValue.bytes !== 8 ||
    readFileSync(uiScreenshotPath).toString('hex') !== '89504e470d0a1a0a'
  ) {
    throw new Error(
      `invalid mock ui screenshot result: status=${uiScreenshot.status} stdout=${uiScreenshot.stdout} stderr=${uiScreenshot.stderr}`
    );
  }
  console.log('UI_SCREENSHOT_FILE=ok');

  const appIconPath = join(home, 'mock-app-icon.png');
  const appIcon = invoke(
    bin,
    ['get-app-icon', 'com.apple.Preferences', `--output=${appIconPath}`, ...common],
    { env }
  );
  const appIconValue = JSON.parse(appIcon.stdout);
  if (
    appIcon.status !== 0 ||
    appIconValue.bundleID !== 'com.apple.Preferences' ||
    appIconValue.bytes !== 68 ||
    readFileSync(appIconPath).subarray(0, 8).toString('hex') !== '89504e470d0a1a0a'
  ) {
    throw new Error(
      `invalid mock app icon result: status=${appIcon.status} stdout=${appIcon.stdout} stderr=${appIcon.stderr}`
    );
  }
  const missingAppIconBundle = invoke(bin, ['get-app-icon', ...common], { env });
  const missingAppIconValue = JSON.parse(missingAppIconBundle.stderr);
  if (
    missingAppIconBundle.status !== 3 ||
    missingAppIconValue.error?.code !== 'invalid_request'
  ) {
    throw new Error(
      `invalid missing app icon bundle error: status=${missingAppIconBundle.status} stderr=${missingAppIconBundle.stderr}`
    );
  }
  console.log('APP_ICON_FILE_AND_VALIDATION=ok');

  const activation = invoke(bin, ['activate', ...common], { env });
  const activationValue = JSON.parse(activation.stdout);
  if (
    activation.status !== 0 ||
    activationValue.status !== 'already_activated' ||
    activationValue.activationState !== 'Activated' ||
    activationValue.verified !== true ||
    activationValue.mock !== true
  ) {
    throw new Error(
      `invalid mock activation result: status=${activation.status} stdout=${activation.stdout} stderr=${activation.stderr}`
    );
  }
  console.log('ACTIVATE_IDEMPOTENT=ok');

  const implicitErase = invoke(bin, ['erase', '--force', ...common], { env });
  const implicitEraseValue = JSON.parse(implicitErase.stderr);
  if (implicitErase.status !== 3 || implicitEraseValue.error?.code !== 'invalid_request') {
    throw new Error(
      `invalid erase safety error: status=${implicitErase.status} stderr=${implicitErase.stderr}`
    );
  }
  const erase = invoke(bin, ['erase', '--force', '--udid', 'mock-phone', ...common], { env });
  const eraseValue = JSON.parse(erase.stdout);
  if (
    erase.status !== 0 ||
    eraseValue.status !== 'erase_requested' ||
    eraseValue.preserveDataPlan !== true ||
    eraseValue.udid !== 'mock-phone' ||
    eraseValue.mock !== true
  ) {
    throw new Error(
      `invalid mock erase result: status=${erase.status} stdout=${erase.stdout} stderr=${erase.stderr}`
    );
  }
  console.log('ERASE_SAFETY_GATE=ok');

  const ambiguousPair = invoke(
    bin,
    ['pair', '--name=lab-host', '--p12file=organization.p12', ...common],
    { env }
  );
  const ambiguousPairValue = JSON.parse(ambiguousPair.stderr);
  if (ambiguousPair.status !== 3 || ambiguousPairValue.error?.code !== 'invalid_request') {
    throw new Error(
      `invalid supervised pair ambiguity error: status=${ambiguousPair.status} stderr=${ambiguousPair.stderr}`
    );
  }
  const supervisedPair = invoke(
    bin,
    ['pair', '--p12file=organization.p12', '--password=secret', '--udid', 'mock-phone', ...common],
    { env }
  );
  const supervisedPairValue = JSON.parse(supervisedPair.stdout);
  if (
    supervisedPair.status !== 0 ||
    supervisedPairValue.saved !== true ||
    supervisedPairValue.supervised !== true ||
    supervisedPairValue.udid !== 'mock-phone' ||
    supervisedPairValue.mock !== true
  ) {
    throw new Error(
      `invalid supervised pair result: status=${supervisedPair.status} stdout=${supervisedPair.stdout} stderr=${supervisedPair.stderr}`
    );
  }
  console.log('SUPERVISED_PAIR_CONTRACT=ok');

  const invalidWallpaper = invoke(
    bin,
    ['set-wallpaper', 'wallpaper.png', '--screen=display', '--p12file=identity.p12', ...common],
    { env }
  );
  const invalidWallpaperValue = JSON.parse(invalidWallpaper.stderr);
  if (
    invalidWallpaper.status !== 3 ||
    invalidWallpaperValue.error?.code !== 'invalid_request'
  ) {
    throw new Error(
      `invalid wallpaper screen error: status=${invalidWallpaper.status} stderr=${invalidWallpaper.stderr}`
    );
  }
  const wallpaper = invoke(
    bin,
    ['set-wallpaper', 'wallpaper.png', '--screen=lock', '--p12file=identity.p12', ...common],
    { env }
  );
  const wallpaperValue = JSON.parse(wallpaper.stdout);
  if (
    wallpaper.status !== 0 ||
    wallpaperValue.status !== 'set' ||
    wallpaperValue.path !== 'wallpaper.png' ||
    wallpaperValue.screen !== 'lock' ||
    wallpaperValue.where !== 1 ||
    wallpaperValue.supervised !== true ||
    wallpaperValue.mock !== true
  ) {
    throw new Error(
      `invalid mock wallpaper result: status=${wallpaper.status} stdout=${wallpaper.stdout} stderr=${wallpaper.stderr}`
    );
  }
  console.log('SET_WALLPAPER_CONTRACT=ok');

  const shellHome = mkdtempSync(join(tmpdir(), 'rust-ios-js-shell-'));
  const shellPort = await freePort();
  const shellCommon = [
    '--server-home', shellHome, '--server-addr', `127.0.0.1:${shellPort}`,
  ];
  const shellEnv = {
    ...process.env,
    RUST_IOS_BACKEND: 'mock',
    RUST_IOS_MOCK_DEVICES: 'mock-phone',
  };
  try {
    const shell = invoke(
      bin,
      ['webinspector', 'js-shell', '--udid', 'mock-phone', ...shellCommon],
      { env: shellEnv, input: '2 + 2\n.exit\n' }
    );
    if (shell.status !== 0 || shell.stdout.trim() !== '4' || shell.stderr !== '') {
      throw new Error(
        `invalid JS shell result: status=${shell.status} stdout=${shell.stdout} stderr=${shell.stderr}`
      );
    }
    const shellTasks = invoke(bin, ['task', 'list', ...shellCommon], { env: shellEnv });
    const shellTasksValue = JSON.parse(shellTasks.stdout);
    if (shellTasks.status !== 0 || shellTasksValue.value?.tasks?.length !== 0) {
      throw new Error(
        `JS shell leaked task: status=${shellTasks.status} stdout=${shellTasks.stdout} stderr=${shellTasks.stderr}`
      );
    }
  } finally {
    if (existsSync(join(shellHome, 'server.json'))) {
      invoke(bin, ['kill-server', ...shellCommon], { env: shellEnv });
    }
    rmSync(shellHome, { recursive: true, force: true });
  }
  console.log('WEBINSPECTOR_JS_SHELL=ok');

  const wda = invoke(bin, [
    'runwda', '--bundleid=com.example.WebDriverAgentRunner.xctrunner', '--log-output=-',
    '--udid=mock-phone', ...common,
  ], { env });
  if (wda.status !== 0) {
    throw new Error(
      `runwda event capture failed: status=${wda.status} stdout=${wda.stdout} stderr=${wda.stderr}`
    );
  }
  const wdaValue = JSON.parse(wda.stdout);
  const wdaTask = wdaValue.value?.task;
  if (!wdaTask?.id || wdaTask.runtime_state !== 'running') {
    throw new Error(
      `runwda event capture failed: status=${wda.status} stdout=${wda.stdout} stderr=${wda.stderr}`
    );
  }
  const wdaOutput = invoke(bin, ['task', 'output', wdaTask.id, ...common], { env });
  if (wdaOutput.status !== 0) {
    throw new Error(
      `read WDA event capture failed: status=${wdaOutput.status} stdout=${wdaOutput.stdout} stderr=${wdaOutput.stderr}`
    );
  }
  const wdaOutputValue = JSON.parse(wdaOutput.stdout).value;
  const wdaEvents = Buffer.from(wdaOutputValue.bytes_base64, 'base64').toString('utf8')
    .trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
  if (wdaOutputValue.content_type !== 'application/x-ndjson'
      || wdaEvents.length !== 1 || wdaEvents[0].event?.kind !== 'test_plan_started') {
    throw new Error(
      `invalid WDA event capture: status=${wdaOutput.status} stdout=${wdaOutput.stdout} stderr=${wdaOutput.stderr}`
    );
  }
  const wdaStop = invoke(bin, ['task', 'stop', wdaTask.id, ...common], { env });
  if (wdaStop.status !== 0) throw new Error(`WDA stop failed: ${wdaStop.stderr}`);
  console.log('WDA_EVENT_CAPTURE=ok');

  const shutdown = invoke(bin, ['kill-server', ...common], { env });
  if (shutdown.status !== 0) throw new Error(`kill-server failed: ${shutdown.stderr}`);

  // go-ios v1.2.1 新增命令（参考二进制可能早于 v1.2.1，显式验证而非从 help 提取）
  for (const args of [
    ['pasteboard', '--help'],
    ['pasteboard', 'get', '--help'],
    ['pasteboard', 'set', '--help'],
    ['mdm', '--help'],
    ['mdm', 'security-info', '--help'],
    ['mdm', 'fetch-unlock-token', '--help'],
    ['mdm', 'clear-passcode', '--help'],
    ['mdm', 'clear-screen-time-password', '--help'],
  ]) {
    const result = invoke(bin, [...args, ...common], { env });
    if (result.status !== 0) {
      throw new Error(
        `Rust help failed for ${args.join(' ')}: status=${result.status}\nstdout=${result.stdout}\nstderr=${result.stderr}`
      );
    }
    if (existsSync(join(home, 'server.json'))) {
      throw new Error(`help unexpectedly started server for ${args.join(' ')}`);
    }
  }
  const pasteboardGet = invoke(bin, ['pasteboard', 'get', ...common], { env });
  const pasteboardGetValue = JSON.parse(pasteboardGet.stdout);
  if (
    pasteboardGet.status !== 0 ||
    pasteboardGetValue.pasteboard !== 'general' ||
    pasteboardGetValue.text !== 'mock pasteboard text' ||
    pasteboardGetValue.serviceTransport !== 'mock'
  ) {
    throw new Error(
      `invalid mock pasteboard get: status=${pasteboardGet.status} stdout=${pasteboardGet.stdout} stderr=${pasteboardGet.stderr}`
    );
  }
  const pasteboardSet = invoke(bin, ['pasteboard', 'set', 'compat-text', ...common], { env });
  const pasteboardSetValue = JSON.parse(pasteboardSet.stdout);
  if (pasteboardSet.status !== 0 || pasteboardSetValue.status !== 'set' || 'text' in pasteboardSetValue) {
    throw new Error(
      `invalid mock pasteboard set: status=${pasteboardSet.status} stdout=${pasteboardSet.stdout} stderr=${pasteboardSet.stderr}`
    );
  }
  const mdmInfo = invoke(bin, ['mdm', 'security-info', '--p12file', 'identity.p12', ...common], { env });
  const mdmInfoValue = JSON.parse(mdmInfo.stdout);
  if (
    mdmInfo.status !== 0 ||
    mdmInfoValue.securityInfo?.PasscodePresent !== false ||
    mdmInfoValue.serviceTransport !== 'mock'
  ) {
    throw new Error(
      `invalid mock mdm security-info: status=${mdmInfo.status} stdout=${mdmInfo.stdout} stderr=${mdmInfo.stderr}`
    );
  }
  const mdmNoIdentity = invoke(bin, ['mdm', 'security-info', ...common], { env });
  if (
    mdmNoIdentity.status === 0 ||
    !(mdmNoIdentity.stdout + mdmNoIdentity.stderr).includes('invalid_request')
  ) {
    throw new Error(
      `mdm without --p12file must fail: status=${mdmNoIdentity.status} stdout=${mdmNoIdentity.stdout} stderr=${mdmNoIdentity.stderr}`
    );
  }
  console.log('V121_COMMANDS=ok');
  console.log('CLI_COMPAT_OK');
} finally {
  if (existsSync(join(home, 'server.json'))) {
    invoke(bin, ['kill-server', ...common], { env });
  }
  rmSync(home, { recursive: true, force: true });
}
