# Xbox PC (GDK) games on macOS (Apple Silicon) with Xodus

Status log of getting an Xbox PC / Microsoft Store **GDK** title — **No Man's Sky**
(Store id `BQVQTL3PCH05`, MSIXVC, ~31 GiB) — to run natively-translated on an
M4 MacBook Pro (macOS 27.0.1) using Xodus + a self-built CrossOver/Wine engine.

**Current stage (2026-10-13): playable.** The game downloads, decrypts, boots, renders
through Vulkan → MoltenVK → Metal, signs into Xbox Live, creates a save and loads onto a
planet on an M4 / macOS 27. Remaining work: performance, cloud save sync, upscaling.

> Unofficial and experimental. You need a legitimate license for the game on the
> Microsoft account you log in with. Nothing here bypasses licensing: decryption uses
> the content key from your own license, obtained by Xodus.

---

## Architecture

```
xodus-ui / xodus-cli  ──(MSA/Xbox auth, license, MSIXVC download)──▶ Microsoft / Xbox Live
        │
        │  streaming → encrypted install in ~/Games/xodus/<Game>
        │  run + XODUS_DUMP_DIR → decrypted .exe files
        ▼
wine-cx-sikarugir engine (Wine 11.0 from CrossOver 26.3 sources, x86_64 under Rosetta)
  ├─ winevulkan ─▶ MoltenVK ─▶ Metal          (game renderer is Vulkan)
  └─ xgameruntime.dll (builtin, from xodus-gaming/xgameruntime PR #28)
         └─ AF_UNIX socket /tmp/xodus.sock ─▶ xodus-service (Xbox Live tokens)
```

Repositories involved:

| Repo | Branch / state | What |
|---|---|---|
| this fork of `xodus-gaming/xodus` | `macos` | owned-games API, `xodus-ui`, CLI progress JSON, decrypt-dump hack |
| `diacunap/wine-cx-sikarugir` | `main` + `patches/0002..0005` | Sikarugir engine built from CrossOver 26.3 sources |
| `xodus-gaming/xgameruntime` | PR #28 (`AnnoyedDev:xgamesave-cloud`, head `3fd4ae1`) | XUser / XStore / XGameSave / XNetworking for Wine |
| `xodus-gaming/wine` | `bleeding-edge` | source of the AF_UNIX patch (MR 7650) and the `windows.web` JSON commits |

---

## What was changed in this fork

Commits on `macos` (on top of upstream `53ea781`):

1. **`xodus: add owned games and PC Game Pass catalog API`** — `crates/xodus/src/api/collections.rs`
   - Owned games: `POST https://collections.mp.microsoft.com/v7.0/collections/query`,
     `Authorization` = device MSA token for `www.microsoft.com` (same pattern as
     `licensing/content.rs`), beneficiary = user MSA token + puid,
     `productKinds:["Game"]`, `validityType:"Valid"`, paged via `continuationToken`.
     XSTS (`XBL3.0`) auth returns 200 with `{"items":[]}` — don't use it.
   - Game Pass PC catalog: public sigl `609d944c-d395-4c0a-9ea4-e9f39b52c1ad` ("All PC games", incl. EA Play).
   - Manual test: `cargo run --release -p xodus --example owned`.
2. **`cli: JSON progress for streaming, dump decrypted files in run`**
   - `XODUS_PROGRESS_JSON=1 xodus-cli streaming …` prints `{"done","total","status"}` lines instead of indicatif.
   - **Local hack:** `XODUS_DUMP_DIR=<dir> xodus-cli run <install> <anything>` writes the decrypted
     `keep_encrypted` files (for NMS only `Binaries/NMS.exe`) to `<dir>` instead of launching
     xodus-wine. Needed because stock/CrossOver Wine lacks the `WINE_DLL_FILE_MAP` ntdll patch.
     Upstream PR #208 (`run --materialize`, Linux only) is the proper version of this.
3. **`xodus-ui: desktop library browser and download manager`** — `crates/xodus-ui`
   - tao/wry webview app: My library (owned), Game Pass PC, Store search, Downloads
     (queue driving `xodus-cli streaming`, progress/ETA, cancel/restart), Installed scan.
   - Build/bundle: `./scripts/bundle-macos.sh` → `target/Xodus.app` (ad-hoc signed).

## What was changed in wine-cx-sikarugir

| Patch | Why |
|---|---|
| `0001-ntdll-prepend-renderer-dll-paths` | pre-existing (Sikarugir renderer dirs) |
| `0002-winevulkan-mask-unsupported-features` | NMS requests `wideLines` (VkPhysicalDeviceFeatures field 15), MoltenVK rejects it with `VK_ERROR_FEATURE_NOT_PRESENT`, the game ignores the error and asserts in `vkGetDeviceQueue`. Opt-in: `WINE_VK_MASK_UNSUPPORTED_FEATURES=1` drops unsupported core feature bits in `vkCreateDevice` (pEnabledFeatures and `VkPhysicalDeviceFeatures2` in pNext). |
| `0003-afunix-sockets-wine-mr-7650` | AF_UNIX sockets in ntdll/ws2_32/wineserver, required for `xgameruntime` ↔ `xodus-service`. From `xodus-gaming/wine` `6b7313c1bd`. |
| `0004-xgameruntime-from-xodus-pr28` | Adds `dlls/xgameruntime` (xgameruntime PR #28, `legacy-c` layout) as a Wine builtin + `configure.ac` entry. |
| `0006-xgameruntime-xuser-state-and-privileges` | `XUserGetState`=SignedIn, `GetIsGuest`, `GetAgeGroup`, `CheckPrivilege` grants (were `E_NOTIMPL`; game stayed on the title screen after sign-in). |
| `0007-xgameruntime-xgamesavefiles-local-folder` | `XGameSaveFilesGetFolderWithUiAsync/Result` + quota → `%LOCALAPPDATA%\XodusGameSave\<xuid>\<scid>\files` (local only, no cloud sync yet). |
| `0008-kernelbase-mirror-fiberdata-into-gs-on-macos` | **macOS-specific, generic fix.** MSVC `GetCurrentFiber()` reads `%gs:0x20`; on macOS `%gs` is the pthread TSD base and slot 4 is the QoS class, so fiber job systems switch into garbage (`SwitchToFiber(0x8ff)`). Mirrors FiberData into `%gs:0x20` (opt-out `WINE_FIBER_GS_MIRROR=0`). |
| `0005-windows-web-json-parsing-from-xodus-wine` | `Windows.Data.Json` object/array parsing (13 commits by Olivia Ryan / Rémi Bernon from `xodus-gaming/wine` `bleeding-edge`). Needed by `xgameruntime`'s `XUser` to read Xbox Live title endpoints. |

`config.sh`: `ENGINE_REV=4` → `dist/WS12WineCX26.3_4.tar.xz`.
`scripts/build-wine.sh` now fails the build if `winevulkan.dll` lacks the masking patch or
`xgameruntime.dll` is missing.

---

## Reproduce (from zero)

Prereqs: Apple Silicon, Rosetta 2, Homebrew, Xcode CLT; `brew install rust protobuf mingw-w64 cmake ninja`
(plus the wine-cx-sikarugir build deps listed in its README). ~100 GB free disk.

```sh
# 1. Xodus
git clone <this fork> xodus && cd xodus && git checkout macos
cargo build --release -p xodus-cli -p xodus-service
./target/release/xodus-cli login            # sign in with your Microsoft account in the webview
./scripts/bundle-macos.sh && open target/Xodus.app   # optional UI

# 2. Download the game (no resume: don't interrupt; ~31 GiB for NMS)
mkdir -p ~/Games/xodus/NoMansSky
caffeinate -i ./target/release/xodus-cli streaming BQVQTL3PCH05 ~/Games/xodus/NoMansSky

# 3. Decrypt the executables (writes Binaries/NMS.exe, 88.8 MB, starts with "MZ")
XODUS_DUMP_DIR=$HOME/Games/xodus/NoMansSky-decrypted \
  ./target/release/xodus-cli run ~/Games/xodus/NoMansSky /usr/bin/false

# 4. Engine (rev4 = patches 0001-0004; see wine-cx-sikarugir README for the toolchain)
cd ~/Developer/personal/wine-cx-sikarugir && ./build.sh
mkdir -p ~/Games/xodus/engines/r4
tar -xJf dist/WS12WineCX26.3_4.tar.xz -C ~/Games/xodus/engines/r4

# 5. Run (creates prefix, APFS-clones the install, starts xodus-service, launches)
~/Developer/xodus/scripts/macos/run-nms.sh
```

`scripts/macos/run-nms.sh` sets:
`WINEDLLOVERRIDES="xgameruntime=b;vulkan-1=b"` (use Wine's Vulkan loader, not the game's
bundled `vulkan-1.dll`), `WINE_VK_MASK_UNSUPPORTED_FEATURES=1`, `XODUS_SOCKET=/tmp/xodus.sock`,
`WINEDEBUG=+loaddll,err+all,fixme+all,warn+vulkan,+xgameruntime,+gdkc`.
Logs: `~/Games/xodus/logs/` (`nms-wine.log`, `xodus-service.log`) and the game's own
`NoMansSky-run/GAMEDATA/FullLog.txt`.

---

## Timeline of blockers (and fixes)

| # | Symptom | Cause | Fix |
|---|---|---|---|
| 1 | `.exe` won't run under normal Wine | MSIXVC keeps executables encrypted on disk | `XODUS_DUMP_DIR` hack in `run` |
| 2 | Assertion `!status && "vkGetDeviceQueue"` right after start | `wideLines` unsupported by MoltenVK; game ignores `vkCreateDevice` failure | patch 0002 |
| 3 | Hello Games logo, then page fault in `xcurl.dll` → `ntdll` | `xgameruntime` from `main` only implements XThreading; XNetworking etc. return `E_NOINTERFACE` | builtin xgameruntime from PR #28 (patch 0004) + AF_UNIX (0003) |
| 4 | Silent exit code 1 before `xgameruntime` loads | `system32\xgameruntime.dll` missing (GDK loads it from system32; it had been deleted from the prefix) | copy the engine's builtin into `system32` (done in run script) |
| 5 | Title screen, input ignored after "Press A" | `XUserAddAsync` → `load_endpoints` → `Windows.Data.Json` parse fails (`Object parsing not implemented!`) → `E_NOTIMPL`; error dialog is a stub | patch 0005 |
| 6 | Signed in, still stuck on title | `XUserGetState` / `XUserCheckPrivilege` stubs | patch 0006 |
| 7 | Endless red loading ring | `XGameSaveFilesGetFolderWithUiAsync` stub | patch 0007 |
| 8 | Very slow; `wineserver` ~65% CPU, game ~1 core | no fast sync | `WINEMSYNC=1` (run script) |
| 9 | `SwitchToFiber` page fault on `0x8ff` in a `Background` thread; planet never loads | inline `GetCurrentFiber()` reads macOS QoS TSD slot via `%gs:0x20` | patch 0008 |

### Notes on blocker 5 (historical)

CrossOver 26.3's `dlls/windows.web` only parses JSON scalars. `xodus-gaming/wine`
`bleeding-edge` has the full implementation (13 commits by Olivia Ryan / Rémi Bernon,
touching only `dlls/windows.web/`: `2ff9cdec22` … `83aba5c4a3`). Plan:

1. Replace `dlls/windows.web/{Makefile.in,classes.idl,json_array.c,json_object.c,json_value.c,main.c,private.h,windows.web.spec}`
   with the fork's versions; save as `patches/0005-…`.
2. Build only that DLL: `make -C build/wine-x86_64 dlls/windows.web/x86_64-windows/windows.web.dll`.
3. Drop it over `engines/r4/wswine.bundle/lib/wine/x86_64-windows/` and the prefix's `system32`.

Expected next: `XUser` asks `xodus-service` for RPS tickets (`MsaTokenRequest`) → device auth →
SISU → XSTS. Possible further blockers there; `XGameUiShowErrorDialogAsync` is a stub, so
check the `gdkc` channel in the Wine log rather than the screen.

---

## Gotchas

- **Keychain prompts:** ad-hoc signed binaries are pinned by cdhash, so every rebuild of
  `xodus-cli`/`xodus-service`/`xodus-ui` asks again — choose "Always Allow".
  Don't sign with an *Apple Development* identity: Gatekeeper then flags the app as possible malware.
- **`xodus-service` waits on the Keychain** before creating `/tmp/xodus.sock`; if the game starts
  first, sign-in has no backend. Start the service and approve the prompt before launching.
- **Device group full:** Microsoft limits devices per account; new licenses fail with
  "Device group is full" until you remove a device at account.microsoft.com/devices.
- **`xodus-cli streaming` exits 0 on failure** (upstream PR #207). `xodus-ui` checks the
  `.xodus-streaming.msixvc` marker and the tmp file instead of trusting the exit code.
- **Streaming has no resume** (PR #186 covers `download`, not `streaming`).
- `cat` may be aliased (e.g. to `bat`) in interactive shells; use `/bin/cat` in scripts.
- Rosetta: Apple has said general Rosetta support ends after macOS 27; this stack depends on it.

## Upstream PRs worth tracking

`xodus#216` (TokenAndSignature/UserInfo in xodus-service), `xodus#207` (exit codes),
`xodus#196`/`#197` (macOS run fixes), `xodus#186` (resume), `xodus#208` (materialized launch),
`xgameruntime#28` (XUser/XGameSave), `xgameruntime#18` (Windows-native IPC).
