//! Boot splash: centered logo + subtle title, fade in → hold → fade out.

use eframe::egui;

use super::{BOOT_LEN, audio, boot_logo_alpha};

/// Logo bytes are embedded so the binary is self-contained. Exposed
/// `pub(crate)` so `App::new` can preload it into the image cache before
/// the first paint.
pub(crate) const LOGO_PNG: &[u8] = include_bytes!("../../assets/weftos-gold.png");

/// Render the boot splash. Returns `true` when the boot timeline has
/// elapsed and the caller should transition to the desktop.
pub fn show(ui: &mut egui::Ui, started: web_time::Instant, sfx_played: &mut bool) -> bool {
    let elapsed = started.elapsed().as_secs_f32();
    let alpha = boot_logo_alpha(elapsed);

    // Fire scuttle sound once, early in the fade-in.
    if !*sfx_played && elapsed > 0.05 {
        audio::play_scuttle();
        *sfx_played = true;
    }

    let rect = ui.max_rect();
    let center = rect.center();

    // Paint background + halo + subtitle using this ui's painter. We
    // scope the borrow so we can call `Image::paint_at` on the same ui
    // afterwards (both need to target the same layer so the logo
    // renders above the halo).
    {
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, egui::Color32::BLACK);

        let halo_color = crate::theming::palette::boot_halo((alpha * 150.0) as u8);
        painter.circle_filled(center, 300.0, halo_color);

        let subtitle_alpha = (alpha * 0.55 * 255.0) as u8;
        painter.text(
            center + egui::vec2(0.0, 240.0),
            egui::Align2::CENTER_CENTER,
            "weave the machine",
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
            crate::theming::palette::boot_subtitle(subtitle_alpha),
        );
    }

    // Logo image — drawn into the same ui/layer via `paint_at` so it sits
    // above the halo painted above.
    let logo_size = egui::vec2(420.0, 368.0);
    let logo_rect = egui::Rect::from_center_size(center, logo_size);
    let tint = crate::theming::palette::boot_tint((alpha * 255.0) as u8);
    egui::Image::from_bytes("bytes://weftos-gold.png", LOGO_PNG)
        .fit_to_exact_size(logo_size)
        .tint(tint)
        .paint_at(ui, logo_rect);

    elapsed >= BOOT_LEN
}
