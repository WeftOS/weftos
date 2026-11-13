use std::sync::Arc;

use eframe::egui;
use serde_json::Value;

use super::{DemoState, TerminalLineKind};
use crate::live::{Command, Live};

/// Simple RPC REPL. Commands:
/// - `help` — list commands
/// - `clear` — clear scrollback
/// - `status` / `ps` / `services` — kernel.status / kernel.ps / kernel.services
/// - `logs [n]` — kernel.logs with optional count
/// - `rpc <method> [json]` — raw RPC call with optional JSON params
pub fn show(ui: &mut egui::Ui, state: &mut DemoState, live: &Arc<Live>) {
    ui.heading("Terminal — RPC console");
    ui.label("Send commands to the kernel daemon. Try `help`, `status`, `ps`, `logs 10`.");
    ui.separator();

    // Drain any pending RPC replies the command sender parked in state.
    drain_replies(state);

    egui::Frame::new()
        .fill(crate::theming::palette::BLOCK_TERM_BG)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_min_height(320.0);
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(320.0)
                .show(ui, |ui| {
                    for (kind, line) in &state.terminal_history {
                        let color = match kind {
                            TerminalLineKind::Input => crate::theming::palette::BLOCK_TERM_INPUT,
                            TerminalLineKind::Output => crate::theming::palette::BLOCK_TERM_OUTPUT,
                            TerminalLineKind::Error => crate::theming::palette::BLOCK_TERM_ERROR,
                        };
                        let prefix = match kind {
                            TerminalLineKind::Input => "$ ",
                            _ => "  ",
                        };
                        ui.label(
                            egui::RichText::new(format!("{prefix}{line}"))
                                .monospace()
                                .color(color),
                        );
                    }
                });
        });

    ui.horizontal(|ui| {
        ui.label("$");
        let resp = ui.add(
            egui::TextEdit::singleline(&mut state.terminal_input)
                .desired_width(ui.available_width() - 60.0)
                .font(egui::TextStyle::Monospace),
        );
        let submit = ui.button("run").clicked()
            || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if submit {
            let cmd = std::mem::take(&mut state.terminal_input);
            run_command(state, live, cmd);
            resp.request_focus();
        }
    });
}

fn run_command(state: &mut DemoState, live: &Arc<Live>, cmd: String) {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return;
    }
    state
        .terminal_history
        .push((TerminalLineKind::Input, cmd.clone()));

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    match (parts.next(), parts.next()) {
        (Some("help"), _) => {
            push_out(
                state,
                "commands: help, clear, status, ps, services, logs [n], rpc <method> [json]",
            );
        }
        (Some("clear"), _) => {
            state.terminal_history.clear();
        }
        (Some("status"), _) => dispatch_rpc(state, live, "kernel.status", Value::Null),
        (Some("ps"), _) => dispatch_rpc(state, live, "kernel.ps", Value::Null),
        (Some("services"), _) => dispatch_rpc(state, live, "kernel.services", Value::Null),
        (Some("logs"), tail) => {
            let count = tail
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(20);
            dispatch_rpc(
                state,
                live,
                "kernel.logs",
                serde_json::json!({ "count": count }),
            );
        }
        (Some("rpc"), Some(rest)) => {
            let (method, params_str) = rest
                .split_once(char::is_whitespace)
                .map(|(m, p)| (m, p.trim()))
                .unwrap_or((rest, ""));
            let params = if params_str.is_empty() {
                Value::Null
            } else {
                match serde_json::from_str(params_str) {
                    Ok(v) => v,
                    Err(e) => {
                        push_err(state, format!("invalid JSON params: {e}"));
                        return;
                    }
                }
            };
            dispatch_rpc(state, live, method, params);
        }
        (Some("rpc"), None) => push_err(state, "usage: rpc <method> [json]".into()),
        (Some(unknown), _) => {
            push_err(state, format!("{unknown}: command not found — try `help`"));
        }
        _ => {}
    }
}

fn dispatch_rpc(state: &mut DemoState, live: &Arc<Live>, method: &str, params: Value) {
    let (tx, rx) = crate::live::reply_channel();
    state.pending_rpcs.push(PendingRpc {
        method: method.to_string(),
        rx,
    });
    let submitted = live.submit(Command::Raw {
        method: method.to_string(),
        params,
        reply: Some(tx),
    });
    if !submitted {
        push_err(state, "poller queue full — retry in a moment".into());
        state.pending_rpcs.pop();
    }
}

fn drain_replies(state: &mut DemoState) {
    let pending = std::mem::take(&mut state.pending_rpcs);
    let mut still = Vec::with_capacity(pending.len());
    for mut p in pending {
        match crate::live::try_recv_reply(&mut p.rx) {
            crate::live::TryReply::Done(Ok(value)) => {
                let pretty =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                for line in pretty.lines() {
                    state
                        .terminal_history
                        .push((TerminalLineKind::Output, line.to_string()));
                }
            }
            crate::live::TryReply::Done(Err(err)) => {
                push_err(state, format!("{}: {err}", p.method));
            }
            crate::live::TryReply::Empty => {
                still.push(p);
            }
            crate::live::TryReply::Closed => {
                push_err(state, format!("{}: reply channel closed", p.method));
            }
        }
    }
    state.pending_rpcs = still;
}

fn push_out(state: &mut DemoState, s: impl Into<String>) {
    state
        .terminal_history
        .push((TerminalLineKind::Output, s.into()));
}
fn push_err(state: &mut DemoState, s: String) {
    state.terminal_history.push((TerminalLineKind::Error, s));
}

/// A single outstanding RPC whose reply we're waiting for.
pub struct PendingRpc {
    pub method: String,
    pub rx: crate::live::ReplyRx,
}
