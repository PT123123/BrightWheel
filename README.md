# BrightWheel

A small Windows tray utility for controlling the brightness of **every** connected monitor, with a Twinkle Tray–style floating panel.

## Features

- One slider per physical monitor, regardless of whether it is an external display (DDC/CI) or a laptop's built-in panel (IOCTL).
- A "所有显示器" master slider appears when more than one monitor supports brightness.
- Left-click the tray icon to toggle the panel; the panel pops up right above the tray icon in the bottom-right corner of the screen and is clamped to that monitor.
- Click the track to jump, drag the thumb to adjust, or scroll the wheel over a slider for 5% steps.
- The panel stays in sync with external changes (keyboard brightness keys, monitor OSD, other apps) while it is open.
- Brightness writes are coalesced off the UI thread, so dragging never blocks the interface.
- Tray icon color reflects the average brightness; hovering shows the version.
- Right-click menu: Settings, Rescan monitors, Quit.
- Only one instance runs at a time (a second launch exits quietly).
- Settings window: start with Windows, show/hide percentage labels, rescan monitors, quit.
- Autostart keeps pointing at the newest version folder after an upgrade.
- No automatic brightness adjustment — everything is manual.

## Build

Requires Rust 1.85+ and the Windows SDK (`rc.exe` is used to embed the application icon).

```powershell
cargo build --release
```

The executable is written to `target\release\BrightWheel.exe`.

## Test

Pure logic (layout math, monitor labels, preference persistence, tray icon colors,
snapshot merging) is covered by unit tests and needs no displays:

```powershell
cargo test
```

## Run

```powershell
.\target\release\BrightWheel.exe
```

The app runs tray-only. Left-click the tray icon to open the brightness panel.

## Deploy

```powershell
just build
just deploy-workshop
```

`deploy-workshop` bumps the patch version, rebuilds, and copies `BrightWheel.exe` to
`C:\workshop\BrightWheel-<version>\`.

## How brightness is controlled

- External monitors: DDC/CI via the Windows physical monitor API (`brightness` crate).
- Internal laptop displays: `IOCTL_VIDEO_QUERY/SET_DISPLAY_BRIGHTNESS` (`brightness` crate).
- Monitor access runs on a dedicated worker thread; the UI only receives snapshots.
- Autostart is written to `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` and
  re-pointed at the current executable on startup after a version-folder upgrade.

## License

MIT
