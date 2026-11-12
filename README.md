# MonitorAudioRouter

A Windows desktop app for controlling playback output volume and routing active
audio sources according to the monitor containing their visible application
window.

## Features

- Lists active playback outputs and the audio sessions currently playing through
  each output.
- Controls both output master volume and individual session volume.
- Assigns an output to each detected monitor.
- Re-discovers monitors, outputs, and active sessions every second to reflect
  device hotplugging.
- Applies monitor-based routing to active app sessions. Monitor rules take
  precedence over app rules.
- Checks and applies routing in a background worker, so moving a playing app
  between displays continues to update its output while the UI is hidden.
- Saves monitor assignments and output volume settings in `config.json`.
- Uses a matching monitor-and-audio icon for the app window and system tray;
  its scalable source is [assets/monitor-audio-router.svg](assets/monitor-audio-router.svg).
- Keeps running in the Windows system tray when the main window is closed.
  When visible, the tray context menu has **Minimize to Tray** and **Exit App**;
  when hidden, it has **Restore Window** and **Exit App**. Double-clicking the
  tray icon also restores the UI.
- Runs as a Windows GUI app without an accompanying console window.

Per-app routing uses the Windows internal
`Windows.Media.Internal.AudioPolicyConfig` API because Windows does not expose
a supported public API for setting an individual process's audio output.
Windows updates may change this API; route failures are shown in the app.

## Build and run

On Windows, install Rust and the Visual Studio C++ build tools with a Windows
SDK, then run:

```powershell
cargo build --release
```

Run `target\release\monitor-audio-router.exe`. The app searches for `config.json`
beside the executable and in its parent directories, then searches from the
current directory. Its selected config path is shown in the window. If no file
exists, it starts with empty monitor assignments and creates the file when you
change a setting. Pairings are atomically saved when changed and flushed again
when the app exits.

## Configuration

The existing `monitor_rules`, `app_rules`, `volumes`, `poll_interval_ms`, and
`only_foreground` fields remain readable. The app migrates legacy `primary`,
`secondary`, and `\\.\DISPLAYn` rules to monitor hardware IDs. New assignments
are saved with monitor hardware IDs and audio endpoint IDs so they survive
display-number changes and distinguish outputs that share a friendly name.

Monitor assignments can be set to **System default**. Apps without an explicit
assignment are left on their existing Windows audio output.
