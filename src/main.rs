slint::include_modules!();

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::Result;
use brightness::blocking::{Brightness, BrightnessDevice, brightness_devices};
use slint::winit_030::winit::dpi::PhysicalPosition as WinitPhysicalPosition;
use slint::winit_030::{winit::window::WindowLevel, WinitWindowAccessor};
use slint::{LogicalSize, ModelRc, PhysicalPosition, VecModel, WindowPosition, WindowSize};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, Rect, TrayIcon, TrayIconBuilder, TrayIconEvent};

const MENU_SETTINGS: &str = "brightwheel.menu.settings";
const MENU_REFRESH: &str = "brightwheel.menu.refresh";
const MENU_QUIT: &str = "brightwheel.menu.quit";

static TRAY_PERCENT: AtomicU32 = AtomicU32::new(100);

struct MonitorState {
    device: BrightnessDevice,
    supported: bool,
}

thread_local! {
    static MONITORS: RefCell<Vec<Option<MonitorState>>> = RefCell::new(Vec::new());
    // tray-icon / muda types are !Send, so keep them on the UI thread.
    static TRAY: RefCell<Option<TrayIcon>> = RefCell::new(None);
    static MENU_ITEMS: RefCell<Vec<MenuItem>> = RefCell::new(Vec::new());
}

#[derive(Clone, Copy)]
struct Prefs {
    show_percent: bool,
}

impl Prefs {
    fn load() -> Self {
        let mut show_percent = true;
        if let Ok(text) = std::fs::read_to_string(Self::path()) {
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("show_percent=") {
                    show_percent = value.trim() == "1";
                }
            }
        }
        Self { show_percent }
    }

    fn save(&self) {
        let path = Self::path();
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

fn make_icon(percent: u32) -> Icon {
    const N: usize = 32;
    let mut rgba = vec![0u8; N * N * 4];
    let t = (percent.min(100) as f32) / 100.0;
    let r = (205.0 + 50.0 * t) as u8;
    let g = (150.0 + 55.0 * t) as u8;
    let b = (60.0 - 40.0 * t) as u8;

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
    TRAY_PERCENT.store(percent, Ordering::Relaxed);
    TRAY.with(|slot| {
        if let Some(tray) = slot.borrow().as_ref() {
            let _ = tray.set_icon(Some(make_icon(percent)));
        }
    });
}

fn configure_window(window: &slint::Window) {
    window.with_winit_window(|native| {
        native.set_decorations(false);
        native.set_resizable(false);
        native.set_window_level(WindowLevel::AlwaysOnTop);
    });
}

fn place_popup(window: &slint::Window, rect: &Rect) {
    let size = window.size();
    let rect_width = rect.size.width as f64;
    let rect_height = rect.size.height as f64;
    let x = rect.position.x + rect_width / 2.0 - size.width as f64 / 2.0;
    let y = if rect.position.y > 120.0 {
        rect.position.y - size.height as f64 - 10.0
    } else {
        rect.position.y + rect_height + 10.0
    };
    let (x, y) = (x.round() as i32, y.round() as i32);

    window.set_position(WindowPosition::Physical(PhysicalPosition::new(x, y)));
    window.with_winit_window(|native| {
        native.set_outer_position(WinitPhysicalPosition::new(x, y));
    });
}

fn refresh_monitors(win: &MainWindow) {
    let mut items = Vec::new();
    let mut states: Vec<Option<MonitorState>> = Vec::new();
    let mut first_percent = None;

    for result in brightness_devices() {
        let index = states.len();
        match result {
            Ok(device) => {
                let device_name = device.device_name().unwrap_or_default();
                let friendly = device
                    .friendly_device_name()
                    .unwrap_or_else(|_| device_name.clone());
                let primary = device_name.ends_with("DISPLAY1");
                let label = format!(
                    "{}. {}{}",
                    index + 1,
                    friendly,
                    if primary { "  (主)" } else { "" }
                );

                match device.get() {
                    Ok(percent) => {
                        if first_percent.is_none() {
                            first_percent = Some(percent);
                        }
                        items.push(MonitorItem {
                            index: index as i32,
                            name: label.into(),
                            percent: percent as i32,
                            supported: true,
                            detail: "".into(),
                        });
                        states.push(Some(MonitorState {
                            device,
                            supported: true,
                        }));
                    }
                    Err(error) => {
                        items.push(MonitorItem {
                            index: index as i32,
                            name: label.into(),
                            percent: 0,
                            supported: false,
                            detail: error.to_string().into(),
                        });
                        states.push(Some(MonitorState {
                            device,
                            supported: false,
                        }));
                    }
                }
            }
            Err(error) => {
                items.push(MonitorItem {
                    index: index as i32,
                    name: format!("显示器 {}", index + 1).into(),
                    percent: 0,
                    supported: false,
                    detail: error.to_string().into(),
                });
                states.push(None);
            }
        }
    }

    let height = 88.0 + 74.0 * items.len() as f32;
    win.window()
        .set_size(WindowSize::Logical(LogicalSize::new(360.0, height)));
    win.set_status_text(
        format!("已发现 {} 个显示器 · 拖动滑块调整", items.len()).into(),
    );
    win.set_monitors(ModelRc::new(VecModel::from(items)));
    MONITORS.with(|monitors| *monitors.borrow_mut() = states);
    if let Some(percent) = first_percent {
        paint_tray(percent);
    }
}

fn toggle_popup(weak: &slint::Weak<MainWindow>, rect: &Rect) {
    let Some(win) = weak.upgrade() else {
        return;
    };
    let window = win.window();
    if window.is_visible() {
        let _ = window.hide();
        return;
    }

    refresh_monitors(&win);
    place_popup(window, rect);
    configure_window(window);
    let _ = window.show();
    place_popup(window, rect);
}

fn main() -> Result<()> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .select()?;

    let main_win = MainWindow::new()?;
    let settings = SettingsWindow::new()?;
    let prefs = Rc::new(RefCell::new(Prefs::load()));
    let app_version = format!("BrightWheel v{}", env!("CARGO_PKG_VERSION"));

    settings.set_version_text(app_version.clone().into());
    settings.set_show_percent(prefs.borrow().show_percent);
    settings.set_autostart_enabled(brightwheel::autostart::is_enabled().unwrap_or(false));
    main_win.set_show_percent(prefs.borrow().show_percent);

    let main_weak = main_win.as_weak();
    let settings_weak = settings.as_weak();

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
            {
                if button == MouseButton::Left && button_state == MouseButtonState::Up {
                    toggle_popup(&weak, &rect);
                }
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
                if let Some(window) = settings.upgrade() {
                    let _ = window.window().show();
                }
            } else if event.id == ids.1 {
                if let Some(window) = main.upgrade() {
                    refresh_monitors(&window);
                }
            } else if event.id == ids.2 {
                let _ = slint::quit_event_loop();
            }
        });
    }));

    refresh_monitors(&main_win);
    let _ = main_win.window().show();
    configure_window(main_win.window());
    let _ = main_win.window().hide();

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
        .with_icon(make_icon(TRAY_PERCENT.load(Ordering::Relaxed)))
        .with_tooltip(app_version)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
    {
        Ok(tray) => TRAY.with(|slot| *slot.borrow_mut() = Some(tray)),
        Err(error) => eprintln!("failed to create tray icon: {error}"),
    }

    let brightness_main = main_weak.clone();
    main_win.on_brightness_changed(move |index, value| {
        let percent = value.round() as u32;
        let status = MONITORS.with(|monitors| {
            let monitors = monitors.borrow();
            match monitors.get(index as usize) {
                Some(Some(state)) if state.supported => match state.device.set(percent) {
                    Ok(()) => {
                        if index == 0 {
                            paint_tray(percent);
                        }
                        format!("BrightWheel · 显示器 {} → {}%", index + 1, percent)
                    }
                    Err(error) => format!("设置失败: {error}"),
                },
                Some(Some(_)) => "该显示器不支持亮度调节".to_string(),
                _ => "显示器列表已变化，请重新扫描".to_string(),
            }
        });
        if let Some(window) = brightness_main.upgrade() {
            window.set_status_text(status.into());
        }
    });

    let refresh_weak = main_weak.clone();
    main_win.on_do_refresh(move || {
        if let Some(window) = refresh_weak.upgrade() {
            refresh_monitors(&window);
        }
    });

    let settings_opener = settings_weak.clone();
    main_win.on_open_settings(move || {
        if let Some(window) = settings_opener.upgrade() {
            let _ = window.window().show();
        }
    });

    let quit_on_main = main_weak.clone();
    main_win.on_do_quit(move || {
        if let Some(window) = quit_on_main.upgrade() {
            let _ = window.window().hide();
        }
        let _ = slint::quit_event_loop();
    });

    let autostart_weak = settings_weak.clone();
    settings.on_autostart_changed(move |enabled| {
        if let Err(error) = brightwheel::autostart::set_enabled(enabled) {
            if let Some(window) = autostart_weak.upgrade() {
                window.set_autostart_enabled(!enabled);
                window.set_version_text(format!("开机自启设置失败: {error}").into());
            }
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
        if let Some(window) = settings_refresh.upgrade() {
            refresh_monitors(&window);
        }
    });

    let settings_closer = settings_weak.clone();
    settings.on_close_requested(move || {
        if let Some(window) = settings_closer.upgrade() {
            let _ = window.window().hide();
        }
    });

    let quit_on_settings = settings_weak.clone();
    settings.on_do_quit(move || {
        if let Some(window) = quit_on_settings.upgrade() {
            let _ = window.window().hide();
        }
        let _ = slint::quit_event_loop();
    });

    main_win.run()?;
    Ok(())
}
