use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};

use super::DemoState;

const MAX_SAMPLES: usize = 512;
const WINDOW: f64 = 4.0; // seconds visible

pub fn show(ui: &mut egui::Ui, state: &mut DemoState) {
    ui.heading("Oscilloscope");
    ui.label("Bonus block — sliding-window sine wave via egui_plot.");
    ui.separator();

    let dt = ui.input(|i| i.unstable_dt).clamp(0.001, 0.1);
    state.scope_t += dt;
    let t = state.scope_t as f64;

    // Mixed-frequency signal for visual interest.
    let y = (t * 2.0 * std::f64::consts::PI * 1.0).sin() * 0.6
        + (t * 2.0 * std::f64::consts::PI * 4.0).sin() * 0.25
        + (t * 2.0 * std::f64::consts::PI * 9.0).sin() * 0.15;

    state.scope_samples.push_back((t, y));
    while state.scope_samples.len() > MAX_SAMPLES {
        state.scope_samples.pop_front();
    }

    let points: PlotPoints = state.scope_samples.iter().map(|&(t, y)| [t, y]).collect();
    let line = Line::new("scope", points).color(crate::theming::palette::SCOPE_LINE);

    Plot::new("oscilloscope")
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .show_axes([true, true])
        .view_aspect(3.0)
        .include_y(-1.2)
        .include_y(1.2)
        .include_x(t - WINDOW)
        .include_x(t)
        .show(ui, |plot_ui| {
            plot_ui.line(line);
        });
}
