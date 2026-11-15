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

- **`kitkat` job** (`ubuntu-latest`):
  Runs unit tests for the native Android 4.4 KitKat client (`android-kitkat/`):
  * Sets up JDK 17 (Temurin) with Gradle caching.
  * Sets up Android SDK Platform 36 and Build-Tools 36.0.0.
  * Makes `gradlew` executable (`chmod +x`).
  * Runs `./gradlew testDebugUnitTest --console=plain` in `android-kitkat/`.

- **`setup-script` job** (`windows-latest`):
  Depends on the `rust` job passing (`needs: [rust]`). Verifies the Windows setup script (`scripts/setup-windows.ps1`) without touching real user directories:
  * Builds the companion in release mode (`cargo build --release --manifest-path services/companion-rs/Cargo.toml`).
  * Runs `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-setup-windows.ps1 -Exe services/companion-rs/target/release/chernobyl-companion.exe`.
  * **Note on Scope**: `test-setup-windows.ps1` runs `setup-windows.ps1` in `-ConfigOnly` mode with redirected temp directories (`APPDATA` / `LOCALAPPDATA`) and validates generated configurations against the companion daemon's CLI parser (`--config <path> --devices`). It does not modify persistent system configuration, require administrative privileges, or depend on external runtimes (`agy`, `codex`, `claude`) or tunnels being installed on the runner. The test script exercises configuration generation and syntax verification; it does not support skip environment variables.

- **Security, Hardening & Concurrency**:
  * **Pinned Action SHAs**: Every GitHub Action is pinned to an immutable full commit SHA with the version tag as a trailing comment (e.g. `actions/checkout@11d5960... # v4`, `dtolnay/rust-toolchain` pinned with `toolchain: 1.99.0`). Dependabot tracks and updates these SHAs weekly via `.github/dependabot.yml`.
  * **Credential Isolation (`persist-credentials: false`)**: Every checkout step explicitly specifies `persist-credentials: false`, preventing runner tokens from lingering in `.git/config` where build scripts or third-party dependencies could exfiltrate them.
  * **Least Privilege**: Global workflow permissions are restricted to `contents: read`. Elevated write permissions (`contents: write`) are granted only to the draft release publication job in `release.yml`.
  * **Concurrency Controls**: Concurrency groups are keyed by workflow and ref (`${{ github.workflow }}-${{ github.ref }}`) with `cancel-in-progress: true` to prevent redundant runner usage.

---

### 2. `android.yml` — Android APK Build & Signing

Triggered on demand via `workflow_dispatch` and automatically on release tags matching `v*`.

- Runs on `ubuntu-latest` within the `release` environment (`environment: release`).
- Sets up JDK 17 (Temurin) and Android SDK components via `sdkmanager`:
  * Android Platform 36 (`platforms;android-36`)
  * Build-Tools 36.0.0 (`build-tools;36.0.0`)
  * NDK 27.1.12297006 (`ndk;27.1.12297006`)
  * CMake 3.31.6 (`cmake;3.31.6`)
- **Build-Then-Sign Isolation**:
  * Build executes first without keystore secrets on disk: `npm ci` and `node scripts/build-android.mjs` run while `CHB_KEYSTORE` is completely unset, producing a debug-signed APK. Untrusted npm dependencies and build hooks never run with access to the release signing key.
  * Dedicated Signing Step: Once the build finishes, a separate signing step runs only when signing secrets are provided. It writes the keystore to `$RUNNER_TEMP/keystore/chernobyl-release.jks`, writes the password via `printf '%s'` into `chernobyl-release.pass`, and registers a bash exit trap (`trap 'rm -rf "$RUNNER_TEMP/keystore"' EXIT`) ensuring cryptographic material is removed immediately upon step completion or error.
  * In-place Signing & Toolchain Integration: Calls `CHB_KEYSTORE="$JKS_PATH" node scripts/build-android.mjs --sign-only <apk>` to sign with production parameters.
  * **Certificate Digest Verification**: Immediately after signing, executes `apksigner verify --print-certs <apk>` and verifies that the output contains the pinned release certificate SHA-256 digest (`c8cc22bdfb6c244447d2a5beb007f8fd6adcd20fb8632b0e5998c6b96ead7d36`). Any digest mismatch immediately fails the workflow.
  * **Fail on Tag Without Secrets**: On release tags (`startsWith(github.ref, 'refs/tags/')`), missing signing secrets immediately FAIL the job, preventing unverified or debug-signed binaries from ever reaching release status. On manual `workflow_dispatch` or branch runs, missing secrets fall back to the debug-signed APK with a logged notice.
- Produces the release APK (`arm64-v8a` + `x86_64`) into `release/` and uploads it as an artifact (`chernobyl-apk`).

---

### 3. `release.yml` — Companion Binary, KitKat APK & Draft Release

Triggered automatically on release tags matching `v*`.

- **`kitkat-apk` job** (`ubuntu-latest`):
  * Runs within the `release` environment (`environment: release`).
  * Sets up JDK 17 (Temurin) with Gradle caching and Android SDK (Platform 36, Build-Tools 36.0.0). Pinned Gradle wrapper binary integrity is verified by SHA-256 checksum in `gradle-wrapper.properties`.
  * **Build-Then-Sign Order**: Builds the Android 4.4 KitKat client release APK (`./gradlew assembleRelease --console=plain`) with no signing secrets or keystores present on disk.
  * **Dedicated Signing Step**: Decodes keystore secrets into `$RUNNER_TEMP/keystore/`, writes the password file, sets `trap 'rm -rf "$RUNNER_TEMP/keystore"' EXIT`, signs via `apksigner` (v1 JAR + v2 signature, min SDK 19), and verifies the signer certificate matches the pinned SHA-256 digest `c8cc22bdfb6c244447d2a5beb007f8fd6adcd20fb8632b0e5998c6b96ead7d36`.
  * **Fail on Tag Without Secrets**: When triggered by a tag, missing signing secrets cause an immediate hard failure (`exit 1`).
  * Uploads the APK artifact (`chernobyl-kitkat-apk`).
- **`release` job** (`windows-latest`, `needs: [kitkat-apk]`):
  * Downloads the built KitKat APK into `release/`.
  * Builds the optimized release companion binary:
    `cargo build --release --manifest-path services/companion-rs/Cargo.toml`
  * **No npm in Release Job**: Runs `node scripts/package-release.mjs` directly without running `npm ci`. Because `package-release.mjs` strictly imports Node standard library built-ins (`node:*`), dependency installation is completely omitted.
  * Calculates standard SHA-256 checksums across all release artifacts (`chernobyl-companion.exe` and `release/*.apk`).
  * Publishes a draft GitHub Release using `softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65 # v2` with `contents: write` permissions (the only job with write permissions), attaching:
    * `services/companion-rs/target/release/chernobyl-companion.exe`
    * `release/*.apk` (including the KitKat APK)
    * `release/SHA256SUMS.txt`
  * The modern Android APK artifact built concurrently by `android.yml` can also be attached to this draft release prior to publication.

---

### 4. `dependabot.yml` — Dependency Maintenance

Checks GitHub Actions workflows on a weekly cadence for updates.

---

## Android Signing Secrets & `release` Environment

By default on non-tag builds (e.g. `workflow_dispatch` on feature branches), CI produces a debug-signed APK that can be installed on testing devices. For production releases (triggered by tags matching `v*`), signing with the owner's release key is mandatory: missing secrets will immediately **FAIL** the build to prevent publishing debug-signed APKs.

### GitHub `release` Environment Setup

The repository owner must create a protected GitHub environment named `release`:

1. Navigate to **Repository Settings → Environments → New environment**.
2. Name the environment `release`.
3. Configure protection rules (ideally set **Required reviewers** so deployments and secret access require manual authorization).
4. Add the signing secrets under **Environment secrets**:
   * `ANDROID_KEYSTORE_BASE64`: The base64-encoded content of your `.jks` keystore file.
   * `ANDROID_KEYSTORE_PASS`: The password string stored in your `.pass` file.

### Build-Then-Sign Order & Cryptographic Hygiene

In CI (`android.yml` and `release.yml` `kitkat-apk`), signing material is guarded by strict isolation:
- **Zero secrets during build**: Dependency code (`npm ci`, Expo prebuild, Gradle tasks) executes while the keystore does not exist on runner storage.
- **Dedicated signing step**: Only after compilation completes does the runner decode the keystore to `$RUNNER_TEMP/keystore/chernobyl-release.jks` and write the password via `printf '%s'` into `chernobyl-release.pass`.
- **Guaranteed cleanup**: A bash exit trap (`trap 'rm -rf "$RUNNER_TEMP/keystore"' EXIT`) ensures the keystore and password files are deleted immediately when the step finishes or fails.
- **Signer verification**: `apksigner verify --print-certs` verifies the APK signature and checks that the certificate SHA-256 digest matches the pinned release certificate `c8cc22bdfb6c244447d2a5beb007f8fd6adcd20fb8632b0e5998c6b96ead7d36`. If the digest does not match, the step terminates with an error.

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

In CI, `android.yml` and `release.yml` decode the keystore in a dedicated post-build step into `$RUNNER_TEMP/keystore`, write the password with `printf "%s"` (no trailing newline) into a `.pass` file beside the `.jks`, register a cleanup exit trap, and execute the signing command. Secrets are never echoed to job logs.

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
