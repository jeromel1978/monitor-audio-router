#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod platform;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::io::Write;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::{w, PCWSTR};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_SINGLETHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, MessageBoxW, SetForegroundWindow, ShowWindow, MB_ICONERROR, MB_OK, SW_HIDE,
    SW_RESTORE,
};

const TRAY_OPEN_ID: &str = "open-monitor-audio-router";
const TRAY_MINIMIZE_ID: &str = "minimize-to-tray";
const TRAY_EXIT_ID: &str = "exit-monitor-audio-router";

enum TrayAction {
    Restore,
    Exit,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct MonitorRule {
    monitor: String,
    device: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct AppRule {
    process_name: String,
    device: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
struct Config {
    monitor_rules: Vec<MonitorRule>,
    app_rules: Vec<AppRule>,
    volumes: HashMap<String, u8>,
    poll_interval_ms: u64,
    only_foreground: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            monitor_rules: Vec::new(),
            app_rules: Vec::new(),
            volumes: HashMap::new(),
            poll_interval_ms: 1000,
            only_foreground: false,
        }
    }
}

struct RouterApp {
    config_path: PathBuf,
    config: Config,
    snapshot: Option<platform::Snapshot>,
    last_routes: HashMap<u32, String>,
    last_refresh: Instant,
    status: Option<String>,
    initial_volumes_applied: bool,
    legacy_rules_migrated: bool,
    tray_icon: Option<TrayIcon>,
    visible_tray_menu: Option<Menu>,
    hidden_tray_menu: Option<Menu>,
    tray_menu_matches_visibility: Option<bool>,
    tray_actions: mpsc::Receiver<TrayAction>,
    allow_exit: Arc<AtomicBool>,
}

impl RouterApp {
    fn new(
        config_path: PathBuf,
        tray_result: Result<(TrayIcon, Menu, Menu), String>,
        tray_actions: mpsc::Receiver<TrayAction>,
        allow_exit: Arc<AtomicBool>,
    ) -> Self {
        let (config, status) = match fs::read_to_string(&config_path) {
            Ok(data) => match serde_json::from_str(&data) {
                Ok(config) => (config, None),
                Err(error) => (
                    Config::default(),
                    Some(format!(
                        "Could not parse {}: {error}",
                        config_path.display()
                    )),
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                Config::default(),
                Some(format!(
                    "No config file found. A new one will be created at {}.",
                    config_path.display()
                )),
            ),
            Err(error) => (
                Config::default(),
                Some(format!("Could not read {}: {error}", config_path.display())),
            ),
        };

        let (tray_icon, visible_tray_menu, hidden_tray_menu, tray_error) = match tray_result {
            Ok((icon, visible_menu, hidden_menu)) => {
                (Some(icon), Some(visible_menu), Some(hidden_menu), None)
            }
            Err(error) => (
                None,
                None,
                None,
                Some(format!("System tray unavailable: {error}")),
            ),
        };
        let mut app = Self {
            config_path,
            config,
            snapshot: None,
            last_routes: HashMap::new(),
            last_refresh: Instant::now() - Duration::from_secs(2),
            status,
            initial_volumes_applied: false,
            legacy_rules_migrated: false,
            tray_icon,
            visible_tray_menu,
            hidden_tray_menu,
            tray_menu_matches_visibility: None,
            tray_actions,
            allow_exit,
        };
        if let Some(error) = tray_error {
            app.status = Some(error);
        }
        app.refresh();
        app
    }

    fn handle_tray_events(&mut self, ctx: &egui::Context) {
        let mut should_exit = false;
        let mut should_restore = false;
        for action in self.tray_actions.try_iter() {
            match action {
                TrayAction::Restore => should_restore = true,
                TrayAction::Exit => should_exit = true,
            }
        }

        if should_exit {
            self.allow_exit.store(true, Ordering::Release);
            self.save_config();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if should_restore && !restore_main_window() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn sync_restore_menu(&mut self) {
        let Some(window_visible) = main_window_visible() else {
            return;
        };
        if self.tray_menu_matches_visibility == Some(window_visible) {
            return;
        }
        let Some(tray_icon) = &self.tray_icon else {
            return;
        };
        let menu = if window_visible {
            self.visible_tray_menu.as_ref()
        } else {
            self.hidden_tray_menu.as_ref()
        };
        let Some(menu) = menu else {
            return;
        };
        tray_icon.set_menu(Some(Box::new(menu.clone())));
        self.tray_menu_matches_visibility = Some(window_visible);
    }

    fn hide_to_tray_on_close(&self, ctx: &egui::Context) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested && !self.allow_exit.load(Ordering::Acquire) && self.tray_icon.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !hide_main_window() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }
    }

    fn refresh(&mut self) {
        match platform::snapshot() {
            Ok(snapshot) => {
                if !self.legacy_rules_migrated && !snapshot.monitors.is_empty() {
                    self.migrate_legacy_rules(&snapshot.monitors);
                    self.legacy_rules_migrated = true;
                }
                if !self.initial_volumes_applied {
                    self.apply_configured_volumes(&snapshot);
                    self.initial_volumes_applied = true;
                }
                self.apply_monitor_routes(&snapshot);
                self.snapshot = Some(snapshot);
                self.last_refresh = Instant::now();
            }
            Err(error) => {
                self.status = Some(format!("Windows audio discovery failed: {error}"));
                self.last_refresh = Instant::now();
            }
        }
    }

    fn migrate_legacy_rules(&mut self, monitors: &[platform::Monitor]) {
        let mut changed = false;
        for rule in &mut self.config.monitor_rules {
            if let Some(monitor) = monitors
                .iter()
                .find(|monitor| monitor.display_name.eq_ignore_ascii_case(&rule.monitor))
            {
                if rule.monitor != monitor.id {
                    rule.monitor = monitor.id.clone();
                    changed = true;
                }
            }
        }

        let legacy_rules: HashMap<String, String> = self
            .config
            .monitor_rules
            .iter()
            .filter(|rule| {
                rule.monitor.eq_ignore_ascii_case("primary")
                    || rule.monitor.eq_ignore_ascii_case("secondary")
            })
            .map(|rule| (rule.monitor.to_lowercase(), rule.device.clone()))
            .collect();
        if legacy_rules.is_empty() {
            if changed {
                self.save_config();
            }
            return;
        }

        for monitor in monitors {
            if self
                .config
                .monitor_rules
                .iter()
                .any(|rule| rule.monitor.eq_ignore_ascii_case(&monitor.id))
            {
                continue;
            }
            let legacy_key = if monitor.is_primary {
                "primary"
            } else {
                "secondary"
            };
            if let Some(device) = legacy_rules.get(legacy_key) {
                self.config.monitor_rules.push(MonitorRule {
                    monitor: monitor.id.clone(),
                    device: device.clone(),
                });
                changed = true;
            }
        }
        let rule_count = self.config.monitor_rules.len();
        self.config.monitor_rules.retain(|rule| {
            !rule.monitor.eq_ignore_ascii_case("primary")
                && !rule.monitor.eq_ignore_ascii_case("secondary")
        });
        changed |= self.config.monitor_rules.len() != rule_count;
        if changed {
            self.save_config();
        }
    }

    fn apply_configured_volumes(&mut self, snapshot: &platform::Snapshot) {
        let mut errors = Vec::new();
        for device in &snapshot.devices {
            let configured = self
                .config
                .volumes
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(&device.id))
                .or_else(|| {
                    self.config.volumes.iter().find(|(key, _)| {
                        !key.trim().is_empty()
                            && device.name.to_lowercase().contains(&key.to_lowercase())
                    })
                });
            if let Some((_, level)) = configured {
                if let Err(error) = platform::set_output_volume(device, f32::from(*level) / 100.0) {
                    errors.push(format!("{}: {error}", device.name));
                }
            }
        }
        if !errors.is_empty() {
            self.status = Some(format!(
                "Could not apply configured output volume: {}",
                errors.join("; ")
            ));
        }
    }

    fn apply_monitor_routes(&mut self, snapshot: &platform::Snapshot) {
        let mut active_processes = HashMap::new();
        let mut errors = Vec::new();
        for session in snapshot
            .devices
            .iter()
            .flat_map(|device| device.sessions.iter())
        {
            active_processes.insert(session.process_id, ());
            let monitor_id = snapshot.window_monitors.get(&session.process_id);
            let Some(monitor_id) = monitor_id else {
                continue;
            };
            let monitor_rule = self
                .config
                .monitor_rules
                .iter()
                .find(|rule| rule.monitor.eq_ignore_ascii_case(monitor_id));
            let app_rule = self.config.app_rules.iter().find(|rule| {
                rule.process_name
                    .eq_ignore_ascii_case(&session.process_name)
            });
            let Some(target_device) = monitor_rule
                .map(|rule| rule.device.as_str())
                .or_else(|| app_rule.map(|rule| rule.device.as_str()))
            else {
                continue;
            };
            let output = if target_device.trim().is_empty() {
                snapshot.default_device_id.as_ref().and_then(|id| {
                    snapshot
                        .devices
                        .iter()
                        .find(|device| device.id.eq_ignore_ascii_case(id))
                })
            } else {
                Self::output_for_rule(Some(target_device), &snapshot.devices)
            };

            let Some(output) = output else {
                continue;
            };
            if self
                .last_routes
                .get(&session.process_id)
                .is_some_and(|last| last == &output.id)
            {
                continue;
            }
            match platform::route_process(session.process_id, &output.id) {
                Ok(()) => {
                    self.last_routes
                        .insert(session.process_id, output.id.clone());
                }
                Err(error) => errors.push(format!("{}: {error}", session.process_name)),
            }
        }
        self.last_routes
            .retain(|process_id, _| active_processes.contains_key(process_id));
        if !errors.is_empty() {
            self.status = Some(format!(
                "Could not apply one or more app routes: {}",
                errors.join("; ")
            ));
        }
    }

    fn set_monitor_route(&mut self, monitor: &str, device_id: Option<String>) {
        self.config
            .monitor_rules
            .retain(|rule| !rule.monitor.eq_ignore_ascii_case(monitor));
        self.config.monitor_rules.push(MonitorRule {
            monitor: monitor.to_string(),
            device: device_id.unwrap_or_default(),
        });
        self.last_routes.clear();
        self.save_config();
    }

    fn save_config(&mut self) {
        match serde_json::to_string_pretty(&self.config) {
            Ok(data) => match write_config_atomically(&self.config_path, data.as_bytes()) {
                Ok(()) => self.status = None,
                Err(error) => {
                    self.status = Some(format!(
                        "Could not save {}: {error}",
                        self.config_path.display()
                    ));
                }
            },
            Err(error) => self.status = Some(format!("Could not serialize config: {error}")),
        }
    }

    fn output_for_rule<'a>(
        rule: Option<&str>,
        devices: &'a [platform::AudioDevice],
    ) -> Option<&'a platform::AudioDevice> {
        let rule = rule?.trim();
        if rule.is_empty() {
            return None;
        }
        if let Some(device) = devices
            .iter()
            .find(|device| device.id.eq_ignore_ascii_case(rule))
        {
            return Some(device);
        }
        let rule_lower = rule.to_lowercase();
        let mut matches = devices.iter().filter(|device| {
            device.name.eq_ignore_ascii_case(rule)
                || device.name.to_lowercase().contains(&rule_lower)
        });
        let device = matches.next()?;
        matches.next().is_none().then_some(device)
    }

    fn output_label(
        device: &platform::AudioDevice,
        devices: &[platform::AudioDevice],
        default_device_id: Option<&str>,
    ) -> String {
        let duplicate_name = devices
            .iter()
            .filter(|other| other.name.eq_ignore_ascii_case(&device.name))
            .count()
            > 1;
        let mut label = if duplicate_name {
            let suffix: String = device.id.chars().rev().take(8).collect();
            format!(
                "{} ({})",
                device.name,
                suffix.chars().rev().collect::<String>()
            )
        } else {
            device.name.clone()
        };
        if default_device_id.is_some_and(|id| id.eq_ignore_ascii_case(&device.id)) {
            label.push_str(" (Windows default)");
        }
        label
    }

    fn render(&mut self, ctx: &egui::Context) {
        let mut monitor_changes = Vec::new();
        let mut volume_changes = Vec::new();
        let mut errors = Vec::new();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Monitor Audio Router");
            ui.label("Active audio sources are grouped under their current output.");
            ui.small(format!("Settings: {}", self.config_path.display()));

            if let Some(status) = &self.status {
                ui.colored_label(egui::Color32::YELLOW, status);
            }

            let Some(snapshot) = self.snapshot.as_mut() else {
                ui.spinner();
                ui.label("Waiting for Windows audio devices...");
                return;
            };

            ui.separator();
            ui.heading("Monitor routing");
            if snapshot.monitors.is_empty() {
                ui.label("No monitors detected.");
            }
            for monitor in &snapshot.monitors {
                let label = if monitor.is_primary {
                    format!(
                        "{} — {} (primary)",
                        monitor.friendly_name, monitor.display_name
                    )
                } else {
                    format!("{} — {}", monitor.friendly_name, monitor.display_name)
                };
                let saved_rule = self
                    .config
                    .monitor_rules
                    .iter()
                    .find(|rule| rule.monitor.eq_ignore_ascii_case(&monitor.id))
                    .map(|rule| rule.device.as_str());
                let mut selected = Self::output_for_rule(saved_rule, &snapshot.devices)
                    .map(|device| device.id.clone());
                let selected_label = selected
                    .as_ref()
                    .and_then(|id| snapshot.devices.iter().find(|device| &device.id == id))
                    .map(|device| {
                        Self::output_label(
                            device,
                            &snapshot.devices,
                            snapshot.default_device_id.as_deref(),
                        )
                    })
                    .or_else(|| {
                        saved_rule
                            .filter(|rule| !rule.trim().is_empty())
                            .map(|rule| format!("Unavailable or ambiguous: {rule}"))
                    })
                    .unwrap_or_else(|| "System default".to_string());

                ui.horizontal(|ui| {
                    ui.label(label);
                    let mut chosen_output = None;
                    egui::ComboBox::from_id_salt(("monitor-output", &monitor.id))
                        .selected_text(selected_label)
                        .show_ui(ui, |ui| {
                            let response =
                                ui.selectable_value(&mut selected, None, "System default");
                            if response.clicked() {
                                chosen_output = Some(None);
                            }
                            for device in &snapshot.devices {
                                let output_id = Some(device.id.clone());
                                let response = ui.selectable_value(
                                    &mut selected,
                                    output_id.clone(),
                                    Self::output_label(
                                        device,
                                        &snapshot.devices,
                                        snapshot.default_device_id.as_deref(),
                                    ),
                                );
                                if response.clicked() {
                                    chosen_output = Some(output_id);
                                }
                            }
                        });
                    if let Some(device_id) = chosen_output {
                        monitor_changes.push((monitor.id.clone(), device_id));
                    }
                });
            }

            ui.separator();
            ui.heading("Audio outputs and active sources");
            if snapshot.devices.is_empty() {
                ui.label("No active playback outputs detected.");
            }
            let output_labels: HashMap<String, String> = snapshot
                .devices
                .iter()
                .map(|device| {
                    (
                        device.id.clone(),
                        Self::output_label(
                            device,
                            &snapshot.devices,
                            snapshot.default_device_id.as_deref(),
                        ),
                    )
                })
                .collect();
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for device in &mut snapshot.devices {
                        egui::CollapsingHeader::new(&output_labels[&device.id])
                            .default_open(true)
                            .show(ui, |ui| {
                                let mut output_volume = device.volume;
                                if ui
                                    .add(
                                        egui::Slider::new(&mut output_volume, 0.0..=1.0)
                                            .text("Output volume")
                                            .show_value(true),
                                    )
                                    .changed()
                                {
                                    match platform::set_output_volume(device, output_volume) {
                                        Ok(()) => volume_changes.push((
                                            device.id.clone(),
                                            (output_volume * 100.0).round() as u8,
                                        )),
                                        Err(error) => errors.push(format!(
                                            "{} output volume: {error}",
                                            device.name
                                        )),
                                    }
                                }

                                if device.sessions.is_empty() {
                                    ui.label("No active audio sources.");
                                }
                                for session in &mut device.sessions {
                                    ui.horizontal(|ui| {
                                        ui.label(format!(
                                            "{} (PID {})",
                                            session.process_name, session.process_id
                                        ));
                                        let mut session_volume = session.volume;
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut session_volume, 0.0..=1.0)
                                                    .show_value(true),
                                            )
                                            .changed()
                                        {
                                            match platform::set_session_volume(
                                                session,
                                                session_volume,
                                            ) {
                                                Ok(()) => session.volume = session_volume,
                                                Err(error) => errors.push(format!(
                                                    "{} session volume: {error}",
                                                    session.process_name
                                                )),
                                            }
                                        }
                                    });
                                }
                            });
                    }

                    if !snapshot.warnings.is_empty() {
                        ui.separator();
                        ui.heading("Discovery warnings");
                        for warning in &snapshot.warnings {
                            ui.colored_label(egui::Color32::YELLOW, warning);
                        }
                    }
                });
        });

        for (monitor, device_id) in monitor_changes {
            self.set_monitor_route(&monitor, device_id);
        }
        for (device_id, volume) in volume_changes {
            self.config.volumes.insert(device_id, volume);
            self.save_config();
        }
        if !errors.is_empty() {
            self.status = Some(errors.join("\n"));
        }
    }
}

impl eframe::App for RouterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray_events(ctx);
        self.hide_to_tray_on_close(ctx);
        self.sync_restore_menu();
        let interval = Duration::from_millis(self.config.poll_interval_ms.clamp(250, 5000));
        if self.last_refresh.elapsed() >= interval {
            self.refresh();
        }
        self.render(ctx);
        ctx.request_repaint_after(Duration::from_millis(200).min(interval));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_config();
        self.tray_icon.take();
    }
}

fn write_config_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut temporary_path = path.as_os_str().to_os_string();
    temporary_path.push(".tmp");
    let temporary_path = PathBuf::from(temporary_path);

    let result = (|| {
        let mut file = fs::File::create(&temporary_path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary_path, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn create_tray_icon() -> Result<(TrayIcon, Menu, Menu), String> {
    let open_item = MenuItem::with_id(TRAY_OPEN_ID, "Restore Window", true, None);
    let minimize_item = MenuItem::with_id(TRAY_MINIMIZE_ID, "Minimize to Tray", true, None);
    let visible_exit_item = MenuItem::with_id(TRAY_EXIT_ID, "Exit App", true, None);
    let hidden_exit_item = MenuItem::with_id(TRAY_EXIT_ID, "Exit App", true, None);
    let visible_menu = Menu::new();
    visible_menu
        .append(&minimize_item)
        .map_err(|error| format!("could not add Minimize to Tray menu item: {error}"))?;
    visible_menu
        .append(&visible_exit_item)
        .map_err(|error| format!("could not add Exit menu item: {error}"))?;
    let hidden_menu = Menu::new();
    hidden_menu
        .append(&open_item)
        .map_err(|error| format!("could not add Open menu item: {error}"))?;
    hidden_menu
        .append(&hidden_exit_item)
        .map_err(|error| format!("could not add Exit menu item: {error}"))?;

    let rgba = make_icon_pixels(16);
    let icon = Icon::from_rgba(rgba, 16, 16)
        .map_err(|error| format!("could not create tray icon: {error}"))?;
    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(hidden_menu.clone()))
        .with_icon(icon)
        .with_tooltip("Monitor Audio Router")
        .with_menu_on_left_click(false)
        .build()
        .map_err(|error| format!("could not create tray icon: {error}"))?;
    Ok((tray_icon, visible_menu, hidden_menu))
}

fn make_icon_pixels(size: u32) -> Vec<u8> {
    let scale = 4;
    let sample_size = size as usize * scale;
    let mut samples = vec![[0_u8; 4]; sample_size * sample_size];
    let bars = [
        (0.32, 0.45, 0.39, 0.61),
        (0.43, 0.39, 0.51, 0.67),
        (0.55, 0.44, 0.63, 0.62),
        (0.67, 0.36, 0.75, 0.70),
    ];

    for y in 0..sample_size {
        for x in 0..sample_size {
            let px = (x as f32 + 0.5) / sample_size as f32;
            let py = (y as f32 + 0.5) / sample_size as f32;
            let color = if rounded_rect_contains(px, py, 0.03, 0.03, 0.97, 0.97, 0.22) {
                let mut color = [18, 38, 59, 255];
                if rounded_rect_contains(px, py, 0.18, 0.17, 0.82, 0.70, 0.08) {
                    color = [48, 190, 195, 255];
                }
                if rounded_rect_contains(px, py, 0.25, 0.24, 0.75, 0.63, 0.035) {
                    color = [24, 57, 78, 255];
                }
                if rounded_rect_contains(px, py, 0.455, 0.70, 0.545, 0.81, 0.025)
                    || rounded_rect_contains(px, py, 0.36, 0.79, 0.64, 0.86, 0.035)
                {
                    color = [48, 190, 195, 255];
                }
                for (index, (left, top, right, bottom)) in bars.iter().enumerate() {
                    if rounded_rect_contains(px, py, *left, *top, *right, *bottom, 0.025) {
                        color = if index == 3 {
                            [255, 185, 87, 255]
                        } else {
                            [235, 248, 249, 255]
                        };
                    }
                }
                color
            } else {
                [0, 0, 0, 0]
            };
            samples[y * sample_size + x] = color;
        }
    }

    let mut pixels = vec![0; size as usize * size as usize * 4];
    for y in 0..size as usize {
        for x in 0..size as usize {
            let mut total = [0_u32; 4];
            for sample_y in 0..scale {
                for sample_x in 0..scale {
                    let sample =
                        samples[(y * scale + sample_y) * sample_size + x * scale + sample_x];
                    for (channel, value) in sample.into_iter().enumerate() {
                        total[channel] += u32::from(value);
                    }
                }
            }
            let offset = (y * size as usize + x) * 4;
            for (channel, value) in total.into_iter().enumerate() {
                pixels[offset + channel] = (value / (scale * scale) as u32) as u8;
            }
        }
    }
    pixels
}

fn rounded_rect_contains(
    x: f32,
    y: f32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    radius: f32,
) -> bool {
    let inset_x = radius.min((right - left) * 0.499);
    let inset_y = radius.min((bottom - top) * 0.499);
    let nearest_x = x.clamp(left + inset_x, right - inset_x);
    let nearest_y = y.clamp(top + inset_y, bottom - inset_y);
    let dx = x - nearest_x;
    let dy = y - nearest_y;
    x >= left && x <= right && y >= top && y <= bottom && dx * dx + dy * dy <= radius * radius
}

fn config_path() -> PathBuf {
    let exe_directory = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    if let Some(directory) = &exe_directory {
        if let Some(path) = find_config_in_ancestors(directory) {
            return path;
        }
    }
    if let Ok(current_directory) = std::env::current_dir() {
        if let Some(path) = find_config_in_ancestors(&current_directory) {
            return path;
        }
    }
    exe_directory
        .map(|directory| directory.join("config.json"))
        .unwrap_or_else(|| PathBuf::from("config.json"))
}

fn find_config_in_ancestors(directory: &Path) -> Option<PathBuf> {
    directory
        .ancestors()
        .map(|ancestor| ancestor.join("config.json"))
        .find(|path| path.is_file())
}

fn show_startup_error(message: &str) {
    let message: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            w!("Monitor Audio Router"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn restore_main_window() -> bool {
    let Ok(window) = (unsafe { FindWindowW(None, w!("Monitor Audio Router")) }) else {
        return false;
    };
    unsafe {
        let _ = ShowWindow(window, SW_RESTORE);
        let _ = SetForegroundWindow(window);
    }
    true
}

fn main_window_visible() -> Option<bool> {
    let window = unsafe { FindWindowW(None, w!("Monitor Audio Router")) }.ok()?;
    Some(unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(window) }.as_bool())
}

fn hide_main_window() -> bool {
    let Ok(window) = (unsafe { FindWindowW(None, w!("Monitor Audio Router")) }) else {
        return false;
    };
    unsafe {
        let _ = ShowWindow(window, SW_HIDE);
    }
    true
}

fn main() {
    unsafe {
        if let Err(error) = RoInitialize(RO_INIT_SINGLETHREADED) {
            show_startup_error(&format!("Could not initialize Windows Runtime: {error}"));
            return;
        }
    }

    let app_config_path = config_path();
    let (tray_action_sender, tray_action_receiver) = mpsc::channel();
    let allow_exit = Arc::new(AtomicBool::new(false));
    let icon_size = 64;
    let result = eframe::run_native(
        "Monitor Audio Router",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_icon(Arc::new(egui::IconData {
                rgba: make_icon_pixels(icon_size),
                width: icon_size,
                height: icon_size,
            })),
            ..Default::default()
        },
        Box::new(move |creation_context| {
            let context = creation_context.egui_ctx.clone();
            let menu_sender = tray_action_sender.clone();
            let menu_allow_exit = Arc::clone(&allow_exit);
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                if event.id.as_ref() == TRAY_OPEN_ID {
                    if !restore_main_window() {
                        let _ = menu_sender.send(TrayAction::Restore);
                    }
                    context.request_repaint();
                } else if event.id.as_ref() == TRAY_MINIMIZE_ID {
                    if !hide_main_window() {
                        context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    }
                    context.request_repaint();
                } else if event.id.as_ref() == TRAY_EXIT_ID {
                    menu_allow_exit.store(true, Ordering::Release);
                    let posted = unsafe {
                        FindWindowW(None, w!("Monitor Audio Router"))
                            .and_then(|window| {
                                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                                    window,
                                    windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                                    None,
                                    None,
                                )
                                .map(|()| ())
                            })
                            .is_ok()
                    };
                    if !posted {
                        context.send_viewport_cmd(egui::ViewportCommand::Close);
                        let _ = menu_sender.send(TrayAction::Exit);
                    }
                    context.request_repaint();
                }
            }));

            let context = creation_context.egui_ctx.clone();
            let tray_sender = tray_action_sender.clone();
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                let action = match event {
                    TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    } => {
                        if !restore_main_window() {
                            Some(TrayAction::Restore)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(action) = action {
                    let _ = tray_sender.send(action);
                }
                context.request_repaint();
            }));

            let tray_result = create_tray_icon();
            Ok(Box::new(RouterApp::new(
                app_config_path,
                tray_result,
                tray_action_receiver,
                Arc::clone(&allow_exit),
            )))
        }),
    );

    unsafe { RoUninitialize() };
    if let Err(error) = result {
        show_startup_error(&format!("The application window could not start: {error}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_written_atomically_and_can_be_reopened() {
        let path = std::env::temp_dir().join(format!(
            "monitor-audio-router-config-{}.json",
            std::process::id()
        ));
        let serialized = serde_json::to_vec(&Config {
            monitor_rules: vec![MonitorRule {
                monitor: "monitor-hardware-id".to_string(),
                device: "audio-endpoint-id".to_string(),
            }],
            ..Config::default()
        })
        .unwrap();

        write_config_atomically(&path, &serialized).unwrap();
        let persisted = fs::read(&path).unwrap();
        let config: Config = serde_json::from_slice(&persisted).unwrap();

        assert_eq!(config.monitor_rules.len(), 1);
        assert_eq!(config.monitor_rules[0].monitor, "monitor-hardware-id");
        assert_eq!(config.monitor_rules[0].device, "audio-endpoint-id");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn app_and_tray_icon_pixels_have_valid_rgba_dimensions() {
        for size in [16, 32, 64] {
            let pixels = make_icon_pixels(size);
            assert_eq!(pixels.len(), size as usize * size as usize * 4);
            assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);
            assert!(pixels[(size as usize / 2 * size as usize + size as usize / 2) * 4 + 3] > 0);
        }
    }
}
