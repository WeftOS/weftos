//! Theming lab — a tabbed local mirror of https://www.egui.rs/#demo
//! with WeftOS theming applied (and toggleable for side-by-side proof).
//!
//! `egui_demo_lib` (the library) only exports `DemoWindows` + `ColorTest`.
//! The Fractal Clock, HTTP, and Custom 3D demos live in `egui_demo_app`
//! (a bin-only crate). We vendor those three files verbatim from
//! `emilk/egui @ 0.29.1` into `demo_lab_vendored/` and include them via
//! `#[path]` so the upstream code is never modified.

use clawft_gui_egui::theming;
use eframe::egui;
use egui_demo_lib::{ColorTest, DemoWindows};

#[path = "demo_lab_vendored/custom3d_glow.rs"]
mod custom3d_glow;
#[path = "demo_lab_vendored/fractal_clock.rs"]
mod fractal_clock;
#[path = "demo_lab_vendored/http_app.rs"]
mod http_app;

use custom3d_glow::Custom3d;
use fractal_clock::FractalClock;
use http_app::HttpApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 520.0])
            .with_title("WeftOS — egui demo lab (themed)"),
        ..Default::default()
    };
    eframe::run_native(
        "WeftOS egui demo lab",
        options,
        Box::new(|cc| Ok(Box::new(DemoLab::new(cc)))),
    )
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Tab {
    Demos,
    FractalClock,
    Http,
    Custom3d,
    ColorTest,
}

impl Tab {
    const ALL: [(Tab, &'static str); 5] = [
        (Tab::Demos, "Demos"),
        (Tab::FractalClock, "Fractal Clock"),
        (Tab::Http, "HTTP"),
        (Tab::Custom3d, "3D"),
        (Tab::ColorTest, "Rendering test"),
    ];
}

struct DemoLab {
    tab: Tab,
    theme_on: bool,
    prev_theme_on: bool,

    demo_windows: DemoWindows,
    color_test: ColorTest,
    fractal_clock: FractalClock,
    http_app: HttpApp,
    custom3d: Option<Custom3d>,

    /// Start wall time in seconds since unix epoch — feeds Fractal Clock.
    start_secs: f64,
}

impl DemoLab {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theming::apply(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);

        Self {
            tab: Tab::Demos,
            theme_on: true,
            prev_theme_on: true,
            demo_windows: DemoWindows::default(),
            color_test: ColorTest::default(),
            fractal_clock: FractalClock::default(),
            http_app: HttpApp::default(),
            custom3d: Custom3d::new(cc),
            start_secs: now_secs(),
        }
    }

    fn header(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("WeftOS").strong());
            ui.label(egui::RichText::new("egui demo lab").weak());
            ui.separator();

            for (tab, label) in Tab::ALL {
                if ui.selectable_label(self.tab == tab, label).clicked() {
                    self.tab = tab;
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Theme toggle — the undeniable proof. Flip and watch the widgets change.
                let lbl = if self.theme_on {
                    "WeftOS theme: on"
                } else {
                    "WeftOS theme: OFF"
                };
                if ui.toggle_value(&mut self.theme_on, lbl).changed() {
                    // handled in update() to avoid double-apply this frame
                }
                ui.separator();
                // Token swatches — accent + bg_surface + bg_panel — visible at a glance.
                let tokens = theming::Tokens::default();
                swatch(ui, tokens.accent, "accent");
                swatch(ui, tokens.bg_surface, "surface");
                swatch(ui, tokens.bg_panel, "panel");
            });
        });
        let _ = ctx;
    }
}

fn swatch(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    ui.painter().rect(
        rect,
        2.0,
        color,
        egui::Stroke::new(1.0, theming::palette::DEMO_HAIRLINE),
        egui::StrokeKind::Inside,
    );
    ui.label(egui::RichText::new(label).small().weak());
    ui.add_space(4.0);
}

fn now_secs() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl eframe::App for DemoLab {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.theme_on {
            [8.0 / 255.0, 8.0 / 255.0, 10.0 / 255.0, 1.0]
        } else {
            [0.08, 0.08, 0.08, 1.0]
        }
    }

    fn ui(&mut self, _ui: &mut egui::Ui, _frame: &mut eframe::Frame) {}

    #[allow(deprecated)]
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Re-apply the theme only on toggle, so we don't fight the built-in demos' internal
        // style tweaks every frame.
        if self.theme_on != self.prev_theme_on {
            if self.theme_on {
                theming::apply(ctx);
            } else {
                ctx.set_visuals(egui::Visuals::dark());
                ctx.set_global_style(egui::Style::default());
            }
            self.prev_theme_on = self.theme_on;
        }

        egui::TopBottomPanel::top("demo_lab_header").show(ctx, |ui| {
            self.header(ctx, ui);
        });

        match self.tab {
            Tab::Demos => {
                // egui_demo_lib 0.34: `DemoWindows::ui` now takes
                // `&mut Ui`, not `&Context`. Wrap in a CentralPanel
                // to give it one.
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.demo_windows.ui(ui);
                });
            }
            Tab::FractalClock => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let t = now_secs() - self.start_secs;
                    self.fractal_clock.ui(ui, Some(t));
                });
            }
            Tab::Http => {
                // HttpApp owns its own CentralPanel via its eframe::App impl.
                eframe::App::update(&mut self.http_app, ctx, frame);
            }
            Tab::Custom3d => match &mut self.custom3d {
                Some(c) => eframe::App::update(c, ctx, frame),
                None => {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.label(
                            egui::RichText::new("glow backend not available — Custom 3D disabled")
                                .weak(),
                        );
                    });
                }
            },
            Tab::ColorTest => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::both().show(ui, |ui| {
                        self.color_test.ui(ui);
                    });
                });
            }
        }
    }
}
