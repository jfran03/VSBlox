// Builds the helper, copies it into the extension, and packages a .vsix.
// Run from anywhere: node extension/scripts/package.js
const { execSync } = require('child_process');
const fs = require('fs');
const path = require('path');

const ext = path.resolve(__dirname, '..');
const helper = path.resolve(ext, '..', 'helper');
const run = (cmd, cwd) => execSync(cmd, { cwd, stdio: 'inherit' });

// Build into its own folder so a running helper (from the installed
// extension or an F5 session) doesn't lock the output file.
const target = path.join(helper, 'target-package');
run(`cargo build --release --target-dir "${target}"`, helper);

fs.mkdirSync(path.join(ext, 'bin'), { recursive: true });
fs.copyFileSync(path.join(target, 'release', 'vsblox-helper.exe'), path.join(ext, 'bin', 'vsblox-helper.exe'));

run('npx --yes @vscode/vsce@3 package --no-dependencies --allow-missing-repository --skip-license', ext);
