slint::include_modules!();

mod autostart;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use brightness::blocking::{Brightness, BrightnessDevice, brightness_devices};
use slint::winit_030::winit::dpi::PhysicalPosition as WinitPhysicalPosition;
use slint::winit_030::winit::platform::windows::WindowExtWindows;
use slint::winit_030::WinitWindowAccessor;
use slint::{LogicalSize, Model, ModelRc, PhysicalPosition, VecModel, WindowPosition, WindowSize};
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
}

struct DeviceEntry {
    device: Option<BrightnessDevice>,
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
    last_touch: Option<Instant>,
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
            last_touch: None,
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

fn monitor_label(index: usize, friendly: &str, primary: bool) -> String {
    let name = friendly.trim();
    let name = if name.is_empty() { "显示器" } else { name };
    let suffix = if primary { "（主）" } else { "" };
    format!("{}. {}{}", index + 1, name, suffix)
}

fn icon_color(percent: u32) -> (u8, u8, u8) {
    let t = percent.min(100) as f32 / 100.0;
    (
        (205.0 + 50.0 * t) as u8,
        (150.0 + 55.0 * t) as u8,
        (60.0 - 40.0 * t) as u8,
    )
}

fn make_icon(percent: u32) -> Icon {
    const N: usize = 32;
    let mut rgba = vec![0u8; N * N * 4];
    let (r, g, b) = icon_color(percent);

    for y in 0..N {
        for x in 0..N {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let distance = (dx * dx + dy * dy).sqrt();
            let angle = dy.atan2(dx);
            let ray = distance > 9.0 && distance < 14.0 && (angle * 4.0).cos().abs() > 0.96;
            if distance <= 6.5 || ray {
                let i = (y * N + x) * 4;
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, N as u32, N as u32).expect("invalid tray icon")
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

fn refresh_master(win: &MainWindow, state: &AppState) {
    let supported: Vec<u32> = (0..state.model.row_count())
        .filter_map(|index| state.model.row_data(index))
        .filter(|item| item.supported)
        .map(|item| item.percent.max(0) as u32)
        .collect();
    win.set_supported_count(supported.len() as i32);
    win.set_has_master(supported.len() > 1);
    if let Some(average) = average_percent(&supported) {
        win.set_master_percent(average as i32);
        paint_tray(average);
    }
}

fn apply_snapshot(win: &MainWindow, snapshot: Vec<MonSnapshot>, reason: SnapshotReason) {
    STATE.with(|cell| {
        let state = cell.borrow();
        let preserving = state
            .last_touch
            .is_some_and(|touched| touched.elapsed() < INTERACTION_GRACE);

        let mut items = Vec::with_capacity(snapshot.len());
        for (index, monitor) in snapshot.iter().enumerate() {
            let percent = if preserving && monitor.supported {
                state
                    .model
                    .row_data(index)
                    .filter(|old| old.supported)
                    .map(|old| old.percent)
                    .unwrap_or(monitor.percent as i32)
            } else {
                monitor.percent as i32
            };
            items.push(MonitorItem {
                index: index as i32,
                name: monitor.name.as_str().into(),
                percent,
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
            win.set_master_percent(average as i32);
            paint_tray(average);
        }
        win.set_scanning(false);

        if !preserving {
            let status = if items.is_empty() {
                "未检测到显示器".to_string()
            } else if reason == SnapshotReason::Refresh {
                format!("已发现 {} 台显示器", items.len())
            } else {
                "亮度已同步".to_string()
            };
            win.set_status_text(status.into());
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
}

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

fn enumerate_devices() -> Vec<DeviceEntry> {
    let mut entries = Vec::new();
    for result in brightness_devices() {
        let index = entries.len();
        match result {
            Ok(device) => {
                let device_name = device.device_name().unwrap_or_default();
                let friendly = device
                    .friendly_device_name()
                    .unwrap_or_else(|_| device_name.clone());
                let name = monitor_label(index, &friendly, is_primary_display(&device_name));
                match device.get() {
                    Ok(percent) => entries.push(DeviceEntry {
                        device: Some(device),
                        name,
                        percent,
                        supported: true,
                        detail: String::new(),
                    }),
                    Err(_) => entries.push(DeviceEntry {
                        device: Some(device),
                        name,
                        percent: 0,
                        supported: false,
                        detail: "该显示器不支持亮度调节".to_string(),
                    }),
                }
            }
            Err(_) => entries.push(DeviceEntry {
                device: None,
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

fn send_error(ui: &slint::Weak<MainWindow>, message: String) {
    let weak = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(win) = weak.upgrade() {
            win.set_scanning(false);
            win.set_status_text(message.into());
        }
    });
}

fn spawn_worker(ui: slint::Weak<MainWindow>) -> Sender<Command> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut devices: Vec<DeviceEntry> = Vec::new();
        while let Ok(command) = receiver.recv() {
            match command {
                Command::Refresh => {
                    devices = enumerate_devices();
                    send_snapshot(&ui, &devices, SnapshotReason::Refresh);
                }
                Command::Poll => {
                    if devices.is_empty() {
                        devices = enumerate_devices();
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
                    match device.set(percent) {
                        Ok(()) => entry.percent = percent,
                        Err(error) => send_error(&ui, format!("调节失败：{error}")),
                    }
                }
                Command::SetAll { percent } => {
                    for entry in devices.iter_mut().filter(|entry| entry.supported) {
                        let Some(device) = &entry.device else {
                            continue;
                        };
                        match device.set(percent) {
                            Ok(()) => entry.percent = percent,
                            Err(error) => {
                                send_error(&ui, format!("调节失败：{error}"));
                                break;
                            }
                        }
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
fn already_running() -> bool {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    static MUTEX: OnceLock<usize> = OnceLock::new();
    let name: Vec<u16> = "Local\\BrightWheel.SingleInstance\0".encode_utf16().collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            return false;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return true;
        }
        let _ = MUTEX.set(handle as usize);
        false
    }
}

#[cfg(not(windows))]
fn already_running() -> bool {
    false
}

fn main() -> Result<()> {
    if already_running() {
        return Ok(());
    }

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
            set_row_percent(&mut state, index, percent);
            refresh_master(&win, &state);
            queue_set(&mut state, index, percent);
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
            state.pending.remove(&index);
            set_row_percent(&mut state, index, percent);
            refresh_master(&win, &state);
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
            for index in 0..state.model.row_count() {
                set_row_percent(&mut state, index, percent);
            }
            state.pending.clear();
            state.pending_all = Some(percent);
            win.set_master_percent(percent as i32);
            paint_tray(percent);
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
            state.pending.clear();
            state.pending_all = None;
            for index in 0..state.model.row_count() {
                set_row_percent(&mut state, index, percent);
            }
            win.set_master_percent(percent as i32);
            paint_tray(percent);
            if let Some(hub) = &state.hub {
                let _ = hub.send(Command::SetAll { percent });
            }
        });
    });

    let refresh_weak = main_weak.clone();
    main_win.on_do_refresh(move || {
        if let Some(win) = refresh_weak.upgrade() {
            win.set_scanning(true);
            win.set_status_text("正在扫描显示器…".into());
        }
        STATE.with(|cell| {
            let state = cell.borrow();
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
            refresh_settings(&window);
            let _ = window.window().show();
            configure_settings_window(window.window());
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
    settings.on_show_percent_changed(move |enabled| {
        percent_prefs.borrow_mut().show_percent = enabled;
        percent_prefs.borrow().save();
        if let Some(window) = percent_main.upgrade() {
            window.set_show_percent(enabled);
        }
    });

    let settings_refresh = main_weak.clone();
    settings.on_do_refresh(move || {
        if let Some(win) = settings_refresh.upgrade() {
            win.set_scanning(true);
            win.set_status_text("正在扫描显示器…".into());
        }
        STATE.with(|cell| {
            let state = cell.borrow();
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
                    refresh_settings(&window);
                    let _ = window.window().show();
                    configure_settings_window(window.window());
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

    // Coalesce rapid slider updates so DDC/CI writes never queue up.
    let flush_timer = slint::Timer::default();
    flush_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(60),
        move || {
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
        },
    );

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

    main_win.run()?;
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
    fn icon_color_tracks_brightness() {
        assert_eq!(icon_color(0), (205, 150, 60));
        assert_eq!(icon_color(100), (255, 205, 20));
        assert_eq!(icon_color(250), (255, 205, 20));
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
    fn snapshot_merging_respects_recent_user_input() {
        // Pure logic mirror: while a drag is in progress, a polled snapshot must
        // not overwrite the percentage the user is currently holding.
        let recent = Some(Instant::now());
        let preserving = recent.is_some_and(|touched| touched.elapsed() < INTERACTION_GRACE);
        assert!(preserving);

        let old = Some(Instant::now() - INTERACTION_GRACE - Duration::from_millis(10));
        let preserving = old.is_some_and(|touched| touched.elapsed() < INTERACTION_GRACE);
        assert!(!preserving);
    }
}

