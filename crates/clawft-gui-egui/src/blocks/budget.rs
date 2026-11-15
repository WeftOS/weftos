use eframe::egui;

struct AgentCost {
    id: &'static str,
    tokens: u64,
    cost_usd: f32,
    budget_usd: f32,
}

const AGENTS: &[AgentCost] = &[
    AgentCost {
        id: "coder",
        tokens: 1_842_000,
        cost_usd: 5.52,
        budget_usd: 10.0,
    },
    AgentCost {
        id: "reviewer",
        tokens: 612_000,
        cost_usd: 1.84,
        budget_usd: 5.0,
    },
    AgentCost {
        id: "researcher",
        tokens: 3_200_000,
        cost_usd: 8.95,
        budget_usd: 10.0,
    },
    AgentCost {
        id: "planner",
        tokens: 140_000,
        cost_usd: 0.42,
        budget_usd: 5.0,
    },
];

pub fn show(ui: &mut egui::Ui) {
    ui.heading("Budget");
    ui.label("Per-agent token usage + spend against budget.");
    ui.separator();

    let total_cost: f32 = AGENTS.iter().map(|a| a.cost_usd).sum();
    let total_budget: f32 = AGENTS.iter().map(|a| a.budget_usd).sum();
    ui.label(format!(
        "Total: ${:.2} / ${:.2} ({} agents)",
        total_cost,
        total_budget,
        AGENTS.len()
    ));
    ui.add_space(8.0);

    for a in AGENTS {
        let pct = (a.cost_usd / a.budget_usd).clamp(0.0, 1.0);
        let color = if pct >= 0.9 {
            crate::theming::palette::BLOCK_BAR_ERR
        } else if pct >= 0.7 {
            crate::theming::palette::BLOCK_BAR_WARN
        } else {
            crate::theming::palette::BLOCK_BAR_OK
        };

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("{:>11}", a.id)).monospace());
            ui.label(format!("{}k tok", a.tokens / 1000));
            ui.label(format!("${:.2} / ${:.2}", a.cost_usd, a.budget_usd));
        });
        let bar = egui::ProgressBar::new(pct)
            .desired_width(ui.available_width())
            .fill(color);
        ui.add(bar);
        ui.add_space(4.0);
    }
}
