#!/usr/bin/env bash
# Build mentra-mic.apk with the bare Android SDK tools (no Gradle).
set -euo pipefail
cd "$(dirname "$0")"

SDK="${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}"
BT="$(ls -d "$SDK"/build-tools/36.0.0 "$SDK"/build-tools/34.0.0 2>/dev/null | head -1)"
PLATFORM="$SDK/platforms/android-35/android.jar"
[ -d "$BT" ] || { echo "no build-tools 36.0.0/34.0.0 under $SDK" >&2; exit 1; }
[ -f "$PLATFORM" ] || { echo "missing $PLATFORM" >&2; exit 1; }

# Locate a JDK: honour JAVA_HOME, else system java, else Homebrew/Android Studio.
if [ -z "${JAVA_HOME:-}" ] && ! java -version >/dev/null 2>&1; then
  for c in /opt/homebrew/opt/openjdk@17 /opt/homebrew/opt/openjdk \
           "/Applications/Android Studio.app/Contents/jbr/Contents/Home"; do
    if [ -x "$c/bin/javac" ]; then export JAVA_HOME="$c"; break; fi
  done
fi
[ -n "${JAVA_HOME:-}" ] && export PATH="$JAVA_HOME/bin:$PATH"
command -v javac >/dev/null || { echo "javac not found; set JAVA_HOME" >&2; exit 1; }

B=build
rm -rf "$B/obj" "$B/classes" "$B/dex" "$B/res.zip"
mkdir -p "$B/obj" "$B/classes" "$B/dex"

"$BT/aapt2" link -o "$B/unsigned-res.apk" -I "$PLATFORM" \
  --manifest AndroidManifest.xml --min-sdk-version 30 --target-sdk-version 34 \
  --version-code 1 --version-name 0.1.0
javac -source 8 -target 8 -Xlint:-options -cp "$PLATFORM" -d "$B/classes" \
  $(find src -name '*.java')
"$BT/d8" --min-api 30 --lib "$PLATFORM" --output "$B/dex" \
  $(find "$B/classes" -name '*.class')

cp "$B/unsigned-res.apk" "$B/unsigned.apk"
(cd "$B/dex" && zip -q -j ../unsigned.apk classes.dex)
"$BT/zipalign" -f -p 4 "$B/unsigned.apk" "$B/aligned.apk"

KS="$B/debug.keystore"
if [ ! -f "$KS" ]; then
  keytool -genkeypair -keystore "$KS" -storepass android -keypass android \
    -alias debug -keyalg RSA -keysize 2048 -validity 10000 \
    -dname "CN=mentramic debug" >/dev/null 2>&1
fi
"$BT/apksigner" sign --ks "$KS" --ks-pass pass:android --key-pass pass:android \
  --out "$B/mentra-mic.apk" "$B/aligned.apk"
"$BT/apksigner" verify "$B/mentra-mic.apk"

echo "apk: $(pwd)/$B/mentra-mic.apk"
shasum -a 256 "$B/mentra-mic.apk"
