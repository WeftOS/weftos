//! Colour snapshot of interaction states on the public GUI surfaces.
//!
//! Companion to `colour_snapshot.rs`: the same tessellation-level dump (fixed
//! size, fixed clock, every vertex colour), but with scripted input events so
//! the frame that is dumped shows a hover, a held press, keyboard focus, a
//! selection or a mode. Widget positions are read from a probe render of the
//! same surface, so the scripts do not hard-code layout. Every dump is
//! rendered twice and must be identical.
//!
//! Surfaces: a gallery of stock egui widgets under the WeftOS theme (hover,
//! press, click and Tab focus for each), the patch editor (select, connect
//! mode, inspector, drag), and the conversation graph (selection, dimming,
//! scrub, and a model that has every role, state and edge kind).
//!
//! See `docs/development/gui-colour-snapshot.md`.

#![cfg(not(target_arch = "wasm32"))]

#[path = "../src/snap_support.rs"]
mod snap_support;

use clawft_gui_egui::explorer::conversation_graph::model::Classification;
use clawft_gui_egui::explorer::conversation_graph::{
    ConversationGraphPanel, GEdge, GNode, GraphModel, NodeRole, NodeStateTag, ViewTime,
};
use clawft_gui_egui::explorer::viewers::graph_edit;
use serde_json::{Value, json};
use snap_support::{Script, probe, render_script_checked, text_centre, text_origin, text_target};

fn setup(ctx: &egui::Context) {
    clawft_gui_egui::theming::apply(ctx);
    egui_extras::install_image_loaders(ctx);
}

// ---------------------------------------------------------------------------
// Widget gallery
// ---------------------------------------------------------------------------

fn setup_dyn(ctx: &egui::Context) {
    setup(ctx)
}

#[test]
fn colsnap_state_gallery_pointer() {
    snap_support::gallery_pointer("state_gal", &setup_dyn);
}

#[test]
fn colsnap_state_gallery_focus_keyboard() {
    snap_support::gallery_focus_keyboard("state_gal", &setup_dyn);
}

#[test]
fn colsnap_state_gallery_text_edit() {
    snap_support::gallery_text_edit("state_gal", &setup_dyn);
}

#[test]
fn colsnap_state_gallery_slider_and_combo() {
    snap_support::gallery_slider_and_combo("state_gal", &setup_dyn);
}

// ---------------------------------------------------------------------------
// Patch editor (graph_edit)
// ---------------------------------------------------------------------------

fn edit_value() -> Value {
    json!({
        "nodes": [
            { "id": "n1", "label": "Oscillator", "kind": "osc", "pos": [70.0, 70.0] },
            { "id": "n2", "label": "Filter", "kind": "vcf", "pos": [200.0, 150.0] },
            { "id": "n3", "label": "Output", "kind": "out", "pos": [330.0, 70.0] },
            { "id": "n4", "label": "Envelope", "pos": [120.0, 260.0] }
        ],
        "edges": [
            { "source": "n1", "target": "n2", "kind": "audio" },
            { "source": "n2", "target": "n3", "kind": "audio" }
        ]
    })
}

fn editor(value: Value) -> impl Fn() -> Box<dyn FnMut(&mut egui::Ui)> {
    move || {
        let v = value.clone();
        Box::new(move |ui: &mut egui::Ui| graph_edit::paint(ui, "substrate/test/patch", &v))
    }
}

fn edit(name: &str, script: &Script) -> String {
    render_script_checked(name, setup, script, editor(edit_value()))
}

struct EditPoints {
    n1: egui::Pos2,
    n2: egui::Pos2,
    n3: egui::Pos2,
    n4: egui::Pos2,
    select_btn: egui::Pos2,
    connect_btn: egui::Pos2,
    add_btn: egui::Pos2,
    delete_btn: egui::Pos2,
}

fn edit_points() -> EditPoints {
    let base = probe(setup, &Script::new(), editor(edit_value()));
    // Monospace 10 pt labels, about 6 points per character.
    EditPoints {
        n1: text_centre(&base, "Oscillator", 10, 6.0),
        n2: text_centre(&base, "Filter", 6, 6.0),
        n3: text_centre(&base, "Output", 6, 6.0),
        n4: text_centre(&base, "Envelope", 8, 6.0),
        select_btn: text_target(&base, "Select"),
        connect_btn: text_target(&base, "Connect"),
        add_btn: text_target(&base, "+ Node"),
        delete_btn: text_target(&base, "Delete"),
    }
}

#[test]
fn colsnap_state_graph_edit_pointer() {
    let p = edit_points();
    edit("state_edit_plain", &Script::new());
    for (n, pos) in [("n1", p.n1), ("n2", p.n2), ("n3", p.n3), ("n4", p.n4)] {
        edit(&format!("state_edit_hover_{n}"), &Script::new().hover(pos));
        edit(&format!("state_edit_press_{n}"), &Script::new().press(pos));
        edit(&format!("state_edit_select_{n}"), &Script::new().click(pos));
    }
    for (n, pos) in [
        ("select", p.select_btn),
        ("connect", p.connect_btn),
        ("add", p.add_btn),
        ("delete_disabled", p.delete_btn),
    ] {
        edit(
            &format!("state_edit_hover_btn_{n}"),
            &Script::new().hover(pos),
        );
        edit(
            &format!("state_edit_press_btn_{n}"),
            &Script::new().press(pos),
        );
    }
    // Empty canvas click while something is selected clears the selection.
    edit(
        "state_edit_deselect",
        &Script::new()
            .click(p.n2)
            .click(p.n4 + egui::vec2(150.0, 0.0)),
    );
    // Selected node with the pointer hovering a different node.
    edit(
        "state_edit_selected_hover_other",
        &Script::new().click(p.n2).hover(p.n1),
    );
}

#[test]
fn colsnap_state_graph_edit_modes() {
    let p = edit_points();
    // The "Connect: pick ..." hint adds a line above the canvas, which moves
    // every node down; find them again in connect mode.
    let entered = Script::new().click(p.connect_btn);
    let cm = probe(setup, &entered, editor(edit_value()));
    let n1 = text_centre(&cm, "Oscillator", 10, 6.0);
    let n3 = text_centre(&cm, "Output", 6, 6.0);
    edit("state_edit_connect_mode", &entered);
    edit(
        "state_edit_connect_mode_hover_node",
        &entered.clone().hover(n1),
    );
    let src = entered.clone().click(n1);
    edit("state_edit_connect_source", &src);
    edit(
        "state_edit_connect_source_hover_target",
        &src.clone().hover(n3),
    );
    edit("state_edit_connect_done", &src.clone().click(n3));
    edit("state_edit_connect_self", &src.clone().click(n1));
    edit(
        "state_edit_connect_back_to_select",
        &entered.clone().click(p.select_btn),
    );
    // Selection held while switching to connect mode.
    edit(
        "state_edit_selected_then_connect",
        &Script::new().click(p.n2).click(p.connect_btn),
    );
}

#[test]
fn colsnap_state_graph_edit_edit_ops() {
    let p = edit_points();
    edit("state_edit_add_node", &Script::new().click(p.add_btn));
    let dirty = Script::new().click(p.add_btn);
    let dirty_base = probe(setup, &dirty, editor(edit_value()));
    let reset = text_target(&dirty_base, "Reset");
    edit("state_edit_dirty_hover_reset", &dirty.clone().hover(reset));
    edit("state_edit_dirty_press_reset", &dirty.clone().press(reset));
    edit("state_edit_dirty_reset_clicked", &dirty.click(reset));
    edit(
        "state_edit_delete_selected",
        &Script::new().click(p.n2).click(p.delete_btn),
    );
    edit(
        "state_edit_selected_hover_delete",
        &Script::new().click(p.n2).hover(p.delete_btn),
    );
    // Drag the selected node.
    let sel = Script::new().click(p.n2);
    edit(
        "state_edit_drag_node",
        &sel.clone()
            .press(p.n2)
            .drag_to(p.n2 + egui::vec2(30.0, 20.0))
            .drag_to(p.n2 + egui::vec2(60.0, 30.0)),
    );
    edit(
        "state_edit_drag_node_released",
        &sel.press(p.n2)
            .drag_to(p.n2 + egui::vec2(30.0, 20.0))
            .release(p.n2 + egui::vec2(30.0, 20.0)),
    );
}

#[test]
fn colsnap_state_graph_edit_inspector() {
    let p = edit_points();
    let sel = Script::new().click(p.n2);
    let after = probe(setup, &sel, editor(edit_value()));
    let label = text_origin(&after, "label").expect("inspector label") + egui::vec2(55.0, 6.0);
    let kind = text_origin(&after, "kind").expect("inspector kind") + egui::vec2(55.0, 6.0);
    let remove = text_target(&after, "×");
    edit("state_edit_inspector", &sel);
    edit(
        "state_edit_inspector_hover_label",
        &sel.clone().hover(label),
    );
    edit(
        "state_edit_inspector_focus_label",
        &sel.clone().click(label),
    );
    edit(
        "state_edit_inspector_typed_label",
        &sel.clone().click(label).text("Z"),
    );
    edit(
        "state_edit_inspector_select_all_label",
        &sel.clone()
            .click(label)
            .key(egui::Key::A, egui::Modifiers::COMMAND),
    );
    edit("state_edit_inspector_focus_kind", &sel.clone().click(kind));
    edit(
        "state_edit_inspector_typed_kind",
        &sel.clone().click(kind).text("q"),
    );
    edit(
        "state_edit_inspector_hover_remove_edge",
        &sel.clone().hover(remove),
    );
    edit(
        "state_edit_inspector_press_remove_edge",
        &sel.clone().press(remove),
    );
    edit("state_edit_inspector_removed_edge", &sel.click(remove));
}

#[test]
fn colsnap_state_graph_edit_keyboard_and_empty() {
    for n in 1..=8usize {
        let mut s = Script::new();
        for _ in 0..n {
            s = s.key(egui::Key::Tab, egui::Modifiers::NONE);
        }
        edit(&format!("state_edit_tab_{n:02}"), &s);
    }
    render_script_checked(
        "state_edit_empty",
        setup,
        &Script::new(),
        editor(json!({ "nodes": [], "edges": [] })),
    );
}

// ---------------------------------------------------------------------------
// Conversation graph
// ---------------------------------------------------------------------------

fn cg(name: &str, script: &Script, mutate: impl Fn(&mut ConversationGraphPanel)) -> String {
    render_script_checked(name, setup, script, || {
        let mut p = ConversationGraphPanel::with_fixture();
        mutate(&mut p);
        move |ui: &mut egui::Ui| p.show(ui)
    })
}

#[test]
fn colsnap_state_conversation_graph_selection() {
    let fixture = ConversationGraphPanel::with_fixture();
    let ids: Vec<String> = fixture.model.nodes.iter().map(|n| n.id.clone()).collect();
    assert!(!ids.is_empty());
    cg("state_cg_plain", &Script::new(), |_| {});
    for id in &ids {
        let id = id.clone();
        cg(
            &format!("state_cg_selected_{id}"),
            &Script::new(),
            move |p| p.interact.select(Some(id.clone())),
        );
    }
    let first = ids[0].clone();
    cg("state_cg_selected_lod", &Script::new(), move |p| {
        p.interact.select(Some(first.clone()));
        p.interact.lod_collapse = true;
    });
    cg("state_cg_lod_collapse", &Script::new(), |p| {
        p.interact.lod_collapse = true;
    });
    cg("state_cg_no_clusters", &Script::new(), |p| {
        p.interact.show_clusters = false;
    });
    // Hover and click on the canvas: not wired to any colour today, kept so a
    // future change that wires it shows up as a difference.
    cg(
        "state_cg_pointer_hover_click",
        &Script::new()
            .hover(egui::pos2(300.0, 200.0))
            .click(egui::pos2(300.0, 200.0)),
        |_| {},
    );
    cg(
        "state_cg_pointer_press",
        &Script::new().press(egui::pos2(300.0, 200.0)),
        |_| {},
    );
}

#[test]
fn colsnap_state_conversation_graph_dim_and_scrub() {
    let fixture = ConversationGraphPanel::with_fixture();
    let kinds: Vec<String> = {
        let mut k: Vec<String> = fixture.model.edges.iter().map(|e| e.kind.clone()).collect();
        k.sort();
        k.dedup();
        k
    };
    let intents: Vec<String> = {
        let mut k: Vec<String> = fixture
            .model
            .nodes
            .iter()
            .filter_map(|n| n.classification.intent.clone())
            .collect();
        k.sort();
        k.dedup();
        k
    };
    for kind in &kinds {
        let kind = kind.clone();
        cg(
            &format!("state_cg_dim_edge_{kind}"),
            &Script::new(),
            move |p| p.interact.toggle_edge_filter(&kind),
        );
    }
    for intent in &intents {
        let intent = intent.clone();
        cg(
            &format!("state_cg_dim_intent_{intent}"),
            &Script::new(),
            move |p| {
                p.interact.dim_intents.insert(intent.clone());
            },
        );
    }
    let all_kinds = kinds.clone();
    let all_intents = intents.clone();
    cg("state_cg_dim_all", &Script::new(), move |p| {
        for k in &all_kinds {
            p.interact.toggle_edge_filter(k);
        }
        for i in &all_intents {
            p.interact.dim_intents.insert(i.clone());
        }
    });
    let (lo, hi) = {
        let t: Vec<u64> = fixture.model.nodes.iter().map(|n| n.ts_ms).collect();
        (*t.iter().min().unwrap(), *t.iter().max().unwrap())
    };
    for (name, t) in [
        ("before", lo.saturating_sub(500)),
        ("first", lo),
        ("mid", (lo + hi) / 2),
        ("last", hi),
        ("after", hi + 5_000),
    ] {
        cg(
            &format!("state_cg_pinned_{name}"),
            &Script::new(),
            move |p| {
                p.view_time = ViewTime::Pinned(t);
            },
        );
    }
    cg("state_cg_pinned_full_history", &Script::new(), move |p| {
        p.view_time = ViewTime::Pinned(lo);
        p.interact.full_history = true;
    });
    // Keyboard scrub: from Live, ArrowLeft pins to 0, ArrowRight steps on, L
    // returns to Live.
    cg(
        "state_cg_key_left",
        &Script::new().key(egui::Key::ArrowLeft, egui::Modifiers::NONE),
        |_| {},
    );
    cg(
        "state_cg_key_left_right",
        &Script::new()
            .key(egui::Key::ArrowLeft, egui::Modifiers::NONE)
            .key(egui::Key::ArrowRight, egui::Modifiers::NONE),
        |_| {},
    );
    cg(
        "state_cg_key_left_then_live",
        &Script::new()
            .key(egui::Key::ArrowLeft, egui::Modifiers::NONE)
            .key(egui::Key::L, egui::Modifiers::NONE),
        |_| {},
    );
}

/// One topic for every node. Topic hulls are drawn largest first (ties by topic
/// id), so a multi-topic model is reproducible too; see
/// `colsnap_state_conversation_graph_two_topics`.
fn full_model(topic: &str) -> GraphModel {
    full_model_topics(&[topic])
}

/// [`full_model`] with the nodes dealt round-robin across `topics`, so the
/// topic hulls overlap.
fn full_model_topics(topics: &[&str]) -> GraphModel {
    let roles = [
        NodeRole::User,
        NodeRole::Assistant,
        NodeRole::Tool,
        NodeRole::Speaker,
    ];
    let states = [
        NodeStateTag::Speculative,
        NodeStateTag::Frontier,
        NodeStateTag::Committed,
        NodeStateTag::Stale,
        NodeStateTag::Pruned,
    ];
    let intents = ["question", "statement", "command"];
    let mut nodes = Vec::new();
    let mut i = 0u64;
    for r in roles {
        for s in states {
            nodes.push(GNode {
                id: format!("m{i}"),
                chain_seq: Some(i),
                conv_id: "full".into(),
                role: r,
                text: format!("{} {} node number {i}", r.as_str(), s.as_str()),
                state: s,
                ts_ms: 1_000 + i * 500,
                classification: Classification {
                    intent: Some(intents[(i % 3) as usize].into()),
                    topic: Some(topics[(i as usize) % topics.len()].into()),
                    emotion: if i.is_multiple_of(4) {
                        Some("calm".into())
                    } else {
                        None
                    },
                    goal: if i.is_multiple_of(5) {
                        Some("g".into())
                    } else {
                        None
                    },
                },
            });
            i += 1;
        }
    }
    let kinds = [
        "Follows",
        "Contradicts",
        "Enables",
        "EvidenceFor",
        "Speaker",
        "Continuer",
        "EmotionCause",
        "GoalMotivation",
        "Other",
    ];
    let mut edges = Vec::new();
    for (k, kind) in kinds.iter().enumerate() {
        for j in 0..(nodes.len() - 1) {
            if j % kinds.len() == k {
                edges.push(GEdge {
                    source: nodes[j].id.clone(),
                    target: nodes[j + 1].id.clone(),
                    kind: (*kind).into(),
                    weight: 1.0,
                });
            }
        }
    }
    GraphModel {
        conv_id: "full".into(),
        nodes,
        edges,
    }
}

fn cg_full(name: &str, script: &Script, mutate: impl Fn(&mut ConversationGraphPanel)) -> String {
    cg_full_topic(name, "alpha", script, mutate)
}

fn cg_full_topic(
    name: &str,
    topic: &'static str,
    script: &Script,
    mutate: impl Fn(&mut ConversationGraphPanel),
) -> String {
    render_script_checked(name, setup, script, || {
        let mut p = ConversationGraphPanel {
            model: full_model(topic),
            view_time: ViewTime::Live,
            interact: Default::default(),
            ecc_enabled: true,
            use_fixture: false,
        };
        mutate(&mut p);
        move |ui: &mut egui::Ui| p.show(ui)
    })
}

/// Every role x state x edge kind, which the small fixture does not reach.
#[test]
fn colsnap_state_conversation_graph_full_model() {
    cg_full("state_cgfull_plain", &Script::new(), |_| {});
    cg_full_topic("state_cgfull_plain_beta", "beta", &Script::new(), |_| {});
    for sel in ["m0", "m4", "m9", "m12", "m19"] {
        cg_full(
            &format!("state_cgfull_selected_{sel}"),
            &Script::new(),
            move |p| p.interact.select(Some(sel.into())),
        );
    }
    cg_full("state_cgfull_lod", &Script::new(), |p| {
        p.interact.lod_collapse = true;
    });
    cg_full("state_cgfull_no_clusters", &Script::new(), |p| {
        p.interact.show_clusters = false;
    });
    cg_full("state_cgfull_dim_all_edges", &Script::new(), |p| {
        for k in [
            "Follows",
            "Contradicts",
            "Enables",
            "EvidenceFor",
            "Speaker",
            "Continuer",
            "EmotionCause",
            "GoalMotivation",
            "Other",
        ] {
            p.interact.toggle_edge_filter(k);
        }
    });
    cg_full("state_cgfull_dim_all_intents", &Script::new(), |p| {
        for i in ["question", "statement", "command"] {
            p.interact.dim_intents.insert(i.into());
        }
    });
    cg_full("state_cgfull_pinned_mid", &Script::new(), |p| {
        p.view_time = ViewTime::Pinned(5_000);
    });
}

/// Two interleaved topics give two overlapping hulls. Their draw order used to
/// follow `HashMap` iteration order, which differs per map, so the z-order and
/// the tessellated shape order changed from run to run. Each render builds a
/// fresh map (fresh hash seed); every dump must be identical.
#[test]
fn colsnap_state_conversation_graph_two_topics() {
    let render_one = || {
        let c = egui::Context::default();
        setup(&c);
        let mut p = ConversationGraphPanel {
            model: full_model_topics(&["alpha", "beta"]),
            view_time: ViewTime::Live,
            interact: Default::default(),
            ecc_enabled: true,
            use_fixture: false,
        };
        snap_support::render(&c, Vec::new(), move |ui: &mut egui::Ui| p.show(ui))
    };
    let first = render_one();
    for run in 1..32 {
        assert_eq!(
            first,
            render_one(),
            "conversation graph with two overlapping topics differs on run {run}"
        );
    }
    snap_support::emit("state_cgfull_two_topics", &first);
}
