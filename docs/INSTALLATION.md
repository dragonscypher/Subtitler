# Installing the Subtitler Engine

**Status:** installation design and development guidance. The current repository has a buildable foundation; it does **not** yet ship a signed end-user installer, a registered native host, or bundled model/FFmpeg assets. A narrowly scoped Windows developer-registration script is available for local testing, but it is not an installer and must not be presented as one. This document specifies the release behavior that the packaging phase must implement.

## What gets installed

Subtitler is two separately installed pieces:

```text
Chrome extension
    detects media, renders overlay, and connects to
Subtitler Engine
    native host + durable local engine + optional models/decoder
```

The extension can be installed independently, but local transcription requires the engine. If Chrome cannot find or connect to the native host, the popup shows:

```text
Subtitler needs its local processing engine.
[ Install Subtitler Engine ]
```

The extension must never attempt to install executables, write registry keys, elevate privileges, or download a model without a user-visible installation action.

## Supported release targets

| Target | V1 release intent | Native-host registration |
| --- | --- | --- |
| Google Chrome on Windows (x64/ARM64 packages where supported) | primary packaged target | per-user `HKCU` Chrome Native Messaging registry key |
| Google Chrome on macOS | packaged after signing/notarization validation | per-user Chrome `NativeMessagingHosts` directory |
| Chromium-family browsers | design-compatible; enable only after a browser-specific registration and test pass | browser-specific host location/identity |
| Linux | development/early-access target until package and desktop-matrix support is complete | browser-specific user configuration directory |

Chrome's native-host discovery is browser- and operating-system-specific. On Windows the installer registers a manifest path under the Chrome Native Messaging registry key; on macOS/Linux Chrome reads a browser-specific `NativeMessagingHosts` location. The native-host manifest allowlists exact extension origins and cannot use wildcards. [Chrome Native Messaging](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)

## End-user installation flow

1. The user installs the signed Subtitler extension from its distribution channel.
2. On the first local job, the extension probes `com.subtitler.native_host` using Native Messaging.
3. If absent, the user chooses **Install Subtitler Engine**. The extension opens the signed engine installer destination; it does not invoke a shell command or silent download.
4. The installer places engine binaries, the native-host binary, host manifest, license notices, and an uninstaller in an installer-owned per-user location. It registers the host for the current user.
5. The popup retries its capability handshake. It shows the engine version and whether a local model needs downloading.
6. On the first model choice, the engine displays the model size, expected disk use, and checksum-verified source. Model download is cancellable and never blocks extension use of existing captions.

The normal first-run path has no Subtitler account, API key, or transcription subscription. A cloud provider appears only if the user explicitly selects it for a job.

## Windows installer requirements

The Windows package should be a code-signed per-user installer (for example, a WiX-produced MSI or an equivalent signed installer) and must not require administrator privileges for normal use. It installs under a fixed per-user application directory such as `%LOCALAPPDATA%\Subtitler\` with user-only ACLs.

It must install these owned items:

```text
<install-root>/
  bin/subtitler-native-host.exe
  bin/subtitler-engine.exe             # added when durable engine phase lands
  bin/ffmpeg.exe and bin/ffprobe.exe   # only in a licensed bundled build
  native-host-manifest.json
  licenses/
  uninstall metadata

<user-data-root>/
  models/
  jobs.sqlite
  cache/
  logs/                                # redacted diagnostics only
```

The installer writes the current-user registration only:

```text
HKCU\Software\Google\Chrome\NativeMessagingHosts\com.subtitler.native_host
    (Default) = <absolute path to native-host-manifest.json>
```

The manifest's `path` is installer-owned, `type` is `stdio`, and `allowed_origins` contains the exact production Chrome extension ID. Its name is `com.subtitler.native_host`. A release build must fail packaging if the extension ID is missing, malformed, or differs from the extension package being distributed. Chrome documents the Windows registry discovery flow and checks both 32-bit and 64-bit registry views. [Chrome host registration](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)

Do not register a system-wide host by default. Enterprise deployment may use an administrator-managed `HKLM` package only with a separate documented policy and uninstall path.

## macOS and Linux packaging requirements

macOS distribution uses a signed and notarized package. The installer owns the engine and native-host binary, writes a user-level manifest in Chrome's documented `NativeMessagingHosts` directory, and removes that manifest during uninstall. The package must tolerate the user moving an application bundle by using a stable installed location or an installer-managed launcher, not a fragile relative path.

Linux support must package browser-specific manifests for the tested browser and honor its native-host discovery location. A `.deb`/`.rpm` package may install a system host only if its package manager ownership and permissions are correct; a user-level installer remains the safer default. Do not claim generic Chromium compatibility until the exact browser channel and manifest location are tested.

## Development setup

The current implementation can be built locally with the prerequisites in the repository README:

- Node.js 20 or later.
- Rust stable and MSVC build tools on Windows.
- FFmpeg, a compatible whisper.cpp CLI, and a local model only when exercising actual media extraction/transcription; deterministic unit tests do not need them.

From the repository root:

```powershell
npm --prefix extension install
npm --prefix extension run validate

cargo test --manifest-path native/Cargo.toml --workspace
cargo build --manifest-path native/Cargo.toml --release -p subtitler-native-host
```

Load `extension/dist` as an unpacked extension through Chrome's extension developer page after a successful extension build. Its extension ID is development-specific. A development-only host-manifest generation step must use that exact ID and must write to a disposable per-user developer registration; production manifests must never wildcard an extension ID. Native-host registration is intentionally an installer/developer-script responsibility, not an extension responsibility.

### Windows developer native-host registration

`scripts/register-native-host.ps1` and `scripts/unregister-native-host.ps1` support a deliberately narrow, per-user **development** workflow. They do not download, copy, sign, or execute a binary; they do not write `HKLM`; and they do not substitute for the future signed installer.

The default manifest directory is:

```text
%LOCALAPPDATA%\Subtitler\developer\native-messaging\
```

The scripts accept only a child of `%LOCALAPPDATA%\Subtitler\developer\` as `-InstallDirectory`. They also require a current host `.exe` on a ready, fixed local drive. UNC shares, mapped network drives, device paths, alternate data streams, relative paths, and reparse-point paths are rejected. This keeps developer registration separate from a packaged installation and prevents a manifest from resolving through an unexpected location.

1. Build the native host, load the unpacked extension, and copy the test host to a fixed local developer location if the build output is not already on one. Copying is an explicit developer step; the registration script never performs it.

   ```powershell
   $developerRoot = Join-Path $env:LOCALAPPDATA 'Subtitler\developer'
   $hostDirectory = Join-Path $developerRoot 'bin'
   New-Item -ItemType Directory -Force -Path $hostDirectory | Out-Null
   Copy-Item .\native\target\release\subtitler-native-host.exe `
       (Join-Path $hostDirectory 'subtitler-native-host.exe') -Force
   ```

2. Copy the exact unpacked extension ID from `chrome://extensions` (Developer mode), then run a dry run first. The ID must be 32 lower-case characters in the Chrome `a`–`p` alphabet.

   ```powershell
   $extensionId = '<exact ID from chrome://extensions>'
   $hostExecutable = Join-Path $env:LOCALAPPDATA 'Subtitler\developer\bin\subtitler-native-host.exe'

   .\scripts\register-native-host.ps1 `
       -ExtensionId $extensionId `
       -HostExecutable $hostExecutable `
       -WhatIf
   ```

   `-WhatIf` performs the same input, ownership, and existing-registration validation without creating a directory, manifest, or registry key. After reviewing the planned targets, omit `-WhatIf` to register the host.

3. The actual command writes only the exact current-user Chrome key:

   ```text
   HKCU\Software\Google\Chrome\NativeMessagingHosts\com.subtitler.native_host
   ```

   The manifest has the hard-coded name `com.subtitler.native_host`, type `stdio`, and one exact `chrome-extension://<extension-id>/` origin. It is written to a random same-directory temporary file, ACL-hardened for the current user, atomically published, parsed back, and validated before the registry value is written. If the current key points somewhere else or contains unexpected values/subkeys, registration fails closed rather than replacing it.

4. To remove only that development registration, use the same extension ID and begin with another dry run:

   ```powershell
   .\scripts\unregister-native-host.ps1 `
       -ExtensionId $extensionId `
       -WhatIf

   # After confirming the exact key and manifest targets:
   .\scripts\unregister-native-host.ps1 `
       -ExtensionId $extensionId
   ```

   Unregistration requires the registry default value to point to the exact default developer manifest, then validates the manifest's host name, property set, local executable path, and exact extension origin again. It removes only that `HKCU` host key and manifest. It intentionally does **not** remove the executable or developer directory. Missing, tampered, or differently located manifests/registrations are refused for manual inspection rather than guessed at or deleted.

#### Developer validation and manual smoke test

Run the deterministic registration validation before using the scripts:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\test-native-host-registration.ps1
```

It has no Pester dependency and never writes to the registry. It creates a GUID-named fixture below the developer-owned root, verifies the unsafe-path and extension-ID rejections, exercises manifest creation plus atomic replacement and ACL hardening, verifies a malformed manifest is rejected, and removes only its own fixture. `scripts/verify.ps1` runs the same check as part of native verification.

For a manual Chrome check after actual registration:

```powershell
$registryPath = 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.subtitler.native_host'
$manifestPath = (Get-Item -LiteralPath $registryPath).GetValue('')
$manifestPath
Get-Content -Raw -LiteralPath $manifestPath
```

Confirm that the default value is the expected developer manifest and that `allowed_origins` contains only the unpacked extension's exact origin. Reload the unpacked extension and use its native-host connection path. Then run the unregistration dry run and actual command above; querying `$registryPath` should no longer find a key. Do not test registration against a production or another product's native-host key.

For development-only direct/local transcription, the installer-owned default layout is `%LOCALAPPDATA%\Subtitler\developer\tools`: `ffmpeg\ffmpeg.exe`, `whisper\whisper-cli.exe`, and `models\ggml-base.en.bin`. Its known base-English asset may run locally only with at least 1 GiB available memory and two logical CPUs; the popup still reports the coarse performance advisory and never enables cloud processing itself. `SUBTITLER_FFMPEG_PATH`, `SUBTITLER_WHISPER_CPP_PATH`, and `SUBTITLER_WHISPER_MODEL_PATH` can replace those defaults for development. Any replacement model must set `SUBTITLER_LOCAL_MODEL`, `SUBTITLER_MODEL_QUANTIZATION`, and `SUBTITLER_COMPUTE_BACKEND` together as one validated advanced triple, preventing an arbitrary path from being mislabeled as an automatically selected model. `SUBTITLER_DENO_PATH` optionally overrides `%LOCALAPPDATA%\Subtitler\developer\tools\deno\deno.exe`, used only by the isolated YouTube `yt-dlp`/EJS adapter. The optional `SUBTITLER_COMPILED_BACKENDS` declaration records installer/developer-known compiled accelerators such as `cuda` or `metal`; it is not GPU discovery and an omitted declaration means CPU only. `SUBTITLER_CACHE_ROOT` and `SUBTITLER_EXPORT_ROOT` can redirect test-only output. The host stores temporary audio beneath the local Subtitler data directory and writes exports there by default. These variables are read by the native host only—not from page or extension messages—and are a development bridge, not a release installation contract. A release model manager may use the automatic hardware plan only after it owns a signed/verified model-asset mapping.

The optional, installer-owned YouTube proof-token layout keeps the maintained provider outside the repository: a `bgutil-ytdlp-pot-provider` release zip beneath `yt-dlp\plugins` and the matching provider server beneath `youtube-pot-provider\server`. `SUBTITLER_YTDLP_POT_PLUGIN_DIR` and `SUBTITLER_YTDLP_POT_SERVER_HOME` can replace those exact directories for development. When both are present, the native adapter supplies them through yt-dlp's plugin API and runs the existing Deno runtime; it does not import Chrome cookies, profiles, static tokens, or captions. Each provider invocation receives an `XDG_CACHE_HOME` inside the job's private RAII media directory, so any short-lived token cache is removed together with temporary audio after processing.

The current native host can run a generic in-process job when those assets are configured. It must not be represented as a fully installed local ASR product until the signed installer, model manager, durable engine, packaged decoder, and real-media phase tests have passed.

## Install, update, repair, and uninstall behavior

### Update

- The installer verifies the signature of the update before replacing binaries.
- It stops/restarts the per-user engine at a safe point and performs transactional database migrations with a backup.
- A native host and extension exchange protocol versions during `engine.hello`; an incompatible pair shows an update/repair action, not a cryptic pipe error.
- Models are versioned by manifest hash. An update never silently removes a valid installed model until it has verified that no retained job requires it.

### Repair

The extension may diagnose only safe states: host missing, host manifest not registered, version mismatch, engine launch failure, or unavailable local model. It can open the signed installer/repair page but does not attempt to write native-host registration itself.

### Uninstall

The engine uninstaller removes its native-host registration, binaries, and installer-owned manifests. It asks whether to remove retained transcripts/models/cache; temporary audio is removed by default. Removing the extension alone does not automatically delete local results or the engine, because doing so could destroy user data without a clear engine-uninstall action.

## Data locations, retention, and permissions

- Installed executables and native-host manifest are writable only by the installer/user account.
- Model files are checksum-verified before activation and reside in the app-private data directory, not a browser extension directory.
- Job metadata, retained results, and cache are user-private. Temporary media/audio is job-scoped and deleted at completion/cancel/failure unless the user opted to keep it.
- API keys and the local engine handshake secret use OS protected storage. They do not appear in registry values, command lines, extension storage, support bundles, or logs.
- The engine listens on a user-private named pipe/Unix socket only. It does not bind a localhost TCP port as part of normal installation.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the target process topology and [SECURITY.md](SECURITY.md) for threat-model and support-data rules.
