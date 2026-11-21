#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"
dist="${CEF_PATH:-/Users/Shared/gups-cef}/154.0.34/cef_macos_aarch64"
main=ProxyProbe.app/Contents
mkdir -p "$main/MacOS" "$main/Frameworks"
ln -sfn "$dist/Chromium Embedded Framework.framework" "$main/Frameworks/Chromium Embedded Framework.framework"
for binary in ProxyProbe "ProxyProbe Helper" "ProxyProbe Helper (Renderer)" "ProxyProbe Helper (GPU)"; do
  item="$main"
  if [[ "$binary" != ProxyProbe ]]; then
    item="$main/Frameworks/$binary.app/Contents"
    mkdir -p "$item/MacOS" "$item/Frameworks"
    ln -sfn "$dist/Chromium Embedded Framework.framework" "$item/Frameworks/Chromium Embedded Framework.framework"
  fi
  python3 - "$item" "$binary" <<'PY'
import plistlib
import sys
from pathlib import Path

directory, binary = sys.argv[1:]
with (Path(directory) / "Info.plist").open("wb") as plist:
    plistlib.dump(
        {
            "CFBundleExecutable": binary,
            "CFBundleIdentifier": "dev.gups.tb6." + "".join(c for c in binary.replace(" ", "-") if c.isalnum() or c == "-"),
            "CFBundlePackageType": "APPL",
            "LSBackgroundOnly": True,
        }, plist
    )
PY
done
clang++ -std=c++20 -I"$dist" -F"$dist" \
  -framework AppKit -framework "Chromium Embedded Framework" \
  -Wl,-rpath,@executable_path/../Frameworks \
  -Wl,-rpath,@executable_path/../../.. \
  probe.mm -o "$main/MacOS/ProxyProbe"
for binary in "ProxyProbe Helper" "ProxyProbe Helper (Renderer)" "ProxyProbe Helper (GPU)"; do
  cp "$main/MacOS/ProxyProbe" "$main/Frameworks/$binary.app/Contents/MacOS/$binary"
done
