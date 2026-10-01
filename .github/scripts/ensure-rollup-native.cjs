'use strict';

// Ubuntu x64 CI only. Some package-manager installs omit Rollup's optional
// native package. Recover exactly the resolved Rollup version in an isolated
// temporary npm project, without rewriting the application's manifests/locks.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { createRequire } = require('node:module');
const { execFileSync } = require('node:child_process');

if (process.platform !== 'linux' || process.arch !== 'x64') {
  throw new Error('This dependency recovery is only for Ubuntu x64 CI.');
}
const projectRequire = createRequire(path.join(process.cwd(), 'package.json'));
const viteRequire = createRequire(projectRequire.resolve('vite/package.json'));
const rollupManifest = viteRequire.resolve('rollup/package.json');
const rollupRequire = createRequire(rollupManifest);
const nativePackage = '@rollup/rollup-linux-x64-gnu';
try {
  rollupRequire('./dist/native.js');
  console.log('Rollup native dependency is available.');
  process.exit(0);
} catch (error) {
  if (error?.cause?.code !== 'MODULE_NOT_FOUND' || !error.cause.message.includes(nativePackage)) {
    throw error;
  }
}
const version = JSON.parse(fs.readFileSync(rollupManifest, 'utf8')).version;
if (!/^4\.\d+\.\d+$/.test(version)) {
  throw new Error('Unexpected Rollup version; refusing unbounded dependency recovery.');
}
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'kiro-rollup-native-'));
try {
  fs.writeFileSync(path.join(temporary, 'package.json'), JSON.stringify({ private: true }));
  execFileSync('npm', [
    'install', '--prefix', temporary, '--ignore-scripts', '--no-audit', '--no-fund',
    '--package-lock=false', `${nativePackage}@${version}`,
  ], { stdio: 'inherit', timeout: 120000 });
  const source = path.join(temporary, 'node_modules', nativePackage);
  const installed = JSON.parse(fs.readFileSync(path.join(source, 'package.json'), 'utf8'));
  if (installed.name !== nativePackage || installed.version !== version) {
    throw new Error('Recovered native dependency does not match resolved Rollup.');
  }
  const destination = path.join(path.dirname(rollupManifest), 'node_modules', nativePackage);
  if (fs.existsSync(destination)) {
    throw new Error('Existing native dependency was not loadable; refusing to overwrite it.');
  }
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  fs.cpSync(source, destination, { recursive: true, force: false, errorOnExist: true });
  rollupRequire('./dist/native.js');
  console.log(`Recovered ${nativePackage}@${version} without changing project manifests.`);
} finally {
  fs.rmSync(temporary, { recursive: true, force: true });
}
