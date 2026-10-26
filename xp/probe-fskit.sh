#!/bin/bash
# Try to get an FSKit module enabled without the System Settings click, then mount.
# LIB = fuset | macfuse
set -x
X=$(cd "$(dirname "$0")" && pwd)
W=${RUNNER_TEMP:-/tmp}/probe; mkdir -p "$W/b" "$W/m"; echo hi > "$W/b/hello"
pluginkit -mAvvv -p com.apple.fskit.fsmodule
sudo pluginkit -mAvvv -p com.apple.fskit.fsmodule
find /Applications /Library /usr/local -maxdepth 6 -name "*.appex" 2>/dev/null | grep -iv "^/System"
ls -la /Library/Filesystems /Library/ExtensionKit/Extensions /Applications 2>&1 | head -60
for appex in $(find /Applications /Library -maxdepth 6 -name "*.appex" 2>/dev/null | grep -i fuse); do
  codesign -dv --entitlements - "$appex" 2>&1 | head -30
  pluginkit -a "$appex"; sudo pluginkit -a "$appex"
done
open -a fuse-t 2>/dev/null; open -a macFUSE 2>/dev/null; sleep 5
pluginkit -mAvvv -p com.apple.fskit.fsmodule
for id in $(pluginkit -mA -p com.apple.fskit.fsmodule | awk '{print $1}' | sed 's/^[+!=-]*//; s/(.*//'); do
  pluginkit -e use -i "$id"; sudo pluginkit -e use -i "$id"
done
pluginkit -mAvvv -p com.apple.fskit.fsmodule
case $1 in
  fuset) cc -O2 -o "$W/pt" "$X/passthrough.c" -D_FILE_OFFSET_BITS=64 $(pkg-config --cflags --libs fuse-t) ;;
  macfuse) cc -O2 -o "$W/pt" "$X/passthrough.c" -I/usr/local/include/fuse -D_FILE_OFFSET_BITS=64 -L/usr/local/lib -lfuse ;;
esac
"$W/pt" "$W/b" "$W/m" -f -o backend=fskit > "$W/pt.log" 2>&1 &
pid=$!
sleep 10
mount; cat "$W/pt.log"; cat "$W/m/hello"
log show --last 1m --predicate 'process CONTAINS "fskit" OR subsystem CONTAINS "fskit"' 2>/dev/null | tail -40
umount "$W/m"; kill $pid; true
