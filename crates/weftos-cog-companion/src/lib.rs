//! Shared shell for sensor-cog companion apps (WeftOS ADR-104).
//!
//! A sensor app implements [`SensorApp`] (its live view, its sensor checks, its calibration
//! panel, and any extra polling of the cog's export) and gets the rest from [`Companion`]:
//! the Seed connection bar, start/stop/test-run, the hook-up checklist (the four connection
//! steps + the app's steps, blocked in order, each with a "?" into the cog's guide), a cog
//! settings panel generated from the cog's manifest, the Live/Guide tabs, and native + wasm
//! entry points.

pub mod client;
#[cfg(not(target_arch = "wasm32"))]
pub mod shot;
pub mod widgets;

pub use client::{CogClient, CogState, Settings};
pub use widgets::{Mark, Step, step};

use eframe::egui::{self, RichText};
use weftos_sensor_guide::GuideView;

/// What a probe of one endpoint last returned.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Probe {
    #[default]
    Unknown,
    Ok,
    Failed(String),
}

impl Probe {
    pub fn is_ok(&self) -> bool {
        matches!(self, Probe::Ok)
    }
}

/// The sensor-specific part of a companion app.
pub trait SensorApp: 'static {
    fn cog_id(&self) -> &'static str;
    fn export_port(&self) -> u16;
    fn title(&self) -> &'static str;
    /// Console command for the "Test run" button (must be in the cog's allowed_commands).
    fn test_command(&self) -> &'static str {
        "--once --simulate"
    }
    /// Extra polling (e.g. the cog's raw/frame endpoints). Called every frame.
    fn tick(&mut self, ctx: &egui::Context, cog: &mut CogClient);
    /// The sensor's checklist steps, after the four connection steps.
    fn steps(&self, st: &CogState) -> Vec<Step>;
    /// The Live tab body.
    fn live(&mut self, ui: &mut egui::Ui, st: &CogState, cog: &mut CogClient);
    /// The right panel (readouts, calibration tools); the settings panel is appended below.
    fn side(&mut self, ui: &mut egui::Ui, st: &CogState, cog: &mut CogClient);
    /// A short wiring table shown under the checklist: (from, to).
    fn wiring(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }
    /// The Seed changed: drop any data held from the previous one.
    fn on_reconnect(&mut self) {}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Live,
    Guide,
}

pub struct Companion<A: SensorApp> {
    pub app: A,
    pub cog: CogClient,
    draft: Settings,
    tab: Tab,
    guide: GuideView,
    cfg_draft: Option<serde_json::Map<String, serde_json::Value>>,
}

impl<A: SensorApp> Companion<A> {
    pub fn new(app: A, settings: Settings) -> Self {
        let cog = CogClient::new(app.cog_id(), app.export_port(), settings.clone());
        let tab = if client::env("COMPANION_TAB").is_some_and(|t| t == "guide") {
            Tab::Guide
        } else {
            Tab::Live
        };
        Self {
            app,
            cog,
            draft: settings,
            tab,
            guide: GuideView::at(client::env("COMPANION_PAGE")),
            cfg_draft: None,
        }
    }
}

impl<A: SensorApp> eframe::App for Companion<A> {
    // eframe 0.34 requires `ui`; the shell is driven from `update`, as in clawft-gui-egui.
    fn ui(&mut self, _ui: &mut egui::Ui, _frame: &mut eframe::Frame) {}

    #[allow(deprecated)]
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(web_time::Duration::from_millis(100));
        self.cog.tick(ctx);
        self.app.tick(ctx, &mut self.cog);
        #[cfg(not(target_arch = "wasm32"))]
        shot::poll(ctx);
        let st = self.cog.snapshot();
        let seed_ok = st.seed_api.is_ok();

        egui::TopBottomPanel::top("companion_conn").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong(self.app.title());
                ui.separator();
                ui.label("Seed")
                    .on_hover_text("169.254.42.1 over USB, or the Seed's LAN / tailnet IP");
                let host = ui.add(
                    egui::TextEdit::singleline(&mut self.draft.seed_host).desired_width(150.0),
                );
                ui.label("token")
                    .on_hover_text("optional pairing token, if the agent refuses writes");
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.seed_token)
                        .password(true)
                        .desired_width(80.0),
                );
                let enter = host.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.button("Connect").clicked() || enter) && self.draft != self.cog.settings {
                    self.cog.reconnect(self.draft.clone());
                    self.app.on_reconnect();
                    self.cfg_draft = None;
                }
                let s = &self.cog.settings;
                ui.label(
                    RichText::new(format!(
                        "agent {}  ·  export {}",
                        s.agent_base(),
                        self.cog.export_base()
                    ))
                    .small()
                    .weak(),
                );
            });
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(seed_ok, |ui| {
                    if ui
                        .button("Start cog")
                        .on_hover_text("continuous mode: keeps the export up")
                        .clicked()
                    {
                        self.cog.action("start", None, ctx);
                    }
                    if ui.button("Stop cog").clicked() {
                        self.cog.action("stop", None, ctx);
                    }
                    let cmd = self.app.test_command();
                    if ui
                        .button("Test run")
                        .on_hover_text(format!("console {cmd}"))
                        .clicked()
                    {
                        self.cog.action(
                            "console",
                            Some(serde_json::json!({ "command": cmd })),
                            ctx,
                        );
                    }
                });
                ui.label(match st.cog_running {
                    Some(true) => RichText::new("cog running").color(widgets::GREEN),
                    Some(false) => RichText::new("cog stopped").color(widgets::AMBER),
                    None => RichText::new("cog ?").color(widgets::GREY),
                });
                if let Some(a) = &st.last_action {
                    ui.separator();
                    ui.label(RichText::new(a).italics());
                }
            });
            // Simulate mode must never be buried in Advanced settings: say so up top, with a
            // one-click way back to the real sensor.
            if st.config.as_ref().is_some_and(|c| c["simulate"] == serde_json::Value::Bool(true)) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("SIMULATED: the cog is generating synthetic data, not reading the sensor.").color(widgets::AMBER).strong());
                    if ui.add_enabled(seed_ok, egui::Button::new("Use the real sensor")).on_hover_text("sets simulate = false (the cog restarts)").clicked() {
                        let mut ch = serde_json::Map::new();
                        ch.insert("simulate".into(), serde_json::Value::Bool(false));
                        self.cog.put_config(&ch, ctx);
                    }
                });
            }
        });

        egui::SidePanel::left("companion_checklist")
            .resizable(true)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.heading("Hook-up checklist");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let mut steps =
                        widgets::connection_steps(&st, self.cog.cog_id, &self.cog.export_base());
                    steps.extend(self.app.steps(&st));
                    if let Some(id) = widgets::checklist(ui, &widgets::chain(steps)) {
                        if let Some(Ok(g)) = &st.guide {
                            self.guide.open_for_step(g, id);
                        }
                        self.tab = Tab::Guide;
                    }
                    let wiring = self.app.wiring();
                    if !wiring.is_empty() {
                        ui.separator();
                        egui::CollapsingHeader::new("Wiring (3.3 V only)")
                            .default_open(true)
                            .show(ui, |ui| {
                                egui::Grid::new("companion_wiring")
                                    .striped(true)
                                    .show(ui, |ui| {
                                        for (a, b) in wiring {
                                            ui.label(*a);
                                            ui.label(*b);
                                            ui.end_row();
                                        }
                                    });
                            });
                    }
                });
            });

        if self.tab == Tab::Live {
            egui::SidePanel::right("companion_side")
                .resizable(true)
                .default_width(310.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        self.app.side(ui, &st, &mut self.cog);
                        ui.separator();
                        egui::CollapsingHeader::new("Cog settings")
                            .default_open(true)
                            .show(ui, |ui| {
                                if let Some(changes) =
                                    widgets::config_panel(ui, &st, &mut self.cfg_draft, seed_ok)
                                {
                                    self.cog.put_config(&changes, ctx);
                                }
                            });
                    });
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Live, RichText::new("Live").size(16.0));
                ui.selectable_value(&mut self.tab, Tab::Guide, RichText::new("Guide").size(16.0));
            });
            ui.separator();
            match self.tab {
                Tab::Live => self.app.live(ui, &st, &mut self.cog),
                Tab::Guide => match &st.guide {
                    Some(Ok(g)) => self.guide.show(ui, g),
                    Some(Err(e)) => {
                        ui.label(RichText::new(format!("The cog's guide could not be loaded: {e}")).color(widgets::RED));
                        ui.label("The cog serves it at <export>/guide. Start the cog, or set COMPANION_GUIDE_DIR to a local guide folder.");
                    }
                    None => {
                        ui.label("Loading the guide from the cog...");
                    }
                },
            }
        });
    }
}

/// Native entry point.
#[cfg(not(target_arch = "wasm32"))]
pub fn run_native<A: SensorApp>(app: A, window_title: &str) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 820.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title(window_title),
        ..Default::default()
    };
    eframe::run_native(
        window_title,
        options,
        Box::new(move |_cc| Ok(Box::new(Companion::new(app, Settings::default())))),
    )
}

/// Browser entry point: mounts the app on `<canvas id=canvas_id>`. `?seed=<host>` picks the Seed.
#[cfg(target_arch = "wasm32")]
pub async fn start_web<A: SensorApp>(app: A, canvas_id: &str) -> Result<(), wasm_bindgen::JsValue> {
    use wasm_bindgen::JsCast;
    console_error_panic_hook::set_once();
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| wasm_bindgen::JsValue::from_str("no document"))?;
    let canvas = document
        .get_element_by_id(canvas_id)
        .ok_or_else(|| wasm_bindgen::JsValue::from_str("canvas not found"))?
        .dyn_into::<web_sys::HtmlCanvasElement>()?;
    eframe::WebRunner::new()
        .start(
            canvas,
            eframe::WebOptions::default(),
            Box::new(move |_cc| Ok(Box::new(Companion::new(app, Settings::default())))),
        )
        .await
}
