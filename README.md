# BrightWheel

A small Windows tray utility for controlling the brightness of **every** connected monitor, with a Twinkle Tray–style floating panel.

## Features

- One slider per physical monitor, regardless of whether it is an external display (DDC/CI) or a laptop's built-in panel (IOCTL).
- Left-click the tray icon to toggle the panel; the panel pops up right above the tray icon in the bottom-right corner of the screen.
- Tray icon color reflects the current brightness; hovering shows the version.
- Right-click menu: Settings, Rescan monitors, Quit.
- Settings window: start with Windows, show/hide percentage labels, rescan monitors, quit.
- No automatic brightness adjustment — everything is manual.

## Build

Requires Rust 1.85+ and the Windows SDK (`rc.exe` is used to embed the application icon).

```powershell
cargo build --release
```

The executable is written to `target\release\BrightWheel.exe`.

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
- Autostart registry configuration uses the `brightwheel` crate.

## License

MIT
