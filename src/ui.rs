//! The window (Models / Test / Settings), the floating overlay, and the tray icon.

use crate::dictation::{self, Cmd};
use crate::{Config, Row, audio, engine, output, update};
use eframe::egui::{self, Color32, CornerRadius, Event, Frame, RichText, Sense, Stroke, Vec2, pos2, vec2};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const BG: Color32 = Color32::from_rgb(13, 15, 20);
const SIDEBAR: Color32 = Color32::from_rgb(18, 21, 28);
const CARD: Color32 = Color32::from_rgb(24, 28, 37);
const BORDER: Color32 = Color32::from_rgb(38, 44, 58);
const ACCENT: Color32 = Color32::from_rgb(124, 92, 255);
const RED: Color32 = Color32::from_rgb(255, 77, 94);
const AMBER: Color32 = Color32::from_rgb(255, 184, 76);
const GREEN: Color32 = Color32::from_rgb(64, 200, 140);
const MUTED: Color32 = Color32::from_rgb(140, 148, 168);

/// `hidden`: start in the tray (launched at sign-in).
pub fn run(hidden: bool) -> eframe::Result {
    let (rgba, w, h) = dot_icon(ACCENT);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 600.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("voicetype")
            .with_icon(egui::IconData { rgba, width: w, height: h })
            .with_visible(!hidden),
        ..Default::default()
    };
    eframe::run_native("voicetype", options, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx, hidden)))))
}

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(10, 12, 16);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.hyperlink_color = ACCENT;
    for (w, fill) in [
        (&mut v.widgets.inactive, Color32::from_rgb(34, 39, 52)),
        (&mut v.widgets.hovered, Color32::from_rgb(46, 52, 70)),
        (&mut v.widgets.active, Color32::from_rgb(60, 66, 88)),
        (&mut v.widgets.open, Color32::from_rgb(46, 52, 70)),
    ] {
        w.weak_bg_fill = fill;
        w.bg_fill = fill;
        w.corner_radius = CornerRadius::same(8);
        w.bg_stroke = Stroke::NONE;
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.corner_radius = CornerRadius::same(8);
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = vec2(10.0, 10.0);
        s.spacing.button_padding = vec2(14.0, 7.0);
        s.spacing.interact_size.y = 30.0;
        use egui::{FontFamily::Proportional, FontId, TextStyle::*};
        s.text_styles = [
            (Heading, FontId::new(24.0, Proportional)),
            (Body, FontId::new(15.0, Proportional)),
            (Button, FontId::new(15.0, Proportional)),
            (Small, FontId::new(12.5, Proportional)),
            (Monospace, FontId::new(14.0, egui::FontFamily::Monospace)),
        ]
        .into();
    });
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(12)
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn section(ui: &mut egui::Ui, title: &str, hint: &str) {
    ui.label(RichText::new(title).size(17.0).strong().color(Color32::WHITE));
    if !hint.is_empty() {
        ui.label(RichText::new(hint).small().color(MUTED));
    }
}

fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    Frame::new().fill(color.gamma_multiply(0.18)).corner_radius(10).inner_margin(egui::Margin::symmetric(8, 2)).show(ui, |ui| {
        ui.label(RichText::new(text).small().color(color));
    });
}

fn primary(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(Color32::WHITE).strong()).fill(ACCENT)
}

/// A rounded level bar; RMS is shown on a sqrt scale so speech fills most of it.
fn meter(ui: &mut egui::Ui, value: f32, threshold: Option<f32>, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 10.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 5.0, Color32::from_rgb(10, 12, 16));
    let scale = |v: f32| v.max(0.0).sqrt().min(1.0);
    let fill = egui::Rect::from_min_size(rect.min, vec2(rect.width() * scale(value), rect.height()));
    let over = threshold.is_none_or(|t| value > t);
    p.rect_filled(fill, 5.0, if over { GREEN } else { MUTED });
    if let Some(t) = threshold {
        let x = rect.left() + rect.width() * scale(t);
        p.line_segment([pos2(x, rect.top() - 3.0), pos2(x, rect.bottom() + 3.0)], Stroke::new(2.0, AMBER));
    }
}

/// A plain anti-aliased disc in a ring: the window icon and the tray's state light.
fn dot_icon(c: Color32) -> (Vec<u8>, u32, u32) {
    let n = 32u32;
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let d = ((x as f32 - 15.5).powi(2) + (y as f32 - 15.5).powi(2)).sqrt();
            let ring = (1.0 - (d - 14.0).abs()).clamp(0.0, 1.0);
            let core = (9.5 - d).clamp(0.0, 1.0);
            let a = ring.max(core);
            px.extend_from_slice(&[c.r(), c.g(), c.b(), (a * 255.0) as u8]);
        }
    }
    (px, n, n)
}

fn tray_icon(c: Color32) -> Option<tray_icon::Icon> {
    let (rgba, w, h) = dot_icon(c);
    tray_icon::Icon::from_rgba(rgba, w, h).ok()
}

/// The label a dropdown shows for its current value.
fn label_of<T: PartialEq + Copy>(all: &[(T, &'static str)], value: T) -> &'static str {
    all.iter().find(|(v, _)| *v == value).map_or("", |(_, label)| label)
}

/// "ctrl+alt+Space" → "Ctrl + Alt + Space"
fn pretty_hotkey(s: &str) -> String {
    s.split('+').map(|p| {
        let mut c = p.chars();
        c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" + ")
}

/// A recording for the Test tab, started off the UI thread (winit's COM apartment and WASAPI don't mix).
#[derive(Default)]
struct Recorder {
    want: bool,
    cap: Option<audio::Capture>,
}
type Rec = Arc<Mutex<Recorder>>;

fn rec_start(rec: &Rec, device: Option<String>, tx: Sender<Msg>) {
    rec.lock().unwrap().want = true;
    let rec = rec.clone();
    std::thread::spawn(move || match audio::start_capture(device.as_deref()) {
        Ok(cap) => {
            let mut r = rec.lock().unwrap();
            if r.want {
                r.cap = Some(cap);
            }
        }
        Err(e) => {
            rec.lock().unwrap().want = false;
            let _ = tx.send(Msg::Status(format!("Mic error: {e}")));
        }
    });
}

fn rec_stop(rec: &Rec) -> Option<Vec<f32>> {
    let mut r = rec.lock().unwrap();
    r.want = false;
    r.cap.take().map(audio::Capture::stop)
}

enum Msg {
    Status(String),
    Result(Row),
    TestDone,
    Downloaded(Result<(), String>),
    Show,
    UpdateChecked(Result<Option<update::Release>, String>),
    UpdateDownloaded(Result<(std::path::PathBuf, String), String>),
    Quit,
}

enum Upd {
    Off,
    Checking,
    UpToDate,
    Available(update::Release),
    Downloading(String),
    /// Downloaded and verified; installed as soon as nothing is being dictated.
    Ready(std::path::PathBuf, String),
    Failed(String),
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Models,
    Test,
    Shortcut,
    HandsFree,
    Microphone,
    Output,
    General,
}

/// Sidebar groups: (heading, pages).
const NAV: &[(&str, &[(Tab, &str)])] = &[
    ("SETUP", &[(Tab::Models, "Models"), (Tab::Test, "Test bench")]),
    ("DICTATION", &[(Tab::Shortcut, "Shortcut"), (Tab::HandsFree, "Hands-free"), (Tab::Microphone, "Microphone"), (Tab::Output, "Output")]),
    ("APP", &[(Tab::General, "General")]),
];

struct App {
    tab: Tab,
    cfg: Arc<Mutex<Config>>,
    edit: Config,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    status: String,
    hotkeys: GlobalHotKeyManager,
    hotkey: Option<HotKey>,
    hotkey_error: String,
    capturing_hotkey: bool,
    state: Arc<dictation::State>,
    test_rec: Rec,
    clip: Option<Vec<f32>>,
    wav_path: String,
    test_model: String,
    busy: bool,
    results: Vec<Row>,
    devices: Vec<String>,
    gpu_name: Option<String>,
    downloading: Option<&'static str>,
    tray: Option<TrayIcon>,
    tray_color: Color32,
    hidden: bool,
    quitting: bool,
    hide_frames: u8,
    upd: Upd,
    overlay_open: bool,
}

impl App {
    fn new(ctx: &egui::Context, hidden: bool) -> Self {
        style(ctx);
        let cfg = crate::load_config();
        let (tx, rx) = channel();
        let shared = Arc::new(Mutex::new(cfg.clone()));
        let state = Arc::new(dictation::State::default());
        let repaint = ctx.clone();
        let cmds = dictation::spawn(shared.clone(), state.clone(), Arc::new(move || repaint.request_repaint()));
        {
            // Runs on the main thread inside the message pump, even while the window is hidden.
            let (shared, ctx) = (shared.clone(), ctx.clone());
            let cmds = Mutex::new(cmds);
            GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
                let hold = shared.lock().unwrap().mode == "hold";
                let cmd = match (hold, e.state) {
                    (true, HotKeyState::Pressed) => Cmd::Hold,
                    (true, HotKeyState::Released) => Cmd::Release,
                    (false, HotKeyState::Pressed) => Cmd::Toggle,
                    (false, HotKeyState::Released) => return,
                };
                let _ = cmds.lock().unwrap().send(cmd);
                ctx.request_repaint();
            }));
        }

        let show = MenuItem::new("Show voicetype", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        let _ = menu.append(&show);
        let _ = menu.append(&quit);
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("voicetype")
            .with_icon(tray_icon(ACCENT).expect("icon"))
            .build()
            .ok();
        {
            let (tx2, ctx2) = (tx.clone(), ctx.clone());
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    let _ = tx2.send(Msg::Show);
                    ctx2.request_repaint();
                }
            }));
            let (tx, ctx, show, quit) = (tx.clone(), ctx.clone(), show.id().clone(), quit.id().clone());
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let _ = tx.send(if e.id == quit { Msg::Quit } else if e.id == show { Msg::Show } else { return });
                ctx.request_repaint();
            }));
        }

        let mut app = App {
            tab: if engine::MODELS.iter().any(|m| engine::installed(m.id)) { Tab::Shortcut } else { Tab::Models },
            test_model: cfg.model.clone(),
            edit: cfg,
            cfg: shared,
            tx,
            rx,
            status: "Ready".into(),
            hotkeys: GlobalHotKeyManager::new().expect("hotkey manager"),
            hotkey: None,
            hotkey_error: String::new(),
            capturing_hotkey: false,
            state,
            test_rec: Rec::default(),
            clip: None,
            wav_path: String::new(),
            busy: false,
            results: Vec::new(),
            devices: audio::list_input_devices(),
            gpu_name: engine::nvidia_gpu(),
            downloading: None,
            tray,
            tray_color: ACCENT,
            hidden,
            quitting: false,
            hide_frames: if hidden { 3 } else { 0 },
            upd: Upd::Off,
            overlay_open: false,
        };
        app.apply_hotkey();
        if update::enabled() {
            app.check_updates(ctx);
        }
        if app.edit.run_at_startup {
            crate::set_autostart(true);
        }
        if app.hidden && app.tray.is_none() {
            // No tray to bring it back from: show the window after all.
            app.hidden = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        app
    }

    fn unregister_hotkey(&mut self) {
        if let Some(h) = self.hotkey.take() {
            let _ = self.hotkeys.unregister(h);
        }
    }

    fn apply_hotkey(&mut self) {
        self.unregister_hotkey();
        self.hotkey_error = match HotKey::from_str(&self.edit.hotkey) {
            Ok(h) => match self.hotkeys.register(h) {
                Ok(()) => {
                    self.hotkey = Some(h);
                    String::new()
                }
                Err(e) => format!("Couldn't register (another app may own it): {e}"),
            },
            Err(e) => format!("Couldn't parse: {e}"),
        };
    }

    fn spawn(&self, f: impl FnOnce(&Sender<Msg>) + Send + 'static, ctx: &egui::Context) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            f(&tx);
            ctx.request_repaint();
        });
    }

    fn header(ui: &mut egui::Ui, title: &str, sub: &str) {
        ui.add_space(4.0);
        ui.heading(RichText::new(title).strong().color(Color32::WHITE));
        ui.label(RichText::new(sub).color(MUTED));
        ui.add_space(6.0);
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
            ui.painter().circle_stroke(r.center(), 10.0, Stroke::new(2.0, ACCENT));
            ui.painter().circle_filled(r.center(), 5.5, ACCENT);
            ui.label(RichText::new("voicetype").size(20.0).strong().color(Color32::WHITE));
        });
        ui.add_space(8.0);
        for (group, pages) in NAV {
            ui.add_space(8.0);
            ui.label(RichText::new(*group).size(11.0).strong().color(MUTED.gamma_multiply(0.7)));
            for &(tab, name) in *pages {
                let selected = self.tab == tab;
                let text = RichText::new(format!("   {name}")).size(15.0).color(if selected { Color32::WHITE } else { MUTED });
                let b = egui::Button::new(text)
                    .fill(if selected { ACCENT.gamma_multiply(0.25) } else { Color32::TRANSPARENT })
                    .min_size(vec2(ui.available_width(), 34.0));
                if ui.add(b).clicked() {
                    self.tab = tab;
                }
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(14.0);
            ui.label(RichText::new(format!("{} {}", if self.edit.mode == "hold" { "Hold" } else { "Press" }, pretty_hotkey(&self.edit.hotkey))).small().color(MUTED));
            match &self.upd {
                Upd::Available(r) => drop(ui.label(RichText::new(format!("Update {} available (General)", r.version)).small().color(ACCENT))),
                Upd::Downloading(v) | Upd::Ready(_, v) => drop(ui.label(RichText::new(format!("Updating to {v}…")).small().color(ACCENT))),
                _ => {}
            }
            if self.edit.wake_word {
                ui.label(RichText::new(format!("or say “{}”", self.edit.wake_phrase)).small().color(MUTED));
            }
            let (color, text) = self.live_status();
            Frame::new().fill(CARD).corner_radius(10).inner_margin(10).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 5.0, color);
                    ui.add(egui::Label::new(RichText::new(text).small()).truncate());
                });
            });
        });
    }

    fn live_status(&self) -> (Color32, String) {
        if self.state.recording.load(Ordering::Relaxed) {
            (RED, "Listening…".into())
        } else if self.state.pending.load(Ordering::Relaxed) > 0 {
            (AMBER, "Transcribing…".into())
        } else {
            let s = self.state.status.lock().unwrap().clone();
            let s = if s.is_empty() { self.status.clone() } else { s };
            (if s.starts_with("Error") || s.starts_with("Mic error") { RED } else { GREEN }, s)
        }
    }

    fn models_tab(&mut self, ui: &mut egui::Ui) {
        Self::header(ui, "Models", "Download a speech model. Bigger is more accurate and slower; a GPU makes even the largest instant.");
        if engine::MODELS.iter().any(|m| engine::installed(m.id)) {
            self.engine_card(ui);
        }
        let downloading = engine::DOWNLOADING.load(Ordering::Relaxed);
        if let Some(name) = self.gpu_name.clone() {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    section(ui, &name, "");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if engine::gpu_engine_installed() {
                            if ui.button("Remove").clicked() {
                                self.status = engine::remove_gpu_engine().err().unwrap_or_else(|| "GPU engine removed".into());
                            }
                            badge(ui, "Installed", GREEN);
                        } else if ui.add_enabled(!downloading, primary("Download GPU engine")).clicked() {
                            self.downloading = Some("gpu");
                            self.spawn(|tx| drop(tx.send(Msg::Downloaded(engine::install_gpu_engine()))), ui.ctx());
                        }
                    });
                });
                // Below the buttons, not beside them, so the two never overlap in a narrow window.
                ui.label(RichText::new("The GPU engine is a one-off 670 MB download and makes transcription 10–100× faster.").small().color(MUTED));
                if self.downloading == Some("gpu") {
                    self.progress(ui);
                }
            });
        }
        for m in engine::MODELS {
            let installed = engine::installed(m.id);
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(m.label).size(16.0).strong().color(Color32::WHITE));
                            badge(ui, &format!("{} MB", m.size / 1_000_000), MUTED);
                            badge(ui, if m.multilingual { "Multilingual" } else { "English" }, ACCENT);
                            if self.edit.model == m.id && installed {
                                badge(ui, "Dictation model", GREEN);
                            }
                        });
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if installed {
                            if ui.button("Delete").clicked() {
                                self.status = engine::remove_model(m.id).err().unwrap_or_else(|| format!("Deleted {}", m.label));
                            }
                            if self.edit.model != m.id && ui.button("Use for dictation").clicked() {
                                self.edit.model = m.id.into();
                            }
                        } else if ui.add_enabled(!downloading, primary("Download")).clicked() {
                            let id = m.id;
                            self.downloading = Some(id);
                            self.spawn(move |tx| drop(tx.send(Msg::Downloaded(engine::install_model(id)))), ui.ctx());
                        }
                    });
                });
                if self.downloading == Some(m.id) {
                    self.progress(ui);
                }
            });
        }
    }

    fn progress(&self, ui: &mut egui::Ui) {
        let (got, total) = (engine::RECEIVED.load(Ordering::Relaxed), engine::TOTAL.load(Ordering::Relaxed).max(1));
        ui.horizontal(|ui| {
            ui.add(
                egui::ProgressBar::new(got as f32 / total as f32)
                    .desired_width(ui.available_width() - 90.0)
                    .fill(ACCENT)
                    .corner_radius(6)
                    .text(format!("{} / {} MB", got / 1_000_000, total / 1_000_000)),
            );
            if ui.button("Cancel").clicked() {
                engine::CANCEL.store(true, Ordering::Relaxed);
            }
        });
        ui.ctx().request_repaint_after(Duration::from_millis(200));
    }

    fn test_tab(&mut self, ui: &mut egui::Ui) {
        Self::header(ui, "Test bench", "Record once, run it through every model, compare speed and accuracy.");
        let installed: Vec<&str> = engine::MODELS.iter().map(|m| m.id).filter(|id| engine::installed(id)).collect();
        if installed.is_empty() {
            card(ui, |ui| ui.label("Download a model first."));
            return;
        }
        if !installed.contains(&self.test_model.as_str()) {
            self.test_model = installed[0].into();
        }
        card(ui, |ui| {
            section(ui, "1. Clip", "Record from your mic, or load / drop a WAV file.");
            ui.horizontal(|ui| {
                let recording = self.test_rec.lock().unwrap().want;
                if recording {
                    if ui.add(egui::Button::new(RichText::new("Stop").color(Color32::WHITE).strong()).fill(RED)).clicked() {
                        self.clip = rec_stop(&self.test_rec);
                    }
                    let level = self.test_rec.lock().unwrap().cap.as_ref().map_or(0.0, |c| c.level());
                    meter(ui, level, None, 160.0);
                    ui.ctx().request_repaint_after(Duration::from_millis(50));
                } else if ui.add(primary("Record")).clicked() {
                    rec_start(&self.test_rec, self.edit.device.clone(), self.tx.clone());
                }
                ui.separator();
                ui.add(egui::TextEdit::singleline(&mut self.wav_path).hint_text("C:\\path\\to\\clip.wav").desired_width((ui.available_width() - 160.0).max(200.0)));
                if ui.button("Load").clicked() {
                    self.load_wav();
                }
                if let Some(clip) = &self.clip {
                    badge(ui, &format!("{:.1} s clip", clip.len() as f32 / audio::RATE as f32), GREEN);
                }
            });
        });
        card(ui, |ui| {
            section(ui, "2. Run", "");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("test_model").selected_text(&self.test_model).show_ui(ui, |ui| {
                    for id in &installed {
                        ui.selectable_value(&mut self.test_model, id.to_string(), *id);
                    }
                });
                let ready = self.clip.is_some() && !self.busy;
                let models: Option<Vec<String>> = if ui.add_enabled(ready, egui::Button::new("Transcribe")).clicked() {
                    Some(vec![self.test_model.clone()])
                } else if ui.add_enabled(ready, primary("Run on all installed models")).clicked() {
                    Some(installed.iter().map(|s| s.to_string()).collect())
                } else {
                    None
                };
                if let (Some(models), Some(clip)) = (models, self.clip.clone()) {
                    self.busy = true;
                    let (gpu, lang) = (self.edit.gpu, self.edit.language.clone());
                    self.spawn(
                        move |tx| {
                            for m in models {
                                let _ = tx.send(Msg::Result(crate::run(&m, gpu, &clip, &lang)));
                            }
                            let _ = tx.send(Msg::TestDone);
                        },
                        ui.ctx(),
                    );
                }
                if self.busy {
                    ui.spinner();
                }
                if !self.results.is_empty() && ui.button("Clear").clicked() {
                    self.results.clear();
                }
            });
        });
        if self.results.is_empty() {
            return;
        }
        section(ui, "Results", "Server start is a one-off per model. RTF is transcribe time / clip length; lower is faster.");
        for r in self.results.iter().rev() {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&r.model).size(16.0).strong().color(Color32::WHITE));
                    badge(ui, if r.gpu { "GPU" } else { "CPU" }, if r.gpu { GREEN } else { AMBER });
                    ui.label(RichText::new(format!("start {:.2} s   ·   transcribe {:.2} s   ·   RTF {:.3}", r.start_s, r.secs, r.secs / r.audio_s.max(0.01))).color(MUTED));
                });
                match &r.text {
                    Ok(t) => ui.add(egui::Label::new(RichText::new(t).size(15.5)).wrap()),
                    Err(e) => ui.add(egui::Label::new(RichText::new(e).color(RED)).wrap()),
                };
            });
        }
    }

    fn load_wav(&mut self) {
        match audio::load_wav(std::path::Path::new(self.wav_path.trim().trim_matches('"'))) {
            Ok(s) => self.clip = Some(s),
            Err(e) => self.status = format!("Couldn't load WAV: {e}"),
        }
    }

    fn hotkey_capture(&mut self, ui: &mut egui::Ui) {
        if self.capturing_hotkey {
            let pressed = ui.input(|i| {
                i.events.iter().find_map(|e| match e {
                    Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                    _ => None,
                })
            });
            if let Some((key, m)) = pressed {
                if key != egui::Key::Escape {
                    let mut parts: Vec<&str> = Vec::new();
                    if m.ctrl {
                        parts.push("ctrl");
                    }
                    if m.alt {
                        parts.push("alt");
                    }
                    if m.shift {
                        parts.push("shift");
                    }
                    let candidate = format!("{}{}{}", parts.join("+"), if parts.is_empty() { "" } else { "+" }, key.name());
                    match HotKey::from_str(&candidate) {
                        Ok(_) => self.edit.hotkey = candidate,
                        Err(_) => self.hotkey_error = format!("{} can't be a hotkey", key.name()),
                    }
                }
                self.capturing_hotkey = false;
                // Saving re-registers when it changed; re-register here when it didn't.
                if self.edit.hotkey == self.cfg.lock().unwrap().hotkey {
                    self.apply_hotkey();
                }
            }
        }
        let (text, fill) = if self.capturing_hotkey {
            ("Press your shortcut…  (Esc cancels)".to_string(), ACCENT.gamma_multiply(0.35))
        } else {
            (pretty_hotkey(&self.edit.hotkey), Color32::from_rgb(34, 39, 52))
        };
        let b = ui.add(egui::Button::new(RichText::new(text).strong()).fill(fill).min_size(vec2(260.0, 36.0)));
        if b.clicked() && !self.capturing_hotkey {
            // The live hotkey would swallow the keypress before the window sees it.
            self.unregister_hotkey();
            self.capturing_hotkey = true;
            self.hotkey_error.clear();
        }
        if !self.hotkey_error.is_empty() {
            ui.colored_label(RED, &self.hotkey_error);
        }
    }

    fn engine_card(&mut self, ui: &mut egui::Ui) {
        let e = &mut self.edit;
        card(ui, |ui| {
            section(ui, "Dictation model", "The model used for dictation and the wake word. Download more below.");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("model").width(260.0).selected_text(engine::model(&e.model).map_or(e.model.as_str(), |m| m.label)).show_ui(ui, |ui| {
                    for m in engine::MODELS.iter().filter(|m| engine::installed(m.id)) {
                        ui.selectable_value(&mut e.model, m.id.to_string(), m.label);
                    }
                });
                ui.label("Language");
                ui.add(egui::TextEdit::singleline(&mut e.language).desired_width(60.0).hint_text("auto"));
                ui.checkbox(&mut e.gpu, "Use the GPU when available");
            });
        });
    }

    fn shortcut_tab(&mut self, ui: &mut egui::Ui) {
        Self::header(ui, "Shortcut", "The key combination that starts dictation from any app.");
        card(ui, |ui| {
            section(ui, "Keys", "Click the box, then press the keys you want. Esc cancels.");
            self.hotkey_capture(ui);
        });
        let e = &mut self.edit;
        card(ui, |ui| {
            section(ui, "How it works", "");
            ui.radio_value(&mut e.mode, "hold".into(), "Hold to talk: record while the keys are held, type when you let go");
            ui.radio_value(&mut e.mode, "toggle".into(), "Press to start, press again to stop (or let it stop on silence, see Hands-free)");
        });
    }

    fn hands_free_tab(&mut self, ui: &mut egui::Ui) {
        Self::header(ui, "Hands-free", "Each of these is optional and works on its own.");
        let e = &mut self.edit;
        card(ui, |ui| {
            section(ui, "Stop on silence", "Press-to-start recordings end by themselves. Hold-to-talk always ends when you let go; wake-word recordings have their own setting below.");
            ui.checkbox(&mut e.auto_stop, "Stop recording automatically");
            ui.add_enabled(e.auto_stop, egui::Slider::new(&mut e.auto_stop_secs, 0.5..=6.0).text("seconds of silence").step_by(0.1));
        });
        card(ui, |ui| {
            section(ui, "Real-time typing", "Type each phrase as soon as you pause instead of waiting for the end. Short pieces transcribe faster and are easier on the GPU.");
            ui.checkbox(&mut e.split_on_pause, "Type each phrase when I pause");
            ui.add_enabled(e.split_on_pause, egui::Slider::new(&mut e.pause_secs, 0.3..=2.0).text("second pause").step_by(0.1));
        });
        card(ui, |ui| {
            section(ui, "Live preview", "Show the words heard so far in the floating indicator while you talk.");
            ui.checkbox(&mut e.live_preview, "Show live preview");
            ui.add_enabled(e.live_preview, egui::Slider::new(&mut e.preview_secs, 0.2..=5.0).text("seconds between updates").step_by(0.1));
            ui.label(RichText::new("Each update transcribes the current phrase again, so shorter gaps work the CPU or GPU harder. If the model can't keep up, the preview updates as fast as it can.").small().color(MUTED));
            if !e.overlay {
                ui.label(RichText::new("The preview appears in the floating indicator, which is off (see General).").small().color(AMBER));
            }
        });
        card(ui, |ui| {
            section(ui, "Wake word", "Say the phrase to start recording without touching the keyboard. The mic stays open and each burst of speech is checked with the dictation model. The phrase itself isn't typed.");
            ui.checkbox(&mut e.wake_word, "Listen for a wake phrase");
            ui.horizontal(|ui| {
                ui.label("Phrase");
                ui.add_enabled(e.wake_word, egui::TextEdit::singleline(&mut e.wake_phrase).desired_width(220.0));
            });
            ui.add_enabled_ui(e.wake_word, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Trigger on");
                    let selected = label_of(&dictation::WakeMatch::ALL, e.wake_match);
                    egui::ComboBox::from_id_salt("wake_match").width(ui.available_width().min(400.0)).selected_text(selected).show_ui(ui, |ui| {
                        for (mode, label) in dictation::WakeMatch::ALL {
                            ui.selectable_value(&mut e.wake_match, mode, label);
                        }
                    });
                });
                ui.label(RichText::new("If it starts when nobody asked it to, go stricter. If it misses you, go more forgiving.").small().color(MUTED));
                ui.add(egui::Slider::new(&mut e.wake_stop_secs, 0.5..=10.0).text("seconds of silence end the recording").step_by(0.1));
                ui.checkbox(&mut e.wake_chime, "Play a sound when the phrase is heard");
            });
        });
    }

    fn microphone_tab(&mut self, ui: &mut egui::Ui) {
        Self::header(ui, "Microphone", "Which mic to use, and how loud counts as speech.");
        let e = &mut self.edit;
        card(ui, |ui| {
            section(ui, "Input device", "");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("device")
                    .width((ui.available_width() - 100.0).max(200.0))
                    .selected_text(e.device.as_deref().unwrap_or("System default"))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut e.device, None, "System default");
                        for d in &self.devices {
                            ui.selectable_value(&mut e.device, Some(d.clone()), d);
                        }
                    });
                if ui.button("Refresh").clicked() {
                    self.devices = audio::list_input_devices();
                }
            });
        });
        card(ui, |ui| {
            section(ui, "Silence threshold", "Speech has to cross the amber line. Raise it if room noise keeps recordings going; lower it if quiet speech gets cut off.");
            ui.add(egui::Slider::new(&mut e.silence_threshold, 0.001..=0.1).logarithmic(true));
            let mut monitor = self.state.monitor.load(Ordering::Relaxed);
            ui.checkbox(&mut monitor, "Show live mic level");
            if monitor || self.state.busy() || e.wake_word {
                let (rms, th) = self.state.vad();
                meter(ui, rms, Some(th), ui.available_width());
                ui.ctx().request_repaint_after(Duration::from_millis(50));
            }
            self.state.monitor.store(monitor, Ordering::Relaxed);
        });
    }

    fn output_tab(&mut self, ui: &mut egui::Ui) {
        Self::header(ui, "Output", "How the text gets into the app you're typing in.");
        let e = &mut self.edit;
        card(ui, |ui| {
            section(ui, "Method", "");
            ui.radio_value(&mut e.output, output::Mode::Paste, "Paste: fastest and handles any character; your clipboard text is put back afterwards");
            ui.radio_value(&mut e.output, output::Mode::Keys, "Type keystrokes: for places that block paste (some terminals, remote desktops, games)");
        });
        card(ui, |ui| {
            section(ui, "After each phrase", "Typed between phrases, and after the last one so the next dictation doesn't run into it.");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("after_phrase").width(160.0).selected_text(label_of(&output::After::ALL, e.after_phrase)).show_ui(ui, |ui| {
                    for (after, label) in output::After::ALL {
                        ui.selectable_value(&mut e.after_phrase, after, label);
                    }
                });
                if e.after_phrase == output::After::Custom {
                    ui.add(egui::TextEdit::singleline(&mut e.custom_after).desired_width(160.0).hint_text("text to add"));
                }
            });
            if e.after_phrase == output::After::NewLine && e.output == output::Mode::Keys {
                ui.label(RichText::new("With keystrokes a new line is typed as Enter, which sends the message in most chat apps. Paste keeps it as a line break.").small().color(AMBER));
            }
        });
        card(ui, |ui| {
            section(ui, "When a recording ends", "Happens once per recording, after all its text is in, and only if something was typed. With real-time typing, phrases in between don't trigger it.");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("end_key").width(200.0).selected_text(label_of(&output::EndKey::ALL, e.end_key)).show_ui(ui, |ui| {
                    for (key, label) in output::EndKey::ALL {
                        ui.selectable_value(&mut e.end_key, key, label);
                    }
                });
            });
            ui.add_enabled(e.end_key != output::EndKey::None, egui::Slider::new(&mut e.end_key_delay_secs, 0.0..=3.0).text("seconds to wait first").step_by(0.1));
            ui.label(RichText::new("Enter sends the message in most chat apps; the wait gives slow apps time to take the text in. A key replaces the text above at the end, so no trailing space goes with it.").small().color(MUTED));
        });
        card(ui, |ui| {
            section(ui, "Text", "");
            ui.checkbox(&mut e.drop_period, "Remove the period at the end of each phrase");
        });
    }

    fn check_updates(&mut self, ctx: &egui::Context) {
        self.upd = Upd::Checking;
        self.spawn(|tx| drop(tx.send(Msg::UpdateChecked(update::check()))), ctx);
    }

    fn download_update(&mut self, r: update::Release, ctx: &egui::Context) {
        self.upd = Upd::Downloading(r.version.clone());
        self.spawn(move |tx| drop(tx.send(Msg::UpdateDownloaded(update::download(&r).map(|exe| (exe, r.version))))), ctx);
    }

    /// Swap in the downloaded exe and restart into it, keeping the window hidden if it was.
    fn install_update(&mut self, exe: std::path::PathBuf) {
        engine::shutdown();
        let args: &[&str] = if self.hidden { &["--hidden"] } else { &[] };
        match update::apply(&exe, args) {
            Ok(()) => {
                self.tray = None; // removes the icon now instead of leaving a ghost until hovered
                std::process::exit(0);
            }
            Err(e) => self.upd = Upd::Failed(e),
        }
    }

    fn update_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            section(ui, "Updates", &format!("You have voicetype {}.", update::current()));
            ui.checkbox(&mut self.edit.auto_update, "Download and install updates automatically");
            ui.label(RichText::new("Checked each time voicetype starts; an update installs when you're not dictating, then voicetype restarts.").small().color(MUTED));
            ui.horizontal(|ui| {
                let text = match &self.upd {
                    Upd::Off => "Updates are off in development builds".to_string(),
                    Upd::Checking => "Checking…".into(),
                    Upd::UpToDate => "Up to date".into(),
                    Upd::Available(r) => format!("Version {} is available", r.version),
                    Upd::Downloading(v) => format!("Downloading {v}…"),
                    Upd::Ready(_, v) => format!("Installing {v}…"),
                    Upd::Failed(e) => e.clone(),
                };
                let enabled = update::enabled() && matches!(self.upd, Upd::UpToDate | Upd::Failed(_) | Upd::Available(_));
                if let Upd::Available(r) = &self.upd {
                    if ui.add(primary("Update now")).clicked() {
                        let r = r.clone();
                        self.download_update(r, ui.ctx());
                    }
                } else if ui.add_enabled(enabled, egui::Button::new("Check for updates")).clicked() {
                    self.check_updates(ui.ctx());
                }
                let color = if matches!(self.upd, Upd::Failed(_)) { RED } else { MUTED };
                ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
            });
        });
    }

    fn general_tab(&mut self, ui: &mut egui::Ui) {
        Self::header(ui, "General", "");
        let e = &mut self.edit;
        if cfg!(windows) {
            card(ui, |ui| {
                section(ui, "Startup", "Starts quietly in the tray when you sign in to Windows.");
                ui.checkbox(&mut e.run_at_startup, "Run voicetype at startup");
            });
        }
        card(ui, |ui| {
            section(ui, "Window", "");
            ui.checkbox(&mut e.close_to_tray, "Closing the window keeps voicetype running in the tray");
            ui.checkbox(&mut e.overlay, "Show a floating indicator while dictating");
        });
        self.update_card(ui);
    }

    /// A pill at the bottom of the screen: never focused, clicks pass through it.
    /// Deferred, so eframe creates and paints it itself, even while the main window is hidden.
    fn overlay(&self, ctx: &egui::Context) {
        let size = vec2(460.0, 96.0);
        let monitor = ctx.input(|i| i.viewport().monitor_size).unwrap_or(vec2(1920.0, 1080.0));
        let builder = egui::ViewportBuilder::default()
            .with_title("voicetype indicator")
            .with_decorations(false)
            .with_transparent(true)
            .with_window_level(egui::WindowLevel::AlwaysOnTop)
            .with_taskbar(false)
            .with_active(false)
            .with_mouse_passthrough(true)
            .with_resizable(false)
            .with_inner_size(size)
            .with_position(pos2((monitor.x - size.x) / 2.0, monitor.y - size.y - 64.0));
        let state = self.state.clone();
        ctx.show_viewport_deferred(egui::ViewportId::from_hash_of("overlay"), builder, move |ui, _| {
            let pill = paint_overlay(ui, &state);
            #[cfg(windows)]
            clip_to_pill("voicetype indicator", pill, ui.ctx().pixels_per_point());
            #[cfg(not(windows))]
            let _ = pill;
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        });
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0; 4] // the overlay window is transparent; the main window paints its own panels
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Status(s) => self.status = s,
                Msg::Result(r) => self.results.push(r),
                Msg::TestDone => {
                    self.busy = false;
                    self.status = "Test finished".into();
                }
                Msg::Downloaded(r) => {
                    self.downloading = None;
                    self.status = r.err().unwrap_or_else(|| "Download finished".into());
                }
                Msg::Show => {
                    self.hidden = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Msg::UpdateChecked(Ok(Some(r))) if self.edit.auto_update => self.download_update(r, ctx),
                Msg::UpdateChecked(Ok(Some(r))) => self.upd = Upd::Available(r),
                Msg::UpdateChecked(Ok(None)) => self.upd = Upd::UpToDate,
                Msg::UpdateChecked(Err(e)) | Msg::UpdateDownloaded(Err(e)) => self.upd = Upd::Failed(e),
                Msg::UpdateDownloaded(Ok((exe, v))) => self.upd = Upd::Ready(exe, v),
                Msg::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        let busy = self.state.busy();
        if let Upd::Ready(exe, _) = &self.upd {
            // Never restart in the middle of a dictation or a model download.
            if !busy && !engine::DOWNLOADING.load(Ordering::Relaxed) {
                let exe = exe.clone();
                self.install_update(exe);
            } else {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
        }
        let color = if self.state.recording.load(Ordering::Relaxed) { RED } else if busy { AMBER } else { ACCENT };
        if color != self.tray_color {
            self.tray_color = color;
            if let Some(t) = &self.tray {
                let _ = t.set_icon(tray_icon(color));
                let _ = t.set_tooltip(Some(format!("voicetype: {}", self.live_status().1)));
            }
        }
        // eframe shows the window after its first frame no matter what; undo that for a --hidden launch.
        if self.hide_frames > 0 {
            self.hide_frames -= 1;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.request_repaint();
        }
        // While hidden in the tray eframe skips `ui`, which is what draws the overlay.
        let want_overlay = busy && self.edit.overlay;
        if self.hidden && (want_overlay || self.overlay_open) {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && self.edit.close_to_tray && self.tray.is_some() && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
        }
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty())) {
            self.wav_path = path.display().to_string();
            self.load_wav();
            self.tab = Tab::Test;
        }
        self.overlay_open = self.state.busy() && self.edit.overlay;
        if self.overlay_open {
            self.overlay(&ctx);
        }
        if self.hidden {
            return;
        }

        egui::Panel::left("nav")
            .exact_size(210.0)
            .resizable(false)
            .frame(Frame::new().fill(SIDEBAR).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().frame(Frame::new().fill(BG).inner_margin(24)).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.tab {
                Tab::Models => self.models_tab(ui),
                Tab::Test => self.test_tab(ui),
                Tab::Shortcut => self.shortcut_tab(ui),
                Tab::HandsFree => self.hands_free_tab(ui),
                Tab::Microphone => self.microphone_tab(ui),
                Tab::Output => self.output_tab(ui),
                Tab::General => self.general_tab(ui),
            });
        });
        if self.state.busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        if self.edit != *self.cfg.lock().unwrap() {
            let old_startup = self.cfg.lock().unwrap().run_at_startup;
            let hotkey_changed = self.edit.hotkey != self.cfg.lock().unwrap().hotkey;
            *self.cfg.lock().unwrap() = self.edit.clone();
            crate::save_config(&self.edit);
            if self.edit.run_at_startup != old_startup {
                crate::set_autostart(self.edit.run_at_startup);
            }
            if hotkey_changed {
                self.apply_hotkey();
            }
        }
    }
}

/// Windows 11 draws a border, a shadow and a 1 px top line around every
/// top-level window, borderless and transparent ones included, which shows as a
/// square around the pill. Clip the window to the pill's own rounded shape so
/// nothing outside it is drawn, and turn the DWM border and shadow off too.
#[cfg(windows)]
fn clip_to_pill(title: &str, pill: egui::Rect, ppp: f32) {
    use std::sync::Mutex;
    use windows_sys::Win32::Graphics::Dwm::*;
    use windows_sys::Win32::Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn};
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;
    static LAST: Mutex<(isize, [i32; 4])> = Mutex::new((0, [0; 4]));
    let wide: Vec<u16> = title.encode_utf16().chain([0]).collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) };
    if hwnd.is_null() {
        return;
    }
    // A pixel of slack keeps the pill's anti-aliased edge inside the region's hard edge.
    let r = pill.expand(1.0);
    let px = [r.left(), r.top(), r.right(), r.bottom()].map(|v| (v * ppp).round() as i32);
    let mut last = LAST.lock().unwrap();
    if *last == (hwnd as isize, px) {
        return;
    }
    if last.0 != hwnd as isize {
        let set = |attr: i32, value: u32| unsafe { DwmSetWindowAttribute(hwnd, attr as u32, &value as *const u32 as _, 4) };
        set(DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE);
        set(DWMWA_NCRENDERING_POLICY, DWMNCRP_DISABLED as u32);
    }
    *last = (hwnd as isize, px);
    let d = ((OVERLAY_RADIUS + 1.0) * 2.0 * ppp).round() as i32;
    unsafe {
        // The window owns the region after this call; it frees the old one.
        SetWindowRgn(hwnd, CreateRoundRectRgn(px[0], px[1], px[2] + 1, px[3] + 1, d, d), 1);
    }
}

const OVERLAY_RADIUS: f32 = 24.0;

/// Paints the pill at the bottom of the window and returns where it is.
fn paint_overlay(ui: &mut egui::Ui, state: &dictation::State) -> egui::Rect {
    let recording = state.recording.load(Ordering::Relaxed);
    let level = state.level();
    let preview = state.preview.lock().unwrap().clone();
    let full = ui.max_rect();
    let size = if preview.is_empty() { vec2(240.0, 52.0) } else { vec2(full.width() - 8.0, 80.0) };
    let rect = egui::Rect::from_min_size(pos2(full.center().x - size.x / 2.0, full.bottom() - size.y - 4.0), size);
    let p = ui.painter();
    p.rect_filled(rect, OVERLAY_RADIUS, Color32::from_rgba_unmultiplied(16, 18, 24, 240));
    p.rect_stroke(rect, OVERLAY_RADIUS, Stroke::new(1.5, if recording { RED.gamma_multiply(0.7) } else { AMBER.gamma_multiply(0.7) }), egui::StrokeKind::Inside);
    let t = ui.input(|i| i.time) as f32;
    let row_y = rect.top() + 26.0;
    let dot = pos2(rect.left() + 26.0, row_y);
    if recording {
        let pulse = 0.5 + 0.5 * (t * 4.0).sin();
        p.circle_filled(dot, 7.0 + 5.0 * pulse, RED.gamma_multiply(0.25));
        p.circle_filled(dot, 6.0, RED);
    } else {
        for i in 0..3 {
            let a = t * 5.0 + i as f32 * 2.1;
            p.circle_filled(dot + Vec2::angled(a) * 6.0, 2.5, AMBER);
        }
    }
    let label = if recording { "Listening" } else { "Transcribing" };
    p.text(pos2(dot.x + 20.0, row_y), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(16.0), Color32::WHITE);
    if recording {
        // Five bars that follow the mic, each wobbling a little out of step.
        for i in 0..5 {
            let h = 6.0 + 22.0 * (level.sqrt() * (0.6 + 0.4 * (t * 9.0 + i as f32 * 1.3).sin().abs())).min(1.0);
            let x = rect.right() - 80.0 + i as f32 * 12.0;
            p.rect_filled(egui::Rect::from_center_size(pos2(x, row_y), vec2(6.0, h)), 3.0, ACCENT);
        }
    }
    if !preview.is_empty() {
        let chars: Vec<char> = preview.chars().collect();
        let n = chars.len().saturating_sub(60);
        let tail: String = (if n > 0 { "…" } else { "" }).chars().chain(chars[n..].iter().copied()).collect();
        p.text(pos2(rect.left() + 22.0, rect.bottom() - 22.0), egui::Align2::LEFT_CENTER, tail, egui::FontId::proportional(14.0), MUTED);
    }
    rect
}
