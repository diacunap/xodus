<p align="center"><img width="128" src="assets/Icon/Icon.ico" /></p>
<h1 align="center">Xodus for macOS</h1>
<p align="center">Play your Xbox PC / Microsoft Store games on Apple Silicon Macs</p>

> [!CAUTION]
> Unofficial and experimental — use at your own risk. Not affiliated with, endorsed by, or
> sponsored by Microsoft, Xbox or Apple. You need a legitimate license for every game on the
> Microsoft account you sign in with; decryption uses the content key from your own license.

This is a macOS-focused fork of [xodus-gaming/xodus](https://github.com/xodus-gaming/xodus)
(which targets Linux). It adds a native Mac app to browse and download the games tied to your
account, and documents how to run them through a self-built CrossOver/Wine engine
(Rosetta 2 + MoltenVK/Metal).

## Status

| | |
|---|---|
| Sign in with your Microsoft account | ✅ |
| Browse your library (owned games) and the PC Game Pass catalog | ✅ `Xodus.app` |
| Search the Microsoft Store, see which games Xodus can handle (MSIXVC) | ✅ |
| Download games (queue, progress, ETA) | ✅ |
| Decrypt executables for Wine | ✅ (local hack, see docs) |
| **No Man's Sky** boots, renders (Vulkan → MoltenVK → Metal) and reaches the title screen | ✅ on M4 / macOS 27 |
| Xbox Live sign-in inside games (`XUser`) | 🚧 blocked on Wine `windows.web` JSON parsing |
| One-click **Play** from the app | 🚧 after sign-in works |
| MSIXVC2 packages | ❌ upstream [#53](https://github.com/xodus-gaming/xodus/issues/53) |

Full log of what was done, every blocker and its fix: **[docs/macos/README.md](docs/macos/README.md)**.

## Requirements

- Apple Silicon Mac, macOS 14+ (developed on macOS 27 / M4), Rosetta 2
- Homebrew: `brew install rust protobuf`
- For running games: a Wine engine built from
  [wine-cx-sikarugir](https://github.com/diacunap/wine-cx-sikarugir) (rev 4+) — see the docs
- Disk space for the games (No Man's Sky ≈ 31 GiB)

## Quick start

```sh
git clone -b local/sikarugir-dump https://github.com/diacunap/xodus.git
cd xodus
./scripts/bundle-macos.sh        # builds xodus-ui + xodus-cli → target/Xodus.app
./target/release/xodus-cli login # sign in once (webview)
open target/Xodus.app
```

In the app: **My library** lists your games, **Supported only** filters to the ones Xodus can
download, the detail panel has **Download**, and **Downloads** shows the queue and installed games.

Keychain: macOS asks the first time each (re)built binary reads your Xodus credentials —
choose **Always Allow**. Downloads can't resume yet, so keep the Mac awake and plugged in while they run.

## Running a game (manual, for now)

```sh
# decrypt the executables of an installed game
XODUS_DUMP_DIR=$HOME/Games/xodus/NoMansSky-decrypted \
  ./target/release/xodus-cli run ~/Games/xodus/NoMansSky /usr/bin/false

# launch with the patched engine + xodus-service (No Man's Sky example)
./scripts/macos/run-nms.sh
```

Step-by-step, engine patches and troubleshooting: [docs/macos/README.md](docs/macos/README.md).

## What this fork adds

| Area | Path |
|---|---|
| Mac app (library, Game Pass, store search, download manager) | `crates/xodus-ui`, `scripts/bundle-macos.sh` |
| Owned games + Game Pass catalog API | `crates/xodus/src/api/collections.rs` |
| Machine-readable download progress (`XODUS_PROGRESS_JSON=1`) | `crates/xodus-cli/src/commands/streaming.rs` |
| Decrypt-to-disk for stock Wine (`XODUS_DUMP_DIR`) | `crates/xodus-cli/src/commands/run.rs` |
| Mac docs and run script | `docs/macos/`, `scripts/macos/` |

## Upstream

Core Xodus (auth, licensing, MSIXVC, xodus-service) is upstream work — build docs, CLI
reference and FAQ are in [docs/upstream-README.md](docs/upstream-README.md).
Community: [Discord](https://discord.gg/ZG774FK4tq).
