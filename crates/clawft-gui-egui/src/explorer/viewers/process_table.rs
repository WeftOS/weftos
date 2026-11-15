//! `ProcessTableViewer` — renders a `kernel.ps`-shaped array as a
//! sortable table.
//!
//! Priority **12**. Chain-tail (also 12) matches on `seq + ts + kind`
//! so there's no shape collision — process rows have `pid + name/agent_id`.
//!
//! Accepts the canonical `kernel.ps` shape (`{ pid, agent_id, state,
//! memory_bytes, cpu_time_ms, ... }`) as well as any array of objects
//! that carries `pid` + a name field (`name` or `agent_id`) plus at
//! least one of `cpu` / `cpu_time_ms` / `mem` / `memory_bytes` /
//! `state`. Anything looser falls through to JsonFallback.

use super::SubstrateViewer;
use serde_json::Value;

pub struct ProcessTableViewer;

fn row_has_shape(o: &serde_json::Map<String, Value>) -> bool {
    if o.get("pid").and_then(Value::as_u64).is_none() {
        return false;
    }
    let has_name = o.get("name").and_then(Value::as_str).is_some()
        || o.get("agent_id").and_then(Value::as_str).is_some();
    if !has_name {
        return false;
    }
    o.get("cpu").is_some()
        || o.get("cpu_time_ms").is_some()
        || o.get("mem").is_some()
        || o.get("memory_bytes").is_some()
        || o.get("state").is_some()
}

impl SubstrateViewer for ProcessTableViewer {
    fn matches(value: &Value) -> u32 {
        let Some(arr) = value.as_array() else {
            return 0;
        };
        if arr.is_empty() {
            return 0;
        }
        for item in arr {
            let Some(o) = item.as_object() else {
                return 0;
            };
            if !row_has_shape(o) {
                return 0;
            }
        }
        12
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let arr = match value.as_array() {
            Some(a) => a,
            None => return,
        };

        ui.label(
            egui::RichText::new(format!("processes · {path}  ({} rows)", arr.len()))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );
        ui.add_space(4.0);

        // Per-path sort state persisted in egui memory.
        let id = egui::Id::new(("weft-explorer-ps-sort", path));
        let sort_col = ui
            .ctx()
            .data_mut(|d| d.get_temp::<SortCol>(id).unwrap_or(SortCol::Pid));

        let mut rows: Vec<&Value> = arr.iter().collect();
        rows.sort_by(|a, b| sort_col.cmp(a, b));

        egui::ScrollArea::vertical()
            .max_height(360.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new(("ps_grid", path))
                    .num_columns(5)
                    .spacing([12.0, 2.0])
                    .striped(true)
                    .show(ui, |ui| {
                        header(ui, "pid", sort_col == SortCol::Pid, id, SortCol::Pid);
                        header(ui, "name", sort_col == SortCol::Name, id, SortCol::Name);
                        header(ui, "state", sort_col == SortCol::State, id, SortCol::State);
                        header(ui, "cpu", sort_col == SortCol::Cpu, id, SortCol::Cpu);
                        header(ui, "mem", sort_col == SortCol::Mem, id, SortCol::Mem);
                        ui.end_row();

                        for row in rows {
                            let Some(o) = row.as_object() else {
                                continue;
                            };
                            let pid = o.get("pid").and_then(Value::as_u64).unwrap_or(0);
                            let name = o
                                .get("name")
                                .and_then(Value::as_str)
                                .or_else(|| o.get("agent_id").and_then(Value::as_str))
                                .unwrap_or("?");
                            let state = o.get("state").and_then(Value::as_str).unwrap_or("-");
                            let cpu = cpu_str(o);
                            let mem = mem_str(o);

                            ui.monospace(pid.to_string());
                            ui.label(name);
                            ui.label(egui::RichText::new(state).color(state_color(state)));
                            ui.monospace(cpu);
                            ui.monospace(mem);
                            ui.end_row();
                        }
                    });
            });
    }
}

fn header(ui: &mut egui::Ui, label: &str, selected: bool, id: egui::Id, col: SortCol) {
    let text = if selected {
        egui::RichText::new(format!("{label} ▼"))
            .small()
            .strong()
            .color(crate::theming::palette::PROC_PID_TEXT)
    } else {
        egui::RichText::new(label).small().weak()
    };
    if ui.selectable_label(selected, text).clicked() {
        ui.ctx().data_mut(|d| d.insert_temp(id, col));
    }
}

fn cpu_str(o: &serde_json::Map<String, Value>) -> String {
    if let Some(ms) = o.get("cpu_time_ms").and_then(Value::as_u64) {
        return format!("{ms} ms");
    }
    if let Some(pct) = o.get("cpu").and_then(Value::as_f64) {
        return format!("{pct:.1}%");
    }
    "-".to_string()
}

fn mem_str(o: &serde_json::Map<String, Value>) -> String {
    if let Some(bytes) = o.get("memory_bytes").and_then(Value::as_u64) {
        return format_bytes(bytes);
    }
    if let Some(mb) = o.get("mem").and_then(Value::as_f64) {
        return format!("{mb:.1} MB");
    }
    "-".to_string()
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn state_color(state: &str) -> egui::Color32 {
    match state {
        "running" | "ready" | "active" => crate::theming::palette::STATUS_OK,
        "sleeping" | "idle" | "waiting" => crate::theming::palette::STATUS_INFO,
        "stopped" | "paused" => crate::theming::palette::STATUS_WARN,
        "zombie" | "failed" | "error" | "crashed" => crate::theming::palette::STATUS_ERR,
        _ => crate::theming::palette::STATUS_NEUTRAL,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortCol {
    Pid,
    Name,
    State,
    Cpu,
    Mem,
}

impl SortCol {
    fn cmp(&self, a: &Value, b: &Value) -> std::cmp::Ordering {
        let ao = a.as_object();
        let bo = b.as_object();
        match (ao, bo) {
            (Some(ao), Some(bo)) => match self {
                SortCol::Pid => ao
                    .get("pid")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .cmp(&bo.get("pid").and_then(Value::as_u64).unwrap_or(0)),
                SortCol::Name => {
                    let an = ao
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| ao.get("agent_id").and_then(Value::as_str))
                        .unwrap_or("");
                    let bn = bo
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| bo.get("agent_id").and_then(Value::as_str))
                        .unwrap_or("");
                    an.cmp(bn)
                }
                SortCol::State => {
                    let as_ = ao.get("state").and_then(Value::as_str).unwrap_or("");
                    let bs = bo.get("state").and_then(Value::as_str).unwrap_or("");
                    as_.cmp(bs)
                }
                SortCol::Cpu => {
                    let a_ms = ao.get("cpu_time_ms").and_then(Value::as_u64);
                    let b_ms = bo.get("cpu_time_ms").and_then(Value::as_u64);
                    if a_ms.is_some() || b_ms.is_some() {
                        b_ms.unwrap_or(0).cmp(&a_ms.unwrap_or(0))
                    } else {
                        let ac = ao.get("cpu").and_then(Value::as_f64).unwrap_or(0.0);
                        let bc = bo.get("cpu").and_then(Value::as_f64).unwrap_or(0.0);
                        bc.partial_cmp(&ac).unwrap_or(std::cmp::Ordering::Equal)
                    }
                }
                SortCol::Mem => {
                    let a_b = ao.get("memory_bytes").and_then(Value::as_u64);
                    let b_b = bo.get("memory_bytes").and_then(Value::as_u64);
                    if a_b.is_some() || b_b.is_some() {
                        b_b.unwrap_or(0).cmp(&a_b.unwrap_or(0))
                    } else {
                        let am = ao.get("mem").and_then(Value::as_f64).unwrap_or(0.0);
                        let bm = bo.get("mem").and_then(Value::as_f64).unwrap_or(0.0);
                        bm.partial_cmp(&am).unwrap_or(std::cmp::Ordering::Equal)
                    }
                }
            },
            _ => std::cmp::Ordering::Equal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kernel_ps_fixture() -> Value {
        json!([
            {
                "pid": 1,
                "agent_id": "kernel",
                "state": "running",
                "memory_bytes": 1_048_576_u64,
                "cpu_time_ms": 1200_u64,
                "parent_pid": null
            },
            {
                "pid": 2,
                "agent_id": "mic-capture",
                "state": "sleeping",
                "memory_bytes": 524288_u64,
                "cpu_time_ms": 80_u64,
                "parent_pid": 1
            },
        ])
    }

    #[test]
    fn matches_kernel_ps_shape() {
        assert_eq!(ProcessTableViewer::matches(&kernel_ps_fixture()), 12);
    }

    #[test]
    fn matches_generic_shape() {
        let v = json!([
            { "pid": 10, "name": "foo", "cpu": 12.5, "mem": 64.0, "state": "running" },
        ]);
        assert_eq!(ProcessTableViewer::matches(&v), 12);
    }

    #[test]
    fn rejects_empty_array() {
        assert_eq!(ProcessTableViewer::matches(&json!([])), 0);
    }

    #[test]
    fn rejects_missing_pid() {
        let v = json!([{ "name": "foo", "state": "running" }]);
        assert_eq!(ProcessTableViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_missing_name_and_agent_id() {
        let v = json!([{ "pid": 1, "state": "running" }]);
        assert_eq!(ProcessTableViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_missing_signal_columns() {
        let v = json!([{ "pid": 1, "name": "foo" }]);
        assert_eq!(ProcessTableViewer::matches(&v), 0);
    }

    #[test]
    fn rejects_non_array() {
        assert_eq!(ProcessTableViewer::matches(&json!({"pid": 1})), 0);
        assert_eq!(ProcessTableViewer::matches(&Value::Null), 0);
    }

    #[test]
    fn rejects_if_any_row_malformed() {
        let v = json!([
            { "pid": 1, "name": "ok", "state": "running" },
            { "name": "no-pid", "state": "running" },
        ]);
        assert_eq!(ProcessTableViewer::matches(&v), 0);
    }

    #[test]
    fn format_bytes_thresholds() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
    }

    #[test]
    fn paint_does_not_panic_on_kernel_ps() {
        let ctx = egui::Context::default();
        let raw_input = egui::RawInput::default();
        let v = kernel_ps_fixture();
        let _ = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                ProcessTableViewer::paint(ui, "substrate/kernel/ps", &v);
            });
        });
    }
}
