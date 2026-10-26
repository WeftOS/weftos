//! The Catalog tab: Projects, Modules, Chips and the Hardware Dex.

use crate::app::Manager;
use crate::sensor_detail::{self, PanelCtx};
use crate::sensor_mesh;
use crate::style;
use crate::{AMBER, COG, GREEN, GREY, WL};
use eframe::egui::{self, Color32, RichText};
use weftos_cog_market::hw::{Chip, Module, Project};

fn mod_hay(m: &Module) -> String {
    format!("{} {} {} {} {} {} {} {}", m.id, m.name, m.vendor, m.kind, m.summary, m.chips.join(" "), m.good_for.join(" "), m.spec.values().cloned().collect::<Vec<_>>().join(" ")).to_lowercase()
}

fn chip_hay(c: &Chip) -> String {
    format!("{} {} {} {} {} {} {}", c.id, c.name, c.manufacturer, c.role, c.summary, c.tags.join(" "), c.spec.values().cloned().collect::<Vec<_>>().join(" ")).to_lowercase()
}

fn proj_hay(p: &Project) -> String {
    format!("{} {} {} {} {}", p.name, p.category, p.difficulty, p.summary, p.modules.join(" ")).to_lowercase()
}

pub(crate) fn buy_and_datasheet(ui: &mut egui::Ui, buy: Option<(&str, &str)>, datasheet: &str) {
    if buy.is_none_or(|(_, u)| u.is_empty()) && datasheet.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        if let Some((price, url)) = buy.filter(|(_, u)| !u.is_empty()) {
            ui.hyperlink_to(RichText::new(format!("🛒 {} ↗", if price.is_empty() { "Mouser" } else { price })).color(GREEN), url);
        }
        if !datasheet.is_empty() {
            ui.hyperlink_to(RichText::new("datasheet ↗").color(WL), datasheet);
        }
    });
}

/// A tag strip (chips / modules / sensor tags) rendered as uniform pills so every card's
/// metadata row reads the same.
pub(crate) fn tag_row<'a>(ui: &mut egui::Ui, tags: impl Iterator<Item = &'a str>, color: Color32) {
    ui.horizontal_wrapped(|ui| {
        for t in tags {
            style::pill(ui, t, color);
        }
    });
}

fn chip_card(ui: &mut egui::Ui, c: &Chip) {
    style::card(
        ui,
        ("chip", &c.id),
        |ui| {
            style::truncated(ui, &c.name, true);
            if !c.role.is_empty() {
                style::pill(ui, &c.role, COG);
            }
            if !c.manufacturer.is_empty() {
                style::truncated(ui, &c.manufacturer, false);
            }
        },
        |ui| {
            if !c.summary.is_empty() {
                ui.label(style::body(&c.summary));
            }
            if !c.tags.is_empty() {
                tag_row(ui, c.tags.iter().map(String::as_str), WL);
            }
            buy_and_datasheet(ui, None, &c.datasheet);
            style::spec_grid(ui, ("cs", &c.id), c.spec.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        },
    );
}

fn project_card(ui: &mut egui::Ui, p: &Project) {
    style::card(
        ui,
        ("proj", &p.id),
        |ui| {
            style::truncated(ui, &p.name, true);
            if !p.category.is_empty() {
                style::pill(ui, &p.category, AMBER);
            }
            if !p.difficulty.is_empty() {
                ui.label(style::dim(ui, &p.difficulty));
            }
        },
        |ui| {
            if !p.summary.is_empty() {
                ui.label(style::body(&p.summary));
            }
            if !p.modules.is_empty() {
                tag_row(ui, p.modules.iter().map(String::as_str), GREY);
            }
        },
    );
}

/// Count line (scannable, at the top) + the cards, or a centered "nothing matched" when the
/// filter is empty. Shared by the Projects / Modules / Chips tabs so they behave identically.
fn render_catalog_list<T>(ui: &mut egui::Ui, noun: &str, items: &[&T], search: &str, render: impl Fn(&mut egui::Ui, &T)) {
    ui.label(style::dim(ui, format!("{} {noun}", items.len())));
    ui.add_space(style::GAP_XS);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if items.is_empty() {
            ui.add_space(style::GAP_M);
            ui.vertical_centered(|ui| {
                let msg = if search.is_empty() { format!("No {noun} here yet.") } else { format!("No {noun} match “{search}”.") };
                ui.label(style::dim(ui, msg));
            });
            return;
        }
        for it in items {
            render(ui, it);
        }
    });
}

impl Manager {
    pub(crate) fn catalog_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            style::h2(ui, "Hardware catalog");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("🔍  Identify hardware").on_hover_text("scan the host's USB bus and match devices to the catalog").clicked() {
                    self.hw.open(&self.client, ctx);
                }
            });
        });
        style::caption(
            ui,
            format!(
                "{} projects · {} modules · {} chips — Projects → Modules → Chips (explored across several projects)",
                self.catalog.projects.len(),
                self.catalog.modules.len(),
                self.catalog.chips.len()
            ),
        );
        ui.add_space(style::GAP_S);
        // Filter bar: the tab picker, the search field and (for modules) the kind chips read
        // as one toolbar instead of three loose widget rows.
        style::card_frame(ui).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.cat_tab, 0u8, "Projects");
                ui.selectable_value(&mut self.cat_tab, 1u8, "Modules");
                ui.selectable_value(&mut self.cat_tab, 2u8, "Chips");
                ui.selectable_value(&mut self.cat_tab, 3u8, "Dex");
                ui.separator();
                ui.label(style::dim(ui, "🔍"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.cat_search)
                        .hint_text("search name, vendor, spec, what it senses…")
                        .desired_width(300.0),
                );
                if !self.cat_search.is_empty() && ui.button("clear").on_hover_text("clear search").clicked() {
                    self.cat_search.clear();
                }
            });
            if self.cat_tab == 1 {
                ui.add_space(style::GAP_XS);
                ui.horizontal_wrapped(|ui| {
                    ui.label(style::dim(ui, "kind"));
                    for k in ["all", "board", "sensor", "display", "actuator", "tool"] {
                        ui.selectable_value(&mut self.cat_kind, k.to_string(), k);
                    }
                });
            }
        });
        ui.add_space(style::GAP_S);
        if self.cat_tab == 3 {
            self.dex.show(ui, ctx, &self.client, &self.catalog);
            return;
        }
        let q = self.cat_search.to_lowercase();
        let (tab, kind) = (self.cat_tab, self.cat_kind.clone());
        let cat = &self.catalog;
        let search = &self.cat_search;
        // Filter first so the result count can sit at the top where it's scannable, and an
        // empty result reads as a clear "nothing matched" instead of a bare "0".
        match tab {
            0 => {
                let items: Vec<_> = cat.projects.iter().filter(|p| q.is_empty() || proj_hay(p).contains(&q)).collect();
                render_catalog_list(ui, "projects", &items, search, project_card);
            }
            2 => {
                let items: Vec<_> = cat.chips.iter().filter(|c| q.is_empty() || chip_hay(c).contains(&q)).collect();
                render_catalog_list(ui, "chips", &items, search, chip_card);
            }
            _ => {
                let items: Vec<_> = cat
                    .modules
                    .iter()
                    .filter(|m| (kind == "all" || m.kind == kind) && (q.is_empty() || mod_hay(m).contains(&q)))
                    .collect();
                let (market, host) = {
                    let sh = self.client.snapshot();
                    (sh.catalog.clone(), sh.host.clone().and_then(|r| r.ok()))
                };
                self.client.ensure_mesh(ctx);
                let mesh = {
                    let sh = self.client.snapshot();
                    sensor_mesh::mesh_view(sh.mesh.as_ref().and_then(|f| f.result.as_ref()), host.as_ref())
                };
                let pc = PanelCtx { client: &self.client, egui: ctx, catalog: &self.catalog, market: market.as_ref(), host: host.as_ref(), mesh: Some(&mesh), guides: &self.guides, events: &self.events, target: &self.targets, focus_step: self.focus_step.filter(|_| ctx.input(|i| i.time) < 6.5) };
                let focus = self.focus_module.clone();
                render_catalog_list(ui, "modules", &items, search, |ui, m| sensor_detail::module_card(ui, m, &pc, focus.as_deref() == Some(m.id.as_str())));
                self.focus_module = None;
            }
        }
    }
}
