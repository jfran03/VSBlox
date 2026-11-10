// VSBlox: shows Roblox Studio as a VS Code panel.
//
// The panel is a webview filled with a marker color. A small native helper
// (../helper) finds that color on screen and keeps the real Studio window
// pinned exactly over it.

const vscode = require('vscode');
const cp = require('child_process');
const fs = require('fs');
const path = require('path');
const readline = require('readline');

// Must match CORNER and FILL in helper/src/win.rs.
const CORNER = 'rgb(254, 1, 253)';
const FILL = 'rgb(46, 46, 47)';

const STATUS_TEXT = {
  starting: 'Starting…',
  searching_host: 'Finding this panel…',
  host_found: 'Panel found',
  waiting_for_studio: 'Waiting for Roblox Studio',
  attached: 'Docking Studio…',
  docked: 'Docked',
  hidden: 'Hidden',
  detached: 'Undocked',
  error: 'Error',
};

let panel = null;
let helper = null;
let statusItem = null;
let lastState = 'starting';
let autoLaunched = false;

function activate(context) {
  statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  statusItem.command = 'vsblox.openStudioPanel';
  context.subscriptions.push(statusItem);

  context.subscriptions.push(
    vscode.commands.registerCommand('vsblox.openStudioPanel', () => openPanel(context)),
    vscode.commands.registerCommand('vsblox.launchStudio', launchStudio),
    vscode.commands.registerCommand('vsblox.undock', () => panel && panel.dispose()),
    vscode.commands.registerCommand('vsblox.setStudioSize', pickStudioSize),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('vsblox.studioSize')) sendSize();
      if (e.affectsConfiguration('vsblox.hideTitleBar')) sendTitleBar();
    }),
  );
}

function deactivate() {
  stopHelper();
}

function openPanel(context) {
  if (panel) {
    panel.reveal();
    return;
  }
  panel = vscode.window.createWebviewPanel(
    'vsblox.studio',
    'Roblox Studio',
    vscode.ViewColumn.Beside,
    { enableScripts: true, retainContextWhenHidden: true },
  );
  panel.webview.html = webviewHtml();

  panel.onDidChangeViewState(() => send({ cmd: panel.visible ? 'show' : 'hide' }));
  panel.webview.onDidReceiveMessage((msg) => {
    if (msg.type === 'resized') send({ cmd: 'recalibrate' });
    if (msg.type === 'launch') launchStudio();
  });
  panel.onDidDispose(() => {
    panel = null;
    stopHelper();
    statusItem.hide();
  });

  autoLaunched = false;
  startHelper(context);
}

function helperPath(context) {
  const candidates = [
    path.join(context.extensionPath, 'bin', 'vsblox-helper.exe'),
    path.join(context.extensionPath, '..', 'helper', 'target', 'release', 'vsblox-helper.exe'),
    path.join(context.extensionPath, '..', 'helper', 'target', 'debug', 'vsblox-helper.exe'),
  ];
  return candidates.find((p) => fs.existsSync(p));
}

function startHelper(context) {
  if (process.platform !== 'win32') {
    setState('error', 'Only Windows is supported in this prototype.');
    return;
  }
  const exe = helperPath(context);
  if (!exe) {
    setState('error', 'Helper not built. Run "cargo build --release" in the helper folder.');
    return;
  }
  helper = cp.spawn(exe, ['--host-exe', path.basename(process.execPath)], {
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: true,
  });
  setState('starting', '');
  sendSize();
  sendTitleBar();

  readline.createInterface({ input: helper.stdout }).on('line', (line) => {
    try {
      const msg = JSON.parse(line);
      if (msg.event === 'status') setState(msg.state, msg.detail);
    } catch {
      console.log('[vsblox-helper]', line);
    }
  });
  helper.stderr.on('data', (d) => console.error('[vsblox-helper]', d.toString()));
  helper.on('exit', (code) => {
    helper = null;
    if (panel && code !== 0) setState('error', `Helper exited with code ${code}`);
  });
}

function stopHelper() {
  if (!helper) return;
  const h = helper;
  helper = null;
  send({ cmd: 'detach' }, h);
  h.stdin.end();
  // Give it a moment to put Studio back, then make sure it's gone.
  setTimeout(() => {
    if (h.exitCode === null) h.kill();
  }, 1500);
}

function send(obj, h = helper) {
  if (h && h.stdin.writable) h.stdin.write(JSON.stringify(obj) + '\n');
}

// "fit" or "WIDTHxHEIGHT" (physical pixels).
function parseSize(value) {
  const m = /^\s*(\d+)\s*[x×]\s*(\d+)\s*$/i.exec(value || '');
  return m ? { width: Number(m[1]), height: Number(m[2]) } : null;
}

function sendSize() {
  const size = parseSize(vscode.workspace.getConfiguration('vsblox').get('studioSize'));
  send(size ? { cmd: 'size', ...size } : { cmd: 'size' });
}

function sendTitleBar() {
  send({ cmd: 'title_bar', hide: vscode.workspace.getConfiguration('vsblox').get('hideTitleBar') !== false });
}

async function pickStudioSize() {
  const current = vscode.workspace.getConfiguration('vsblox').get('studioSize') || 'fit';
  const presets = ['fit', '1280x720', '1600x900', '1920x1080', '2560x1440'];
  const items = [
    ...presets.map((p) => ({
      label: p === 'fit' ? 'Fit panel' : p,
      description: p === current ? 'current' : p === 'fit' ? 'Studio fills the panel' : '',
      value: p,
    })),
    { label: 'Custom…', value: 'custom' },
  ];
  const pick = await vscode.window.showQuickPick(items, { placeHolder: 'Roblox Studio size inside the panel' });
  if (!pick) return;
  let value = pick.value;
  if (value === 'custom') {
    value = await vscode.window.showInputBox({
      prompt: 'Studio size in pixels, like 1366x768',
      value: parseSize(current) ? current : '1920x1080',
      validateInput: (v) => (parseSize(v) ? null : 'Use WIDTHxHEIGHT, like 1366x768'),
    });
    if (!value) return;
  }
  await vscode.workspace.getConfiguration('vsblox').update('studioSize', value, vscode.ConfigurationTarget.Global);
}

function setState(state, detail) {
  lastState = state;
  // The panel is found but no Studio window exists yet: open Studio once.
  if (state === 'waiting_for_studio' && !autoLaunched) {
    autoLaunched = true;
    if (vscode.workspace.getConfiguration('vsblox').get('autoLaunchStudio')) launchStudio();
  }
  const text = STATUS_TEXT[state] || state;
  statusItem.text = `$(device-desktop) Studio: ${text}`;
  statusItem.tooltip = detail || text;
  statusItem.show();
  if (panel) panel.webview.postMessage({ type: 'state', state, text, detail });
}

function findStudioExe() {
  const versions = path.join(process.env.LOCALAPPDATA || '', 'Roblox', 'Versions');
  if (!fs.existsSync(versions)) return null;
  let best = null;
  for (const name of fs.readdirSync(versions)) {
    const exe = path.join(versions, name, 'RobloxStudioBeta.exe');
    if (!fs.existsSync(exe)) continue;
    const mtime = fs.statSync(exe).mtimeMs;
    if (!best || mtime > best.mtime) best = { exe, mtime };
  }
  return best && best.exe;
}

function launchStudio() {
  const exe = findStudioExe();
  if (!exe) {
    vscode.window.showErrorMessage('Could not find Roblox Studio under %LOCALAPPDATA%\\Roblox\\Versions.');
    return;
  }
  cp.spawn(exe, [], { detached: true, stdio: 'ignore' }).unref();
  vscode.window.setStatusBarMessage('Launching Roblox Studio…', 4000);
}

function webviewHtml() {
  return `<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
  html, body { margin: 0; padding: 0; width: 100%; height: 100%; overflow: hidden; }
  /* The helper finds this panel by the corner squares and checks the flat
     fill to tell when something covers it. Keep both exact: no gradients,
     borders or opacity. */
  body { background: ${FILL}; }
  .corner { position: fixed; width: 4px; height: 4px; background: ${CORNER}; }
  #msg {
    position: absolute; left: 50%; top: 50%; transform: translate(-50%, -50%);
    font: 13px var(--vscode-font-family, sans-serif); color: #fff; text-align: center;
    background: rgba(0, 0, 0, 0.55); padding: 14px 18px; border-radius: 6px;
  }
  #msg.hidden { display: none; }
  button { margin-top: 10px; font: inherit; padding: 4px 12px; cursor: pointer; }
</style>
</head>
<body>
  <div class="corner" style="left:0;top:0"></div>
  <div class="corner" style="right:0;top:0"></div>
  <div class="corner" style="left:0;bottom:0"></div>
  <div class="corner" style="right:0;bottom:0"></div>
  <div id="msg">
    <div id="text">Starting…</div>
    <div id="detail" style="opacity:.8;margin-top:4px"></div>
    <button id="launch" hidden>Launch Roblox Studio</button>
  </div>
<script>
  const vscode = acquireVsCodeApi();
  const msg = document.getElementById('msg');
  const launch = document.getElementById('launch');
  launch.onclick = () => vscode.postMessage({ type: 'launch' });
  // Keep the corner squares 8 screen pixels wide whatever the VS Code zoom.
  function sizeCorners() {
    const px = 8 / window.devicePixelRatio + 'px';
    for (const c of document.querySelectorAll('.corner')) { c.style.width = px; c.style.height = px; }
  }
  sizeCorners();
  new ResizeObserver(() => { sizeCorners(); vscode.postMessage({ type: 'resized' }); }).observe(document.body);
  window.addEventListener('message', (e) => {
    const m = e.data;
    if (m.type !== 'state') return;
    // Once Studio sits on top, keep the panel a plain marker color so the
    // helper can tell when something else (menus, palette) covers it.
    const docked = m.state === 'docked' || m.state === 'hidden' || m.state === 'attached';
    msg.classList.toggle('hidden', docked);
    document.getElementById('text').textContent = m.text;
    document.getElementById('detail').textContent = m.detail || '';
    launch.hidden = m.state !== 'waiting_for_studio';
  });
</script>
</body>
</html>`;
}

module.exports = { activate, deactivate };
