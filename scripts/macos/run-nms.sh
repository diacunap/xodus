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
export WINEDEBUG="${WINEDEBUG:-+loaddll,err+all,fixme+all,warn+vulkan,+xgameruntime,+gdkc}"

cd "$RUN/Binaries"
exec "$WINE" NMS.exe > "$LOGS/nms-wine.log" 2>&1
