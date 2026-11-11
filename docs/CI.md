# Continuous Integration & Delivery

This document covers the GitHub Actions pipelines for CHERNOBYL, how Android release signing works in CI, and how to reproduce each gate locally.

## Workflows

### 1. `ci.yml` — Continuous Integration

Runs on every push to any branch and on all pull requests.

- **`ts` job** (`ubuntu-latest`):
  Sets up Node 24 with npm caching, installs dependencies via `npm ci`, and executes the complete TypeScript verification chain:
  * `npm run typecheck` (strict TypeScript checks across packages and apps)
  * `npm run lint` (ESLint on the mobile application)
  * `npm test` (unit tests across all packages and apps)
  * `npm run build` (builds shared packages and reference host)
- **`rust` job** (`windows-latest`):
  Sets up stable Rust with `clippy` and `rustfmt`, caches cargo registries and build artifacts, and executes:
  * `cargo test --manifest-path services/companion-rs/Cargo.toml`
  * `cargo clippy --manifest-path services/companion-rs/Cargo.toml --all-targets -- -D warnings`
  * `cargo fmt --manifest-path services/companion-rs/Cargo.toml --check`
  * `cargo run --manifest-path services/companion-rs/Cargo.toml -- --self-test`

  > **Note on OS Support**: The Rust desktop companion (`services/companion-rs`) is a zero-dependency, Windows-first runtime. It directly binds Windows APIs (`kernel32`, `psapi`, `ws2_32`, `crypt32`/DPAPI, and raw Winsock socket options) without POSIX fallbacks. It therefore builds and executes only on `windows-latest`.

- **`interop` job** (`windows-latest`):
  Depends on both `ts` and `rust` passing (`needs: [ts, rust]`). Runs on `windows-latest` to verify the end-to-end integration between the TypeScript packages and the compiled Rust companion daemon:
  * `npm run verify:e2e` (`node services/companion-rs/tools/e2e-interop.mjs`)

- **Security & Concurrency**:
  Runs with least-privilege permissions (`contents: read`) and cancel-in-progress concurrency grouped per ref.

---

### 2. `android.yml` — Android APK Build & Signing

Triggered on demand via `workflow_dispatch` and automatically on release tags matching `v*`.

- Runs on `ubuntu-latest`.
- Sets up JDK 17 (Temurin) and Android SDK components via `sdkmanager`:
  * Android Platform 36 (`platforms;android-36`)
  * Build-Tools 36.0.0 (`build-tools;36.0.0`)
  * NDK 27.1.12297006 (`ndk;27.1.12297006`)
  * CMake 3.31.6 (`cmake;3.31.6`)
- Runs `npm ci` and `node scripts/build-android.mjs`.
- Produces the release APK (`arm64-v8a` + `x86_64`) into `release/` and uploads it as an artifact (`chernobyl-apk`).
- If repository signing secrets are configured (see below), the APK is re-signed with the production release keystore; otherwise, the APK remains signed with the debug keystore for development.

---

### 3. `release.yml` — Companion Binary & Draft Release

Triggered automatically on release tags matching `v*`.

- Runs on `windows-latest`.
- Builds the optimized release companion binary:
  `cargo build --release --manifest-path services/companion-rs/Cargo.toml`
- Runs `node scripts/package-release.mjs` to calculate standard SHA-256 checksums across all release artifacts.
- Publishes a draft GitHub Release using `softprops/action-gh-release@v2` with `contents: write` permissions, attaching:
  * `services/companion-rs/target/release/chernobyl-companion.exe`
  * `release/SHA256SUMS.txt`
- The Android APK artifact built concurrently by `android.yml` can be downloaded and attached to this draft release prior to publishing.

---

### 4. `dependabot.yml` — Dependency Maintenance

Checks GitHub Actions workflows on a weekly cadence for updates.

---

## Android Signing Secrets

By default, CI produces a debug-signed APK that can be installed on testing devices. To produce production-signed APKs matching an existing installation, configure two GitHub Actions secrets in **Repository Settings → Secrets and variables → Actions**:

1. `ANDROID_KEYSTORE_BASE64`: The base64-encoded content of your `.jks` keystore file.
2. `ANDROID_KEYSTORE_PASS`: The password string stored in your `.pass` file.

### Generating the Secrets from Local Keys

The local release key is generated once using `scripts/make-signing-key.ps1`:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\make-signing-key.ps1
```

This writes:
- `G:\Secrets\Android\chernobyl-release.jks`
- `G:\Secrets\Android\chernobyl-release.pass`

To encode them for GitHub Secrets using PowerShell:

```powershell
# 1. Base64 encode the keystore for ANDROID_KEYSTORE_BASE64
$bytes = [System.IO.File]::ReadAllBytes('G:\Secrets\Android\chernobyl-release.jks')
$base64 = [System.Convert]::ToBase64String($bytes)
Set-Clipboard -Value $base64
Write-Host "Keystore base64 copied to clipboard! Paste into ANDROID_KEYSTORE_BASE64 secret."

# 2. Read password without trailing newline for ANDROID_KEYSTORE_PASS
$pass = (Get-Content -Raw -Encoding ASCII 'G:\Secrets\Android\chernobyl-release.pass').Trim()
Set-Clipboard -Value $pass
Write-Host "Keystore password copied to clipboard! Paste into ANDROID_KEYSTORE_PASS secret."
```

In CI, `android.yml` writes the decoded keystore to a temporary runner directory, writes the password with `printf "%s"` (no trailing newline) into a `.pass` file beside the `.jks`, and sets `CHB_KEYSTORE`. Secrets are never echoed to job logs.

---

## Running the Same Gates Locally

To run the exact same validation gates that CI executes:

### Full TypeScript Suite
```powershell
npm run verify
```
This runs typecheck across all workspaces, ESLint on mobile, all node:test suites, builds domain/protocol/e2e/host packages, and builds the Android JS export.

### Companion Rust Runtime Suite
```powershell
npm run verify:rust
```
This runs `cargo test`, `cargo clippy -D warnings`, `cargo fmt --check`, and the binary's built-in `--self-test`.

### E2E Interoperability Suite
```powershell
npm run verify:e2e
```
This builds `@chernobyl/e2e` and executes the live companion interop test (`services/companion-rs/tools/e2e-interop.mjs`).
