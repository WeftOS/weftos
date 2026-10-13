# mentra-mic

Tiny capture app for Mentra Live glasses. An adb shell (uid 2000) cannot open
`/dev/snd/pcmC0D12c`, so this app holds `RECORD_AUDIO` and records through
`AudioRecord` (VOICE_RECOGNITION, falling back to MIC; 16 kHz mono PCM16).

Package `com.weftos.mentramic`, minSdk 30, targetSdk 34, plain Java.

## Build

    ./build.sh

Needs the Android SDK at `~/Library/Android/sdk` (build-tools 36.0.0 or 34.0.0,
platform android-35) and a JDK (`JAVA_HOME`, system java, or Homebrew openjdk@17).
Output: `build/mentra-mic.apk`, signed with a debug keystore generated in
`build/` (gitignored, never committed).

## Install and grant

    adb -s 192.168.10.128:5555 install -r mentra-mic.apk
    adb -s 192.168.10.128:5555 shell pm grant com.weftos.mentramic android.permission.RECORD_AUDIO
    adb -s 192.168.10.128:5555 shell am start -n com.weftos.mentramic/.LauncherActivity

The launcher activity is headless; it only brings the process up.

## Use

Always use an explicit component; implicit broadcasts are dropped.

    # record 6 s to utt1.wav (seconds: 1-30, default 6; name default "utt")
    adb -s 192.168.10.128:5555 shell am broadcast -n com.weftos.mentramic/.MicReceiver \
      -a com.weftos.mentramic.RECORD --ei seconds 6 --es name utt1

    # end a capture early (the WAV so far is still written)
    adb -s 192.168.10.128:5555 shell am broadcast -n com.weftos.mentramic/.MicReceiver \
      -a com.weftos.mentramic.STOP

    # 1 s "is the mic live" check, writes level.json
    adb -s 192.168.10.128:5555 shell am broadcast -n com.weftos.mentramic/.MicReceiver \
      -a com.weftos.mentramic.LEVEL

Outputs land in `/sdcard/Android/data/com.weftos.mentramic/files/`:

| file | meaning |
|------|---------|
| `<name>.wav` | the audio (written to a temp file, then renamed) |
| `<name>.done` | `{"frames","seconds","peak","rms","source"}`; written after the WAV |
| `<name>.err` | error message, if the capture failed |
| `level.json` | peak and rms from a LEVEL check |

Poll for `.done` (or `.err`), then pull:

    D=/sdcard/Android/data/com.weftos.mentramic/files
    until adb -s 192.168.10.128:5555 shell "test -e $D/utt1.done -o -e $D/utt1.err"; do sleep 1; done
    adb -s 192.168.10.128:5555 pull $D/utt1.wav .
    adb -s 192.168.10.128:5555 shell cat $D/utt1.done

Delete stale `<name>.done`/`.err` before reusing a name, or the poll returns early.

A peak near 0 with rms near 0 means the mic returned silence (another process may
hold the mic, or the audio route is muted).

## Pipeline after capture

    weftos-cos transcribe utt1.wav

then POST the transcript to `/v1/cos/utterance` as endpoint `mentra`.

## Foreground start (preferred on Android 14)

A broadcast from the background may be refused the microphone. Starting the
capture through the launcher activity runs it from the foreground:

```
adb -s 192.168.10.128:5555 shell am start -n com.weftos.mentramic/.LauncherActivity \
  -a com.weftos.mentramic.RECORD --ei seconds 6 --es name utt1
```

The same form works for `com.weftos.mentramic.LEVEL` and `com.weftos.mentramic.STOP`.

## Shell-started capture (display off, Android 11)

With the display asleep no activity is visible, so a service the app starts
itself is a background start and is denied the microphone, whatever the
RECORD_AUDIO app-op mode. A foreground service started directly by the shell
user is exempt. `MicService` is exported behind `android.permission.DUMP`,
which the adb shell holds and ordinary apps do not:

```
adb -s 192.168.10.128:5555 shell am start-foreground-service \
  -n com.weftos.mentramic/.MicService -a com.weftos.mentramic.RECORD \
  --ei seconds 6 --es name utt1
```

`com.weftos.mentramic.LEVEL` and `com.weftos.mentramic.STOP` work the same way
(STOP through the receiver: the running service ends early).
