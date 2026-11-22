// Release builds are tray-resident GUI apps: a console subsystem makes Windows
// open a black cmd window on launch. Tests keep the console for diagnostics.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

slint::include_modules!();

mod autostart;
mod single_instance;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use brightness::blocking::windows::BrightnessExt;
use brightness::blocking::{Brightness, BrightnessDevice, brightness_devices};
use slint::winit_030::winit::dpi::PhysicalPosition as WinitPhysicalPosition;
use slint::winit_030::winit::platform::windows::WindowExtWindows;
use slint::winit_030::WinitWindowAccessor;
use slint::{
    LogicalSize, Model, ModelRc, PhysicalPosition, Timer, TimerMode, VecModel, WindowPosition,
    WindowSize,
};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, Rect, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

const MENU_SETTINGS: &str = "brightwheel.menu.settings";
const MENU_REFRESH: &str = "brightwheel.menu.refresh";
const MENU_QUIT: &str = "brightwheel.menu.quit";

// Keep these in sync with ui/app.slint.
const PANEL_WIDTH: f32 = 372.0;
const HEADER_H: f32 = 58.0;
const FOOTER_H: f32 = 32.0;
const BODY_PAD: f32 = 10.0;
const ROW_H: f32 = 58.0;
const MASTER_H: f32 = 56.0;
const EMPTY_H: f32 = 96.0;
const GAP: f32 = 8.0;

const TRAY_MARGIN: i32 = 10;
const SCREEN_MARGIN: i32 = 12;

/// How long after a user drag we keep local values instead of overwriting them
/// with a polled snapshot.
const INTERACTION_GRACE: Duration = Duration::from_millis(1500);

/// Trailing-edge debounce for DDC/CI writes. While the user is dragging, only
/// the UI updates; the final value is written once the slider has been still
/// for this long (or immediately on release). Fast drags therefore cannot
/// queue a long tail of stale brightness commands into the worker.
const WRITE_DEBOUNCE: Duration = Duration::from_millis(110);

#[derive(Clone, Debug, PartialEq, Eq)]
struct MonSnapshot {
    name: String,
    percent: u32,
    supported: bool,
    detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotReason {
    Refresh,
    Poll,
}

enum Command {
    Refresh,
    Poll,
    Set { index: usize, percent: u32 },
    SetAll { percent: u32 },
    /// Dev-only: forces the last monitor into the "unwritable" state so the
    /// failure UI can be checked without broken hardware.
    DevFailLast,
}

struct DeviceEntry {
    device: Option<BrightnessDevice>,
    key: String,
    name: String,
    percent: u32,
    supported: bool,
    detail: String,
}

struct AppState {
    model: Rc<VecModel<MonitorItem>>,
    hub: Option<Sender<Command>>,
    pending: HashMap<usize, u32>,
    pending_all: Option<u32>,
    /// Absolute "all monitors" target chosen with the master slider. Unlike a
    /// moving average, this stays put while the user drags so the slider does
    /// not creep back toward an unrelated value.
    master_setpoint: Option<u32>,
    last_touch: Option<Instant>,
    /// Sticky Chinese message shown after a DDC/CI write failure, so the
    /// periodic poll does not immediately replace it with "亮度已同步".
    failed_notice: Option<String>,
    /// Native handle of the popup panel, used to detect focus loss.
    panel_hwnd: Option<isize>,
    visible: bool,
    tray_rect: Option<Rect>,
}

impl AppState {
    fn new() -> Self {
        Self {
            model: Rc::new(VecModel::from(Vec::<MonitorItem>::new())),
            hub: None,
            pending: HashMap::new(),
            pending_all: None,
            master_setpoint: None,
            last_touch: None,
            failed_notice: None,
            panel_hwnd: None,
            visible: false,
            tray_rect: None,
        }
    }
}

thread_local! {
    // tray-icon / muda types are !Send, so they must stay on the UI thread.
    static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
    static MENU_ITEMS: RefCell<Vec<MenuItem>> = const { RefCell::new(Vec::new()) };
    static LAST_TRAY_PERCENT: RefCell<Option<u32>> = const { RefCell::new(None) };
    static STATE: RefCell<AppState> = RefCell::new(AppState::new());
    // Single-shot write timer: restarts on every slider move so only the
    // final value is written once the user stops dragging.
    static WRITE_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

/// Arm (or re-arm) the trailing-edge debounce. Rapid slider moves keep
/// pushing the write out, so the worker never accumulates a stale queue.
fn schedule_write() {
    WRITE_TIMER.with(|cell| {
        if let Some(timer) = cell.borrow().as_ref() {
            timer.restart();
        }
    });
}

/// Write the latest queued targets. Called by the debounce timer; per-index
/// and master targets are coalesced into a single command batch.
fn flush_writes() {
    STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let Some(hub) = state.hub.clone() else {
            return;
        };
        if !state.pending.is_empty() {
            let pending: Vec<(usize, u32)> = state.pending.drain().collect();
            for (index, percent) in pending {
                let _ = hub.send(Command::Set { index, percent });
            }
        }
        if let Some(percent) = state.pending_all.take() {
            let _ = hub.send(Command::SetAll { percent });
        }
    });
}

#[derive(Clone, Copy)]
struct Prefs {
    show_percent: bool,
}

impl Prefs {
    fn load() -> Self {
        Self::load_from(&Self::path())
    }

    fn load_from(path: &Path) -> Self {
        let mut show_percent = true;
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("show_percent=") {
                    show_percent = value.trim() == "1";
                }
            }
        }
        Self { show_percent }
    }

    fn save(&self) {
        self.save_to(&Self::path());
    }

    fn save_to(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body = format!("show_percent={}\n", u8::from(self.show_percent));
        let _ = std::fs::write(path, body);
    }

    fn path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("BrightWheel").join("settings.txt")
    }
}

fn clamp_percent(value: f32) -> u32 {
    if !value.is_finite() {
        return 0;
    }
    value.round().clamp(0.0, 100.0) as u32
}

fn average_percent(values: &[u32]) -> Option<u32> {
    if values.is_empty() {
        return None;
    }
    let sum: u64 = values.iter().map(|value| u64::from(*value)).sum();
    Some(((sum as f64 / values.len() as f64).round() as u32).min(100))
}

fn panel_height(monitor_count: usize, has_master: bool) -> f32 {
    let body = if monitor_count == 0 {
        EMPTY_H
    } else {
        let children = monitor_count + usize::from(has_master);
        let heights = monitor_count as f32 * ROW_H + if has_master { MASTER_H } else { 0.0 };
        let gaps = children.saturating_sub(1) as f32 * GAP;
        heights + gaps
    };
    HEADER_H + BODY_PAD + body + BODY_PAD + FOOTER_H
}

/// Place the panel above the tray icon, flipping below when there is no room,
/// and clamp it inside the monitor that owns the tray icon.
fn popup_position(
    tray: (i32, i32, i32, i32),
    popup: (i32, i32),
    screen: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (tray_x, tray_y, tray_w, tray_h) = tray;
    let (popup_w, popup_h) = popup;
    let (screen_x, screen_y, screen_w, screen_h) = screen;

    let mut x = tray_x + tray_w / 2 - popup_w / 2;
    let mut y = tray_y - popup_h - TRAY_MARGIN;
    if y < screen_y + SCREEN_MARGIN {
        y = tray_y + tray_h + TRAY_MARGIN;
    }

    let min_x = screen_x + SCREEN_MARGIN;
    let max_x = (screen_x + screen_w - popup_w - SCREEN_MARGIN).max(min_x);
    let min_y = screen_y + SCREEN_MARGIN;
    let max_y = (screen_y + screen_h - popup_h - SCREEN_MARGIN).max(min_y);

    x = x.clamp(min_x, max_x);
    y = y.clamp(min_y, max_y);
    (x, y)
}

fn is_primary_display(device_name: &str) -> bool {
    device_name.to_ascii_uppercase().ends_with("DISPLAY1")
}

/// Extracts the EDID model id from a Windows monitor interface path, e.g.
/// `\\?\DISPLAY#LEN8BA1#4&2c6c98d4&0&UID8388688#{...}` -> `LEN8BA1`.
fn display_model_from_path(path: &str) -> Option<String> {
    let rest = path.split_once("DISPLAY#")?.1;
    let model = rest.split('#').next()?;
    (!model.is_empty()).then(|| model.to_string())
}

fn monitor_label(index: usize, friendly: &str, primary: bool) -> String {
    let name = friendly.trim();
    let name = if name.is_empty() { "显示器" } else { name };
    let suffix = if primary { "（主）" } else { "" };
    format!("{}. {}{}", index + 1, name, suffix)
}

const ICON_N: usize = 32;
const ICON_SS: usize = 4;

const ICON_SUN: (f32, f32, f32) = (255.0, 205.0, 72.0);
const ICON_SUN_LO: (f32, f32, f32) = (232.0, 150.0, 46.0);

/// Sun tint: warmer/dimmer at 0%, bright golden at 100%.
fn icon_color(percent: u32) -> (u8, u8, u8) {
    let t = percent.min(100) as f32 / 100.0;
    let mix = |lo: f32, hi: f32| (lo + (hi - lo) * t) as u8;
    (
        mix(ICON_SUN_LO.0, ICON_SUN.0),
        mix(ICON_SUN_LO.1, ICON_SUN.1),
        mix(ICON_SUN_LO.2, ICON_SUN.2),
    )
}

/// A sun (solid core + 8 tapered rays) wrapped in a clockwise progress ring
/// that starts at 12 o'clock. Rendered at 4x then box-filtered for smooth
/// edges at tray sizes. Returns ICON_N*ICON_N*4 RGBA bytes.
fn icon_rgba(percent: u32) -> Vec<u8> {
    let n = ICON_N as f32;
    let ss = ICON_SS as f32;
    let grid_n = ICON_N * ICON_SS;
    let center = ((n - 1.0) / 2.0 + 0.5) * ss;
    let core_r = n * 0.175 * ss;
    let ray_in = n * 0.205 * ss;
    let ray_out = n * 0.335 * ss;
    let ring_r = n * 0.415 * ss;
    let ring_hw = (0.9f32).max(n * 0.035) * ss;
    let sun = icon_color(percent);
    let sweep = percent.min(100) as f32 * 3.6;

    let mut grid = vec![[0u8; 4]; grid_n * grid_n];
    for yy in 0..grid_n {
        for xx in 0..grid_n {
            let dx = xx as f32 + 0.5 - center;
            let dy = yy as f32 + 0.5 - center;
            let dist = dx.hypot(dy);
            let mut px = [0u8; 4];

            if dist <= core_r {
                px = [sun.0, sun.1, sun.2, 255];
            } else {
                let mut ray_hit = false;
                for k in 0..8 {
                    let a = k as f32 * std::f32::consts::PI / 4.0;
                    let ux = a.cos();
                    let uy = a.sin();
                    let proj = dx * ux + dy * uy;
                    let perp = -dx * uy + dy * ux;
                    if proj >= ray_in && proj <= ray_out {
                        let q = (proj - ray_in) / (ray_out - ray_in);
                        if perp.abs() <= (1.9 - 1.15 * q) * ss {
                            ray_hit = true;
                            break;
                        }
                    }
                }
                if ray_hit {
                    px = [sun.0, sun.1, sun.2, 255];
                } else if (dist - ring_r).abs() <= ring_hw {
                    // Only the filled arc is drawn. No dark track underneath:
                    // a grey ring reads as a black frame around the icon at
                    // tray sizes, so the unfilled part stays transparent.
                    let deg = (dy.atan2(dx).to_degrees() + 90.0 + 360.0) % 360.0;
                    if deg <= sweep {
                        px = [sun.0, sun.1, sun.2, 255];
                    }
                }
            }
            grid[yy * grid_n + xx] = px;
        }
    }

    let mut rgba = vec![0u8; ICON_N * ICON_N * 4];
    let ss2 = (ICON_SS * ICON_SS) as u32;
    for y in 0..ICON_N {
        for x in 0..ICON_N {
            let mut acc = [0u32; 4];
            for sy in 0..ICON_SS {
                for sx in 0..ICON_SS {
                    let p = grid[(y * ICON_SS + sy) * grid_n + x * ICON_SS + sx];
                    for i in 0..4 {
                        acc[i] += p[i] as u32;
                    }
                }
            }
            let i = (y * ICON_N + x) * 4;
            for c in 0..4 {
                rgba[i + c] = (acc[c] / ss2) as u8;
            }
        }
    }
    rgba
}

fn make_icon(percent: u32) -> Icon {
    Icon::from_rgba(icon_rgba(percent), ICON_N as u32, ICON_N as u32).expect("invalid tray icon")
}

fn paint_tray(percent: u32) {
    let percent = percent.min(100);
    let should_update = LAST_TRAY_PERCENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let update = match *slot {
            None => true,
            Some(last) => {
                last != percent
                    && (last.abs_diff(percent) >= 2 || percent == 0 || percent == 100)
            }
        };
        if update {
            *slot = Some(percent);
        }
        update
    });
    if !should_update {
        return;
    }
    TRAY.with(|slot| {
        if let Some(tray) = slot.borrow().as_ref() {
            let _ = tray.set_icon(Some(make_icon(percent)));
        }
    });
}

fn resize_panel(win: &MainWindow, state: &AppState) {
    let count = state.model.row_count();
    let supported = (0..count)
        .filter_map(|index| state.model.row_data(index))
        .filter(|item| item.supported)
        .count();
    let height = panel_height(count, supported > 1);
    win.window()
        .set_size(WindowSize::Logical(LogicalSize::new(PANEL_WIDTH, height)));
}

fn set_row_percent(state: &mut AppState, index: usize, percent: u32) {
    if let Some(mut item) = state.model.row_data(index)
        && item.supported
    {
        item.percent = percent as i32;
        state.model.set_row_data(index, item);
    }
}

/// The master slider shows the absolute target the user chose while one is
/// active, and only falls back to the measured average otherwise.
fn displayed_master(state: &AppState, average: u32) -> u32 {
    state.master_setpoint.unwrap_or(average)
}

fn refresh_master(win: &MainWindow, state: &AppState) {
    let supported: Vec<u32> = (0..state.model.row_count())
        .filter_map(|index| state.model.row_data(index))
        .filter(|item| item.supported)
        .map(|item| item.percent.max(0) as u32)
        .collect();
    win.set_supported_count(supported.len() as i32);
    win.set_has_master(supported.len() > 1);
    if let Some(average) = average_percent(&supported) {
        let shown = displayed_master(state, average);
        win.set_master_percent(shown as i32);
        paint_tray(shown);
    }
}

fn merged_percent(preserving: bool, old: Option<&MonitorItem>, monitor: &MonSnapshot) -> i32 {
    if preserving && monitor.supported {
        old.filter(|item| item.supported)
            .map(|item| item.percent)
            .unwrap_or(monitor.percent as i32)
    } else {
        monitor.percent as i32
    }
}

fn apply_snapshot(win: &MainWindow, snapshot: Vec<MonSnapshot>, reason: SnapshotReason) {
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            // An explicit rescan re-reads reality: drop any absolute master
            // target so the slider falls back to the measured average.
            if reason == SnapshotReason::Refresh {
                state.master_setpoint = None;
            }
            let preserving = state
                .last_touch
                .is_some_and(|touched| touched.elapsed() < INTERACTION_GRACE);

        let mut items = Vec::with_capacity(snapshot.len());
        for (index, monitor) in snapshot.iter().enumerate() {
            let old = state.model.row_data(index);
            items.push(MonitorItem {
                index: index as i32,
                name: monitor.name.as_str().into(),
                percent: merged_percent(preserving, old.as_ref(), monitor),
                supported: monitor.supported,
                detail: monitor.detail.as_str().into(),
            });
        }

        if state.model.row_count() == items.len() {
            for (index, item) in items.iter().enumerate() {
                state.model.set_row_data(index, item.clone());
            }
        } else {
            state.model.set_vec(items.clone());
        }

        let supported: Vec<u32> = items
            .iter()
            .filter(|item| item.supported)
            .map(|item| item.percent.max(0) as u32)
            .collect();
        win.set_supported_count(supported.len() as i32);
        win.set_has_master(supported.len() > 1);
        if let Some(average) = average_percent(&supported) {
            let shown = displayed_master(&state, average);
            win.set_master_percent(shown as i32);
            paint_tray(shown);
        }
        win.set_scanning(false);

        if !preserving {
            if reason == SnapshotReason::Poll
                && let Some(notice) = state.failed_notice.clone()
            {
                // Keep the failure visible until the user acts or rescans.
                win.set_status_error(true);
                win.set_status_text(notice.into());
            } else {
                if reason == SnapshotReason::Refresh {
                    state.failed_notice = None;
                }
                let status = if items.is_empty() {
                    "未检测到显示器".to_string()
                } else if reason == SnapshotReason::Refresh {
                    format!("已发现 {} 台显示器", items.len())
                } else {
                    "亮度已同步".to_string()
                };
                win.set_status_error(false);
                win.set_status_text(status.into());
            }
        }

        resize_panel(win, &state);
        if state.visible
            && let Some(rect) = state.tray_rect
        {
            place_popup(win.window(), &rect);
        }
    });
}

fn place_popup(window: &slint::Window, tray: &Rect) {
    let size = window.size();
    let bounds = screen_bounds(window, tray).unwrap_or((0, 0, 1920, 1080));
    let tray_rect = (
        tray.position.x.round() as i32,
        tray.position.y.round() as i32,
        tray.size.width as i32,
        tray.size.height as i32,
    );
    let (x, y) = popup_position(
        tray_rect,
        (size.width as i32, size.height as i32),
        bounds,
    );

    window.set_position(WindowPosition::Physical(PhysicalPosition::new(x, y)));
    window.with_winit_window(|native| {
        native.set_outer_position(WinitPhysicalPosition::new(x, y));
    });
}

fn screen_bounds(window: &slint::Window, tray: &Rect) -> Option<(i32, i32, i32, i32)> {
    window
        .with_winit_window(|native| {
            let point = WinitPhysicalPosition::new(
                tray.position.x.round() as i32,
                tray.position.y.round() as i32,
            );
            native
                .available_monitors()
                .find_map(|monitor| {
                    let position = monitor.position();
                    let size = monitor.size();
                    let contains = point.x >= position.x
                        && point.x < position.x + size.width as i32
                        && point.y >= position.y
                        && point.y < position.y + size.height as i32;
                    contains.then_some((
                        position.x,
                        position.y,
                        size.width as i32,
                        size.height as i32,
                    ))
                })
                .or_else(|| {
                    native.current_monitor().map(|monitor| {
                        let position = monitor.position();
                        let size = monitor.size();
                        (
                            position.x,
                            position.y,
                            size.width as i32,
                            size.height as i32,
                        )
                    })
                })
        })
        .flatten()
}

fn configure_popup_window(window: &slint::Window) {
    window.with_winit_window(|native| {
        native.set_skip_taskbar(true);
        native.set_undecorated_shadow(true);
        native.focus_window();
    });
    apply_rounded_corners(window);
    STATE.with(|cell| {
        cell.borrow_mut().panel_hwnd = native_hwnd(window);
    });
}

fn native_hwnd(window: &slint::Window) -> Option<isize> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    window
        .with_winit_window(|native| {
            if let Ok(handle) = native.window_handle()
                && let RawWindowHandle::Win32(win32) = handle.as_raw()
            {
                Some(win32.hwnd.get())
            } else {
                None
            }
        })
        .flatten()
}

#[cfg(windows)]
fn apply_rounded_corners(window: &slint::Window) {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    window.with_winit_window(|native| {
        if let Ok(handle) = native.window_handle()
            && let RawWindowHandle::Win32(win32) = handle.as_raw()
        {
            // DWMWA_WINDOW_CORNER_PREFERENCE = 33, DWMWCP_ROUND = 2.
            let preference: i32 = 2;
            unsafe {
                let _ = DwmSetWindowAttribute(
                    win32.hwnd.get() as *mut core::ffi::c_void,
                    33,
                    &preference as *const i32 as *const core::ffi::c_void,
                    std::mem::size_of::<i32>() as u32,
                );
            }
        }
    });
}

#[cfg(not(windows))]
fn apply_rounded_corners(_window: &slint::Window) {}

fn configure_settings_window(window: &slint::Window) {
    window.with_winit_window(|native| {
        native.set_skip_taskbar(true);
        native.focus_window();
    });
}

fn refresh_settings(window: &SettingsWindow) {
    window.set_autostart_enabled(autostart::is_enabled());
    window.set_autostart_detail(autostart::detail_text().into());
}

fn center_settings_window(window: &slint::Window) {
    let (screen_w, screen_h) = primary_screen_size();
    let size = window.size();
    let x = (screen_w - size.width as i32) / 2;
    let y = (screen_h - size.height as i32) / 2;
    window.set_position(WindowPosition::Physical(PhysicalPosition::new(x, y)));
    window.with_winit_window(|native| {
        native.set_outer_position(WinitPhysicalPosition::new(x, y));
    });
}

fn show_settings(window: &SettingsWindow) {
    refresh_settings(window);
    let _ = window.window().show();
    configure_settings_window(window.window());
    center_settings_window(window.window());
}

fn enumerate_devices(failed: &HashSet<String>) -> Vec<DeviceEntry> {
    let mut entries = Vec::new();
    for result in brightness_devices() {
        let index = entries.len();
        match result {
            Ok(device) => {
                let device_name = device.device_name().unwrap_or_default();
                let friendly = device.friendly_device_name().unwrap_or_default();
                let model = device
                    .device_path()
                    .ok()
                    .and_then(|path| display_model_from_path(&path));
                let label_source = if !friendly.trim().is_empty() {
                    friendly
                } else if let Some(model) = model {
                    model
                } else {
                    "显示器".to_string()
                };
                let name = monitor_label(index, &label_source, is_primary_display(&device_name));
                let blocked = failed.contains(&device_name);
                match device.get() {
                    Ok(percent) => entries.push(DeviceEntry {
                        device: Some(device),
                        key: device_name,
                        name,
                        percent,
                        supported: !blocked,
                        detail: if blocked {
                            "DDC/CI 写入失败，可能不支持调节".to_string()
                        } else {
                            String::new()
                        },
                    }),
                    Err(_) => entries.push(DeviceEntry {
                        device: Some(device),
                        key: device_name,
                        name,
                        percent: 0,
                        supported: false,
                        detail: "该显示器不支持亮度调节".to_string(),
                    }),
                }
            }
            Err(_) => entries.push(DeviceEntry {
                device: None,
                key: String::new(),
                name: monitor_label(index, "未知显示器", false),
                percent: 0,
                supported: false,
                detail: "无法读取显示器信息".to_string(),
            }),
        }
    }
    entries
}

fn poll_devices(devices: &mut [DeviceEntry]) {
    for entry in devices.iter_mut() {
        if !entry.supported {
            continue;
        }
        if let Some(device) = &entry.device
            && let Ok(percent) = device.get()
        {
            entry.percent = percent;
        }
    }
}

fn send_snapshot(ui: &slint::Weak<MainWindow>, devices: &[DeviceEntry], reason: SnapshotReason) {
    let monitors: Vec<MonSnapshot> = devices
        .iter()
        .map(|entry| MonSnapshot {
            name: entry.name.clone(),
            percent: entry.percent,
            supported: entry.supported,
            detail: entry.detail.clone(),
        })
        .collect();
    let weak = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(win) = weak.upgrade() {
            apply_snapshot(&win, monitors, reason);
        }
    });
}

/// `"3. RTK2775"` -> `"RTK2775"` for compact status messages.
fn strip_index_prefix(name: &str) -> &str {
    match name.split_once(". ") {
        Some((prefix, rest)) if prefix.chars().all(|c| c.is_ascii_digit()) => rest,
        _ => name,
    }
}

/// A monitor rejected a DDC/CI write: flip it to "unsupported" instead of
/// leaving a slider that silently fails, and tell the user in Chinese.
fn mark_monitor_failed(win: &MainWindow, index: usize, monitor_name: &str) {
    STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let Some(mut item) = state.model.row_data(index) else {
            return;
        };
        item.supported = false;
        item.detail = "DDC/CI 写入失败，可能不支持调节".into();
        state.model.set_row_data(index, item);
        refresh_master(win, &state);
        win.set_scanning(false);
        win.set_status_error(true);
        let notice = format!(
            "{} 无法调节（DDC/CI 写入失败）",
            strip_index_prefix(monitor_name)
        );
        win.set_status_text(notice.as_str().into());
        state.failed_notice = Some(notice);
        resize_panel(win, &state);
        if state.visible
            && let Some(rect) = state.tray_rect
        {
            place_popup(win.window(), &rect);
        }
    });
}

fn send_monitor_failed(ui: &slint::Weak<MainWindow>, index: usize, monitor_name: String) {
    let weak = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(win) = weak.upgrade() {
            mark_monitor_failed(&win, index, &monitor_name);
        }
    });
}

/// DDC/CI writes on some adapters fail intermittently, so retry once before
/// giving up on a monitor.
fn write_percent(device: &BrightnessDevice, percent: u32) -> Result<(), brightness::Error> {
    device.set(percent).or_else(|_| {
        thread::sleep(Duration::from_millis(150));
        device.set(percent)
    })
}

fn spawn_worker(ui: slint::Weak<MainWindow>) -> Sender<Command> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut devices: Vec<DeviceEntry> = Vec::new();
        // Monitors that rejected a write are remembered until the user asks for
        // a rescan, so a slider that cannot work is not shown again.
        let mut failed: HashSet<String> = HashSet::new();
        while let Ok(command) = receiver.recv() {
            match command {
                Command::Refresh => {
                    // Explicit rescan gives every monitor another chance.
                    failed.clear();
                    devices = enumerate_devices(&failed);
                    send_snapshot(&ui, &devices, SnapshotReason::Refresh);
                }
                Command::Poll => {
                    if devices.is_empty() {
                        devices = enumerate_devices(&failed);
                        send_snapshot(&ui, &devices, SnapshotReason::Refresh);
                    } else {
                        poll_devices(&mut devices);
                        send_snapshot(&ui, &devices, SnapshotReason::Poll);
                    }
                }
                Command::Set { index, percent } => {
                    let Some(entry) = devices.get_mut(index) else {
                        continue;
                    };
                    if !entry.supported {
                        continue;
                    }
                    let Some(device) = &entry.device else {
                        continue;
                    };
                    match write_percent(device, percent) {
                        Ok(()) => entry.percent = percent,
                        Err(_) => {
                            entry.supported = false;
                            entry.detail = "DDC/CI 写入失败，可能不支持调节".to_string();
                            if !entry.key.is_empty() {
                                failed.insert(entry.key.clone());
                            }
                            send_monitor_failed(&ui, index, entry.name.clone());
                        }
                    }
                }
                Command::SetAll { percent } => {
                    // Keep going on failure so one unsupported monitor does not
                    // block the rest.
                    for (index, entry) in devices.iter_mut().enumerate() {
                        if !entry.supported {
                            continue;
                        }
                        let Some(device) = &entry.device else {
                            continue;
                        };
                        match write_percent(device, percent) {
                            Ok(()) => entry.percent = percent,
                            Err(_) => {
                                entry.supported = false;
                                entry.detail = "DDC/CI 写入失败，可能不支持调节".to_string();
                                if !entry.key.is_empty() {
                                    failed.insert(entry.key.clone());
                                }
                                send_monitor_failed(&ui, index, entry.name.clone());
                            }
                        }
                    }
                }
                Command::DevFailLast => {
                    if let Some(index) = devices.len().checked_sub(1) {
                        let entry = &mut devices[index];
                        entry.supported = false;
                        entry.detail = "DDC/CI 写入失败，可能不支持调节".to_string();
                        if !entry.key.is_empty() {
                            failed.insert(entry.key.clone());
                        }
                        send_monitor_failed(&ui, index, entry.name.clone());
                    }
                }
            }
        }
    });
    sender
}

fn queue_set(state: &mut AppState, index: usize, percent: u32) {
    state.pending.insert(index, percent);
}

fn send_now(state: &AppState, index: usize, percent: u32) {
    if let Some(hub) = &state.hub {
        let _ = hub.send(Command::Set { index, percent });
    }
}

fn toggle_popup(weak: &slint::Weak<MainWindow>, rect: &Rect) {
    let Some(win) = weak.upgrade() else {
        return;
    };
    let window = win.window();
    if window.is_visible() {
        let _ = window.hide();
        STATE.with(|cell| cell.borrow_mut().visible = false);
        return;
    }

    STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        state.visible = true;
        state.tray_rect = Some(*rect);
        resize_panel(&win, &state);
    });

    let scale = window.scale_factor();
    let height = STATE.with(|cell| {
        let state = cell.borrow();
        let count = state.model.row_count();
        let supported = (0..count)
            .filter_map(|index| state.model.row_data(index))
            .filter(|item| item.supported)
            .count();
        panel_height(count, supported > 1)
    });
    let physical = (
        (PANEL_WIDTH * scale).round() as i32,
        (height * scale).round() as i32,
    );
    let x = rect.position.x.round() as i32 + rect.size.width as i32 / 2 - physical.0 / 2;
    let y = rect.position.y.round() as i32 - physical.1 - TRAY_MARGIN;
    window.set_position(WindowPosition::Physical(PhysicalPosition::new(x, y)));

    let _ = window.show();
    configure_popup_window(window);
    place_popup(window, rect);

    STATE.with(|cell| {
        let state = cell.borrow();
        if let Some(hub) = &state.hub {
            let _ = hub.send(Command::Poll);
        }
    });
}

#[cfg(windows)]
fn primary_screen_size() -> (i32, i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

#[cfg(not(windows))]
fn primary_screen_size() -> (i32, i32) {
    (1920, 1080)
}

#[cfg(windows)]
fn foreground_hwnd() -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow() as isize }
}

#[cfg(not(windows))]
fn foreground_hwnd() -> isize {
    0
}

fn main() -> Result<()> {
    // Take ownership and, if an older build is resident, ask it to retire
    // gracefully. Install the listener only after we own the mutex so an old
    // instance cannot make the incoming build quit instead of itself.
    let _instance = single_instance::acquire(Duration::from_secs(3))?;
    single_instance::install_quit_listener()?;

    slint::BackendSelector::new()
        .backend_name("winit".into())
        .select()?;

    let main_win = MainWindow::new()?;
    let settings = SettingsWindow::new()?;
    let prefs = Rc::new(RefCell::new(Prefs::load()));
    let app_version = format!("BrightWheel v{}", env!("CARGO_PKG_VERSION"));

    let model = STATE.with(|cell| cell.borrow().model.clone());
    main_win.set_monitors(ModelRc::from(model));
    main_win.set_show_percent(prefs.borrow().show_percent);
    main_win.set_scanning(true);
    main_win.set_status_text("正在扫描显示器…".into());

    settings.set_version_text(app_version.clone().into());
    settings.set_show_percent(prefs.borrow().show_percent);
    settings.set_autostart_enabled(autostart::is_enabled());
    // Versioned deploy folders move the exe on every upgrade; keep the Run key fresh.
    let _ = autostart::reconcile();
    settings.set_autostart_detail(autostart::detail_text().into());

    let hub = spawn_worker(main_win.as_weak());
    STATE.with(|cell| cell.borrow_mut().hub = Some(hub.clone()));

    let main_weak = main_win.as_weak();
    let settings_weak = settings.as_weak();

    // --- Panel callbacks -------------------------------------------------

    let brightness_weak = main_weak.clone();
    main_win.on_brightness_changed(move |index, value| {
        let Some(win) = brightness_weak.upgrade() else {
            return;
        };
        let percent = clamp_percent(value);
        let index = index.max(0) as usize;
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            state.last_touch = Some(Instant::now());
            state.failed_notice = None;
            // A direct single-monitor drag overrides any master target: from
            // now on the master reflects the real average again.
            state.master_setpoint = None;
            set_row_percent(&mut state, index, percent);
            refresh_master(&win, &state);
            queue_set(&mut state, index, percent);
            schedule_write();
            win.set_status_error(false);
            win.set_status_text(format!("显示器 {}：{}%", index + 1, percent).into());
        });
    });

    let released_weak = main_weak.clone();
    main_win.on_brightness_released(move |index, value| {
        let Some(win) = released_weak.upgrade() else {
            return;
        };
        let percent = clamp_percent(value);
        let index = index.max(0) as usize;
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            state.last_touch = Some(Instant::now());
            state.failed_notice = None;
            state.master_setpoint = None;
            state.pending.remove(&index);
            set_row_percent(&mut state, index, percent);
            refresh_master(&win, &state);
            // Commit the final value now; the pending debounce write for this
            // index was just removed so no duplicate/stale write follows.
            send_now(&state, index, percent);
        });
    });

    let master_weak = main_weak.clone();
    main_win.on_master_changed(move |value| {
        let Some(win) = master_weak.upgrade() else {
            return;
        };
        let percent = clamp_percent(value);
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            state.last_touch = Some(Instant::now());
            state.failed_notice = None;
            // Absolute target: every monitor goes to exactly this percent.
            state.master_setpoint = Some(percent);
            for index in 0..state.model.row_count() {
                set_row_percent(&mut state, index, percent);
            }
            state.pending.clear();
            state.pending_all = Some(percent);
            win.set_master_percent(percent as i32);
            paint_tray(percent);
            schedule_write();
            win.set_status_error(false);
            win.set_status_text(format!("所有显示器：{percent}%").into());
        });
    });

    let master_released_weak = main_weak.clone();
    main_win.on_master_released(move |value| {
        let Some(win) = master_released_weak.upgrade() else {
            return;
        };
        let percent = clamp_percent(value);
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            state.last_touch = Some(Instant::now());
            state.failed_notice = None;
            state.master_setpoint = Some(percent);
            state.pending.clear();
            state.pending_all = None;
            for index in 0..state.model.row_count() {
                set_row_percent(&mut state, index, percent);
            }
            win.set_master_percent(percent as i32);
            paint_tray(percent);
            // Commit the final absolute target immediately; the debounced
            // pending_all was cleared, so no duplicate write follows.
            if let Some(hub) = &state.hub {
                let _ = hub.send(Command::SetAll { percent });
            }
        });
    });

    let refresh_weak = main_weak.clone();
    main_win.on_do_refresh(move || {
        if let Some(win) = refresh_weak.upgrade() {
            win.set_scanning(true);
            win.set_status_error(false);
            win.set_status_text("正在扫描显示器…".into());
        }
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            state.failed_notice = None;
            state.master_setpoint = None;
            if let Some(hub) = &state.hub {
                let _ = hub.send(Command::Refresh);
            }
        });
    });

    let settings_opener = settings_weak.clone();
    let panel_closer = main_weak.clone();
    main_win.on_open_settings(move || {
        if let Some(panel) = panel_closer.upgrade() {
            let _ = panel.window().hide();
        }
        STATE.with(|cell| cell.borrow_mut().visible = false);
        if let Some(window) = settings_opener.upgrade() {
            show_settings(&window);
        }
    });

    let hide_weak = main_weak.clone();
    main_win.on_request_hide(move || {
        if let Some(win) = hide_weak.upgrade() {
            let _ = win.window().hide();
        }
        STATE.with(|cell| cell.borrow_mut().visible = false);
    });

    main_win.on_do_quit(move || {
        let _ = slint::quit_event_loop();
    });

    // --- Settings callbacks ----------------------------------------------

    let autostart_weak = settings_weak.clone();
    settings.on_autostart_changed(move |enabled| {
        let result = autostart::set_enabled(enabled);
        if let Some(window) = autostart_weak.upgrade() {
            if let Err(error) = result {
                window.set_notice_text(format!("开机自启设置失败：{error}").into());
            } else {
                window.set_notice_text("".into());
            }
            refresh_settings(&window);
        }
    });

    let check_autostart_weak = settings_weak.clone();
    settings.on_check_autostart(move || {
        let _ = autostart::reconcile();
        if let Some(window) = check_autostart_weak.upgrade() {
            window.set_notice_text("".into());
            refresh_settings(&window);
        }
    });

    let percent_prefs = prefs.clone();
    let percent_main = main_weak.clone();
    let percent_settings = settings_weak.clone();
    settings.on_show_percent_changed(move |enabled| {
        percent_prefs.borrow_mut().show_percent = enabled;
        percent_prefs.borrow().save();
        if let Some(window) = percent_main.upgrade() {
            window.set_show_percent(enabled);
        }
        if let Some(window) = percent_settings.upgrade() {
            window.set_show_percent(enabled);
        }
    });

    let settings_refresh = main_weak.clone();
    settings.on_do_refresh(move || {
        if let Some(win) = settings_refresh.upgrade() {
            win.set_scanning(true);
            win.set_status_error(false);
            win.set_status_text("正在扫描显示器…".into());
        }
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            state.failed_notice = None;
            state.master_setpoint = None;
            if let Some(hub) = &state.hub {
                let _ = hub.send(Command::Refresh);
            }
        });
    });

    let settings_closer = settings_weak.clone();
    settings.on_hide_window(move || {
        if let Some(window) = settings_closer.upgrade() {
            let _ = window.window().hide();
        }
    });

    settings.on_do_quit(move || {
        let _ = slint::quit_event_loop();
    });

    // --- Tray ------------------------------------------------------------

    let id_settings = MenuId::new(MENU_SETTINGS);
    let id_refresh = MenuId::new(MENU_REFRESH);
    let id_quit = MenuId::new(MENU_QUIT);

    let tray_handler_weak = main_weak.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let weak = tray_handler_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let TrayIconEvent::Click {
                button,
                button_state,
                rect,
                ..
            } = event
                && button == MouseButton::Left
                && button_state == MouseButtonState::Up
            {
                toggle_popup(&weak, &rect);
            }
        });
    }));

    let menu_ids = (id_settings.clone(), id_refresh.clone(), id_quit.clone());
    let menu_main_weak = main_weak.clone();
    let menu_settings_weak = settings_weak.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let ids = menu_ids.clone();
        let main = menu_main_weak.clone();
        let settings = menu_settings_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if event.id == ids.0 {
                if let Some(panel) = main.upgrade() {
                    let _ = panel.window().hide();
                }
                STATE.with(|cell| cell.borrow_mut().visible = false);
                if let Some(window) = settings.upgrade() {
                    show_settings(&window);
                }
            } else if event.id == ids.1 {
                if let Some(win) = main.upgrade() {
                    win.set_scanning(true);
                    win.set_status_text("正在扫描显示器…".into());
                }
                STATE.with(|cell| {
                    let state = cell.borrow();
                    if let Some(hub) = &state.hub {
                        let _ = hub.send(Command::Refresh);
                    }
                });
            } else if event.id == ids.2 {
                let _ = slint::quit_event_loop();
            }
        });
    }));

    let menu = Menu::new();
    let item_settings = MenuItem::with_id(id_settings, "设置", true, None);
    let item_refresh = MenuItem::with_id(id_refresh, "重新扫描显示器", true, None);
    let item_quit = MenuItem::with_id(id_quit, "退出", true, None);
    let _ = menu.append(&item_settings);
    let _ = menu.append(&item_refresh);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item_quit);
    MENU_ITEMS.with(|items| *items.borrow_mut() = vec![item_settings, item_refresh, item_quit]);

    match TrayIconBuilder::new()
        .with_icon(make_icon(100))
        .with_tooltip(app_version)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
    {
        Ok(tray) => TRAY.with(|slot| *slot.borrow_mut() = Some(tray)),
        Err(error) => eprintln!("failed to create tray icon: {error}"),
    }

    // --- Timers ----------------------------------------------------------

    // Trailing-edge debounce: each slider move restarts this single-shot
    // timer, so a fast drag writes only the final value. DDC/CI is slow and
    // serial; a periodic flush would queue stale values that keep changing
    // the screen long after the drag ended.
    WRITE_TIMER.with(|cell| {
        let timer = Timer::default();
        timer.start(TimerMode::SingleShot, WRITE_DEBOUNCE, flush_writes);
        *cell.borrow_mut() = Some(timer);
    });

    // While the panel is open, keep values in sync with external changes
    // (keyboard brightness keys, other apps, monitor OSD).
    let poll_timer = slint::Timer::default();
    poll_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(3000),
        move || {
            STATE.with(|cell| {
                let state = cell.borrow();
                if !state.visible {
                    return;
                }
                if state
                    .last_touch
                    .is_some_and(|touched| touched.elapsed() < INTERACTION_GRACE)
                {
                    return;
                }
                if !state.pending.is_empty() || state.pending_all.is_some() {
                    return;
                }
                if let Some(hub) = &state.hub {
                    let _ = hub.send(Command::Poll);
                }
            });
        },
    );

    let startup = hub.clone();
    let startup_timer = slint::Timer::default();
    startup_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(30),
        move || {
            let _ = startup.send(Command::Refresh);
        },
    );

    // Dismiss the panel when the user clicks anywhere else, like a native
    // tray flyout. The panel itself keeps foreground while it is interacted
    // with, so a mismatch means focus moved to another window.
    let focus_weak = main_weak.clone();
    let focus_timer = slint::Timer::default();
    focus_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(300),
        move || {
            let Some(win) = focus_weak.upgrade() else {
                return;
            };
            if !win.window().is_visible() {
                return;
            }
            let panel = STATE.with(|cell| cell.borrow().panel_hwnd);
            let Some(panel) = panel else {
                return;
            };
            let foreground = foreground_hwnd();
            if foreground == 0 || foreground == panel {
                return;
            }
            let _ = win.window().hide();
            STATE.with(|cell| cell.borrow_mut().visible = false);
        },
    );

    // Dev-only flags used for automated screenshot checks:
    //   --dev-show      open the brightness panel above the bottom-right corner
    //   --dev-settings  open the settings window
    //   --dev-fail      with --dev-show, mark the last monitor as unwritable
    let dev_show = std::env::args().any(|arg| arg == "--dev-show");
    let dev_settings = std::env::args().any(|arg| arg == "--dev-settings");
    let dev_fail = std::env::args().any(|arg| arg == "--dev-fail");
    let mut _dev_timers: Vec<slint::Timer> = Vec::new();
    if dev_show || dev_settings {
        let panel_weak = main_weak.clone();
        let dev_settings_weak = settings_weak.clone();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(900),
            move || {
                if dev_settings {
                    if let Some(window) = dev_settings_weak.upgrade() {
                        show_settings(&window);
                    }
                } else if let Some(win) = panel_weak.upgrade() {
                    let (width, height) = primary_screen_size();
                    let rect = Rect {
                        size: tray_icon::dpi::PhysicalSize::new(24u32, 24u32),
                        position: tray_icon::dpi::PhysicalPosition::new(
                            f64::from(width - 40),
                            f64::from(height - 24),
                        ),
                    };
                    toggle_popup(&win.as_weak(), &rect);
                }
            },
        );
        _dev_timers.push(timer);

        if dev_show && dev_fail {
            let fail_weak = main_weak.clone();
            let fail_timer = slint::Timer::default();
            fail_timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_millis(1600),
                move || {
                    if fail_weak.upgrade().is_some() {
                        STATE.with(|cell| {
                            let state = cell.borrow();
                            if let Some(hub) = &state.hub {
                                let _ = hub.send(Command::DevFailLast);
                            }
                        });
                    }
                },
            );
            _dev_timers.push(fail_timer);
        }
    }

    // Tray-only daemon: Window::run() would show() the main window first,
    // popping a visible window on launch. Our tray icon comes from the
    // `tray-icon` crate, invisible to Slint, so run_event_loop() could exit
    // when no window is visible. run_event_loop_until_quit() keeps the loop
    // alive with zero visible windows until a tray callback calls quit().
    slint::run_event_loop_until_quit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "brightwheel-test-{}-{nanos}-{tag}.txt",
            std::process::id()
        ))
    }

    #[test]
    fn clamps_brightness_values() {
        assert_eq!(clamp_percent(-12.0), 0);
        assert_eq!(clamp_percent(0.0), 0);
        assert_eq!(clamp_percent(42.4), 42);
        assert_eq!(clamp_percent(42.6), 43);
        assert_eq!(clamp_percent(100.0), 100);
        assert_eq!(clamp_percent(180.0), 100);
        assert_eq!(clamp_percent(f32::NAN), 0);
        assert_eq!(clamp_percent(f32::INFINITY), 0);
    }

    #[test]
    fn master_slider_prefers_absolute_target() {
        let mut state = AppState::new();
        assert_eq!(displayed_master(&state, 47), 47);
        state.master_setpoint = Some(80);
        // The measured average must not pull an absolute master target back.
        assert_eq!(displayed_master(&state, 47), 80);
        assert_eq!(displayed_master(&state, 100), 80);
        state.master_setpoint = None;
        assert_eq!(displayed_master(&state, 55), 55);
    }

    #[test]
    fn write_debounce_is_shorter_than_poll_and_interaction_grace() {
        // Long enough to swallow a fast drag, short enough to feel instant.
        assert!(WRITE_DEBOUNCE >= Duration::from_millis(80));
        assert!(WRITE_DEBOUNCE <= Duration::from_millis(200));
        assert!(WRITE_DEBOUNCE < INTERACTION_GRACE);
    }

    #[test]
    fn averages_supported_monitors() {
        assert_eq!(average_percent(&[]), None);
        assert_eq!(average_percent(&[80]), Some(80));
        assert_eq!(average_percent(&[100, 50]), Some(75));
        assert_eq!(average_percent(&[33, 34]), Some(34));
        assert_eq!(average_percent(&[0, 100, 100]), Some(67));
    }

    #[test]
    fn panel_height_matches_slint_layout() {
        assert_eq!(panel_height(0, false), 206.0);
        assert_eq!(panel_height(1, false), 168.0);
        assert_eq!(panel_height(2, true), 298.0);
        assert_eq!(panel_height(3, true), 364.0);
    }

    #[test]
    fn places_panel_above_a_bottom_tray_and_clamps_to_screen() {
        let tray = (1800, 1040, 32, 32);
        let popup = (372, 298);
        let screen = (0, 0, 1920, 1080);
        assert_eq!(popup_position(tray, popup, screen), (1536, 732));
    }

    #[test]
    fn flips_panel_below_a_top_tray() {
        let tray = (500, 0, 32, 32);
        let popup = (372, 298);
        let screen = (0, 0, 1920, 1080);
        assert_eq!(popup_position(tray, popup, screen), (330, 42));
    }

    #[test]
    fn keeps_panel_inside_a_tiny_screen() {
        let tray = (10, 10, 20, 20);
        let popup = (400, 400);
        let screen = (0, 0, 300, 300);
        assert_eq!(popup_position(tray, popup, screen), (12, 12));
    }

    #[test]
    fn detects_the_primary_display_name() {
        assert!(is_primary_display(r"\\.\DISPLAY1"));
        assert!(is_primary_display(r"\\.\display1"));
        assert!(!is_primary_display(r"\\.\DISPLAY2"));
        assert!(!is_primary_display(r"\\.\DISPLAY12"));
        assert!(!is_primary_display(""));
    }

    #[test]
    fn formats_monitor_labels() {
        assert_eq!(monitor_label(0, "DELL U2720Q", true), "1. DELL U2720Q（主）");
        assert_eq!(monitor_label(1, " LG 27UL850 ", false), "2. LG 27UL850");
        assert_eq!(monitor_label(2, "", false), "3. 显示器");
    }

    #[test]
    fn extracts_monitor_model_from_device_path() {
        assert_eq!(
            display_model_from_path(
                r"\\?\DISPLAY#LEN8BA1#4&2c6c98d4&0&UID8388688#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
            ),
            Some("LEN8BA1".to_string())
        );
        assert_eq!(
            display_model_from_path(r"\\?\DISPLAY#XMID005#4&2c6c98d4&0&UID4145#{guid}"),
            Some("XMID005".to_string())
        );
        assert_eq!(display_model_from_path(r"\\?\DISPLAY##4&2c6c98d4#{guid}"), None);
        assert_eq!(display_model_from_path("garbage"), None);
    }

    #[test]
    fn icon_color_tracks_brightness() {
        assert_eq!(icon_color(0), (232, 150, 46));
        assert_eq!(icon_color(100), (255, 205, 72));
        assert_eq!(icon_color(250), (255, 205, 72));
    }

    #[test]
    fn tray_icon_has_expected_size_and_transparent_background() {
        let a = icon_rgba(50);
        assert_eq!(a.len(), ICON_N * ICON_N * 4);
        // Corner is fully transparent.
        assert_eq!(a[3], 0);
        // Center is the solid sun core.
        let center = ((ICON_N / 2) * ICON_N + ICON_N / 2) * 4;
        assert_eq!(a[center + 3], 255);
    }

    #[test]
    fn tray_icon_progress_ring_grows_with_brightness() {
        // Sample the 12-o'clock ring pixel just past the start: at 0% the
        // unfilled arc is transparent (no dark frame), at 100% it is golden.
        let sample = (2 * ICON_N + ICON_N / 2) * 4;
        let lo = icon_rgba(0);
        let hi = icon_rgba(100);
        assert_eq!(lo[sample + 3], 0, "unfilled arc must be transparent");
        assert_eq!(&hi[sample..sample + 3], &[255, 205, 72]);
        assert_eq!(hi[sample + 3], 255);
    }

    #[test]
    fn prefs_default_to_showing_percentages() {
        let path = temp_settings_path("defaults");
        let prefs = Prefs::load_from(&path);
        assert!(prefs.show_percent);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prefs_roundtrip_and_ignore_unknown_keys() {
        let path = temp_settings_path("roundtrip");
        std::fs::write(&path, "something_else=1\nshow_percent=0\n").expect("write settings");
        let prefs = Prefs::load_from(&path);
        assert!(!prefs.show_percent);

        let prefs = Prefs {
            show_percent: true,
        };
        prefs.save_to(&path);
        assert!(Prefs::load_from(&path).show_percent);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn strips_monitor_index_prefix() {
        assert_eq!(strip_index_prefix("3. RTK2775"), "RTK2775");
        assert_eq!(strip_index_prefix("12. LEN8BA1（主）"), "LEN8BA1（主）");
        assert_eq!(strip_index_prefix("显示器"), "显示器");
        assert_eq!(strip_index_prefix("3.RTK2775"), "3.RTK2775");
    }

    #[test]
    fn snapshot_merging_respects_recent_user_input() {
        let old = MonitorItem {
            index: 0,
            name: "1. Test".into(),
            percent: 40,
            supported: true,
            detail: "".into(),
        };
        let polled = MonSnapshot {
            name: "1. Test".into(),
            percent: 70,
            supported: true,
            detail: "".into(),
        };

        // While the user is dragging, keep the value they are holding.
        assert_eq!(merged_percent(true, Some(&old), &polled), 40);
        // Without recent input, the polled hardware value wins.
        assert_eq!(merged_percent(false, Some(&old), &polled), 70);
        // Nothing to preserve yet.
        assert_eq!(merged_percent(true, None, &polled), 70);

        let unsupported_old = MonitorItem {
            index: 0,
            name: "1. Test".into(),
            percent: 0,
            supported: false,
            detail: "no ddc".into(),
        };
        assert_eq!(merged_percent(true, Some(&unsupported_old), &polled), 70);

        let unsupported_poll = MonSnapshot {
            name: "1. Test".into(),
            percent: 0,
            supported: false,
            detail: "no ddc".into(),
        };
        assert_eq!(merged_percent(true, Some(&old), &unsupported_poll), 0);
    }

    #[test]
    fn interaction_grace_expires() {
        let recent = Some(Instant::now());
        assert!(recent.is_some_and(|touched| touched.elapsed() < INTERACTION_GRACE));

        let old = Some(Instant::now() - INTERACTION_GRACE - Duration::from_millis(10));
        assert!(!old.is_some_and(|touched| touched.elapsed() < INTERACTION_GRACE));
    }

    /// Read-only hardware probe. Run with:
    /// `cargo test probe_monitors -- --ignored --nocapture`
    /// It verifies that enumeration works from a non-main thread (the real app
    /// uses a worker thread) without changing any brightness values.
    #[test]
    #[ignore]
    fn probe_monitors() {
        let entries = enumerate_devices(&HashSet::new());
        println!("monitors: {}", entries.len());
        for entry in &entries {
            println!(
                "  {} supported={} percent={} detail={}",
                entry.name, entry.supported, entry.percent, entry.detail
            );
            if let Some(device) = &entry.device {
                println!(
                    "    description={:?} path={:?}",
                    device.device_description(),
                    device.device_path()
                );
            }
        }
        assert!(!entries.is_empty(), "no monitors enumerated");
    }
}

