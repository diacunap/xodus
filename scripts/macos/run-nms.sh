#!/bin/sh
# Manual No Man's Sky test: wine-cx-sikarugir engine (rev4: builtin xgameruntime from
# xodus PR #28 + AF_UNIX) talking to xodus-service for Xbox Live tokens.
# See docs/macos/README.md. Logs go to ~/Games/xodus/logs/.
set -eu

# Engine: rev3+ of ~/Developer/personal/wine-cx-sikarugir (winevulkan feature masking).
ENGINE="${ENGINE:-$HOME/Games/xodus/engines/r4/wswine.bundle}"
WINE="$ENGINE/bin/wine"
SRC="$HOME/Games/xodus/NoMansSky"                 # encrypted install from xodus-cli streaming
DEC="$HOME/Games/xodus/NoMansSky-decrypted"       # XODUS_DUMP_DIR output
RUN="$HOME/Games/xodus/NoMansSky-run"             # APFS clone + decrypted exe
SERVICE="${SERVICE:-$(cd "$(dirname "$0")/../.." && pwd)/target/release/xodus-service}"
export WINEPREFIX="$HOME/Games/xodus/prefix-nms"
LOGS="$HOME/Games/xodus/logs"
mkdir -p "$LOGS"

if [ ! -d "$RUN" ]; then
  cp -cR "$SRC" "$RUN"                             # copy-on-write clone, instant
  (cd "$DEC" && find . -type f -exec cp {} "$RUN/{}" \;)
fi

if [ ! -d "$WINEPREFIX" ]; then
  WINEDLLOVERRIDES="mscoree,mshtml=" "$WINE" wineboot -u > "$LOGS/wineboot.log" 2>&1
fi
# rev4 ships xgameruntime as a builtin. GDK titles load it from system32, so make sure
# the engine's builtin is there (also replaces the native test copy from earlier runs).
cp "$ENGINE/lib/wine/x86_64-windows/xgameruntime.dll" "$WINEPREFIX/drive_c/windows/system32/xgameruntime.dll"

# Xbox Live tokens for XUser/XGameSave come from xodus-service over /tmp/xodus.sock.
export XODUS_SOCKET=/tmp/xodus.sock
if ! pgrep -f "$SERVICE" >/dev/null; then
  rm -f "$XODUS_SOCKET"
  XODUS_LOG=info "$SERVICE" > "$LOGS/xodus-service.log" 2>&1 &
  for _ in 1 2 3 4 5 6 7 8 9 10; do [ -S "$XODUS_SOCKET" ] && break; sleep 1; done
fi

export WINEDLLOVERRIDES="xgameruntime=b;vulkan-1=b"
export WINE_VK_MASK_UNSUPPORTED_FEATURES=1
# Fast sync: without msync every mutex/event round-trips through wineserver
# (seen as wineserver at ~70% CPU and the game stuck at one core).
export WINEMSYNC="${WINEMSYNC:-1}"
# XODUS_HUD=1: Apple's Metal performance HUD (FPS, frame/GPU time) to measure tuning.
[ "${XODUS_HUD:-0}" = 1 ] && export MTL_HUD_ENABLED=1
if [ "${XODUS_DEBUG:-0}" = 1 ]; then
  export WINEDEBUG="${WINEDEBUG:-+loaddll,err+all,fixme+all,warn+vulkan,+xgameruntime}"
  export MVK_CONFIG_LOG_LEVEL="${MVK_CONFIG_LOG_LEVEL:-3}"
else
  export WINEDEBUG="${WINEDEBUG:--all}"
  export MVK_CONFIG_LOG_LEVEL="${MVK_CONFIG_LOG_LEVEL:-1}"   # errors only; warnings also flood FullLog.txt
fi

cd "$RUN/Binaries"
exec "$WINE" NMS.exe > "$LOGS/nms-wine.log" 2>&1
