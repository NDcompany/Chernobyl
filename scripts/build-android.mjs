#!/usr/bin/env node
// build-android.mjs — reproducible Android APK build (Expo prebuild + Gradle).
//
//   node scripts/build-android.mjs                 arm64-v8a + x86_64 release APK
//   node scripts/build-android.mjs --abi x86_64    one ABI (emulator testing)
//
// Needs JDK 17 (JAVA_HOME) and the Android SDK (ANDROID_HOME) with platform 36,
// build-tools 36, NDK 27.1 and a CMake >= 3.31: React Native object paths pass
// Windows' 260-character limit, and only the ninja shipped with CMake 3.31+
// is long-path aware (LongPathsEnabled must be 1 as well). The APK lands in
// release/ where scripts/package-release.mjs hashes it (not dist-android:
// expo export wipes that folder).
// It is signed with the debug keystore of Expo's template: installable, not a
// Play Store identity (same disclosure stance as ADR-007).
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const mobile = join(root, 'apps', 'mobile');
const android = join(mobile, 'android');
const abiArg = process.argv.indexOf('--abi');
const abis = abiArg > 0 ? process.argv[abiArg + 1] : 'arm64-v8a,x86_64';

const sdk = process.env.ANDROID_HOME ?? process.env.ANDROID_SDK_ROOT;
if (!sdk || !existsSync(sdk)) fail('set ANDROID_HOME to the Android SDK');
if (!process.env.JAVA_HOME) fail('set JAVA_HOME to a JDK 17');

const cmakeVersions = existsSync(join(sdk, 'cmake'))
  ? readdirSync(join(sdk, 'cmake')).filter((v) => /^\d+\.\d+/.test(v))
  : [];
const cmake = cmakeVersions
  .filter((v) => cmp(v, '3.31.0') >= 0)
  .sort(cmp)
  .pop();
if (!cmake) fail(`no CMake >= 3.31 under ${sdk}\\cmake — install it: sdkmanager "cmake;3.31.6"`);

// the app bundles the workspace packages from their compiled dist: build
// them first or the APK ships whatever stale dist is on disk
run('npm', ['run', 'build:e2e'], root);
// Icons, splash and manifest are baked into the native project at prebuild
// time, so prebuild again whenever its inputs change (skipping it once shipped
// the old icon). Non-interactive prebuild recreates android/ from scratch,
// which throws away the native build cache, hence the input hash.
const stamp = join(android, '.prebuild-inputs');
const inputs = prebuildInputs();
if (!existsSync(android) || !existsSync(stamp) || readFileSync(stamp, 'utf8') !== inputs) {
  run('npx', ['expo', 'prebuild', '--platform', 'android', '--no-install'], mobile, { CI: '1' });
  writeFileSync(stamp, inputs);
}

const esc = (p) => p.replace(/\\/g, '\\\\').replace(/:/g, '\\:');
writeFileSync(
  join(android, 'local.properties'),
  `sdk.dir=${esc(sdk)}\ncmake.dir=${esc(join(sdk, 'cmake', cmake))}\n`,
);

// absolute: cmd.exe does not search the cwd when NoDefaultCurrentDirectoryInExePath is set
const gradlew = join(android, process.platform === 'win32' ? 'gradlew.bat' : 'gradlew');
run(gradlew, ['assembleRelease', `-PreactNativeArchitectures=${abis}`, '--console=plain'], android, {
  NODE_ENV: 'production',
});

const apk = join(android, 'app', 'build', 'outputs', 'apk', 'release', 'app-release.apk');
if (!existsSync(apk)) fail(`build finished but ${apk} is missing`);
const version = JSON.parse(readFileSync(join(mobile, 'app.json'), 'utf8')).expo.version;
const outDir = join(root, 'release');
mkdirSync(outDir, { recursive: true });
const out = join(outDir, `chernobyl-${version}-${abis.replace(/,/g, '+')}.apk`);
copyFileSync(apk, out);
console.log(`APK: ${out}`);

/** sha256 over everything prebuild reads: app.json, the assets it names and
 *  the local native modules' sources. */
function prebuildInputs() {
  const h = createHash('sha256');
  const walk = (dir) => {
    for (const e of readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const p = join(dir, e.name);
      if (e.isDirectory()) {
        if (e.name !== 'build' && e.name !== '.gradle' && e.name !== 'node_modules') walk(p);
      } else {
        h.update(p.slice(mobile.length)).update(readFileSync(p));
      }
    }
  };
  h.update(readFileSync(join(mobile, 'app.json'))).update(readFileSync(join(mobile, 'package.json')));
  walk(join(mobile, 'assets'));
  walk(join(mobile, 'modules'));
  return h.digest('hex');
}
function run(cmd, args, cwd, env = {}) {
  const r = spawnSync(cmd, args, { cwd, stdio: 'inherit', shell: process.platform === 'win32', env: { ...process.env, ...env } });
  if (r.status !== 0) fail(`${cmd} ${args.join(' ')} exited ${r.status}`);
}
function cmp(a, b) {
  const x = a.split('.').map(Number);
  const y = b.split('.').map(Number);
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    if ((x[i] ?? 0) !== (y[i] ?? 0)) return (x[i] ?? 0) - (y[i] ?? 0);
  }
  return 0;
}
function fail(msg) {
  console.error(`build-android: ${msg}`);
  process.exit(1);
}
