//! Console behaviour through a real `DisplaySession` on the simulated
//! leaf: layout boxes, node ranges, wrapping, scroll bounds, recycling,
//! the state-to-avatar mapping, the page layout and the guide views.

use std::collections::BTreeSet;

use weftos_cos_console::avatar::{ID_CAPTION, ID_FIELD};
use weftos_cos_console::frame::open_sim_unmetered;
use weftos_cos_console::page::SWIPE_PX;
use weftos_cos_console::text::wrap;
use weftos_cos_console::theme::{
    self, ids, range_of, Box, CHAT, CHAT_COLS, CHAT_GUIDE, CHAT_VISIBLE, GUIDE_HEAD, GUIDE_SIDE,
    LINE_PITCH, PAD, PAGE, PAGE_VISIBLE, SCREEN_H, SCREEN_W, STATUS, STATUS_GUIDE, SYSTEM_STRIP,
    TOUCH_MIN, VISUAL,
};
use weftos_cos_console::{
    AgentState, Checklist, ChecklistItem, Choice, Console, DisplaySession, GuideAnswer, LeafSim,
    Layout, ObjectEvent, PageDoc, StatusLine, StepDir, Steps, Survey,
};
use weftos_leaf_scene::{from_px_q8, HitShape, Primitive, Scene};

fn sim() -> (DisplaySession, LeafSim) {
    open_sim_unmetered().unwrap()
}

fn numbers(scene: &Scene) -> Vec<u32> {
    scene.nodes.iter().map(|n| n.id.path_hash()).collect()
}

fn text_of(scene: &Scene, n: u32) -> Option<String> {
    scene.node(theme::nid(n)).and_then(|node| match &node.primitive {
        Primitive::Text { content, .. } => Some(content.clone()),
        _ => None,
    })
}

fn drag(c: &mut Console<'_>, node: u32, dx: i32, dy: i32) -> Option<GuideAnswer> {
    c.on_event(ObjectEvent::Drag { node: theme::nid(node), dx, dy }).unwrap()
}

fn tap(c: &mut Console<'_>, node: u32) -> Option<GuideAnswer> {
    c.on_event(ObjectEvent::Tap { node: theme::nid(node) }).unwrap()
}

#[test]
fn layout_boxes_match_adr_118() {
    assert_eq!(SYSTEM_STRIP, Box::new(0, 0, 800, 24));
    assert_eq!(VISUAL, Box::new(0, 24, 300, 456));
    assert_eq!(STATUS, Box::new(300, 24, 500, 64));
    assert_eq!(CHAT, Box::new(300, 88, 500, 392));
    assert_eq!(PAGE, Box::new(0, 24, 800, 456));
    assert_eq!(CHAT_GUIDE, Box::new(300, 232, 500, 248));
    assert_eq!(STATUS_GUIDE.h, 24);
    for regions in [vec![SYSTEM_STRIP, VISUAL, STATUS, CHAT], vec![SYSTEM_STRIP, GUIDE_SIDE, GUIDE_HEAD, STATUS_GUIDE, CHAT_GUIDE]] {
        for (i, a) in regions.iter().enumerate() {
            for b in &regions[i + 1..] {
                assert!(!a.overlaps(b), "{a:?} overlaps {b:?}");
            }
        }
        let area: i32 = regions.iter().map(|b| b.w * b.h).sum();
        assert_eq!(area, SCREEN_W * SCREEN_H, "regions tile the screen");
    }
    const { assert!(CHAT_VISIBLE >= 14 && CHAT_COLS >= 40) };
}

#[test]
fn split_view_nodes_sit_in_their_ranges_and_boxes() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.push_user_line("hello").unwrap();
    c.push_agent_text("hi there", 1).unwrap();
    c.set_status(StatusLine { task: "triage".into(), model: "m".into(), lanes_running: 1, ptt_active: true }).unwrap();
    let snap = sim.snapshot();
    assert!(!snap.nodes.is_empty());
    for n in &snap.nodes {
        let num = n.id.path_hash();
        let range = range_of(num).unwrap_or_else(|| panic!("node {num} outside every range"));
        assert!(range != "page" && range != "guide", "no page or guide nodes in the split");
        let Some(r) = n.aabb() else { continue };
        let (x, y) = (from_px_q8(r.x), from_px_q8(r.y));
        let (right, bottom) = (from_px_q8(r.right()), from_px_q8(r.bottom()));
        let within = |b: &Box| x >= b.x - 1 && y >= b.y - 1 && right <= b.right() + 1 && bottom <= b.bottom() + 1;
        match range {
            "avatar" => assert!(within(&VISUAL), "avatar node {num} leaves the visual panel"),
            "status" => assert!(within(&STATUS), "status node {num} leaves the status box"),
            "chat" => assert!(within(&CHAT), "chat node {num} leaves the log box"),
            _ => {}
        }
    }
}

#[test]
fn ranges_never_collide_across_a_long_session() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    let states = [AgentState::Listening, AgentState::Thinking, AgentState::Speaking, AgentState::Idle, AgentState::Blocked("x".into())];
    let mut t = 0;
    for i in 0..300u64 {
        c.set_agent_state(states[(i % 5) as usize].clone()).unwrap();
        c.push_user_line(format!("question number {i} with a few extra words to wrap around")).unwrap();
        for k in 0..3 {
            c.push_agent_text(&format!("part {k} of answer {i}; "), i).unwrap();
            t += 50;
            c.tick(t).unwrap();
        }
        if i % 7 == 0 {
            drag(&mut c, ids::CHAT_INPUT, 0, 60);
        }
        match i % 23 {
            0 => {
                c.show_page(PageDoc::paragraphs("p", vec!["a b c".into(); 40])).unwrap();
                c.hide_page().unwrap();
            }
            11 => {
                c.show_checklist(Checklist { id: "c".into(), title: "t".into(), items: vec![ChecklistItem::default(); 9] }).unwrap();
                c.set_layout(Layout::Split).unwrap();
            }
            _ => {}
        }
        let live = c.live_line_ids();
        let set: BTreeSet<_> = live.iter().collect();
        assert_eq!(set.len(), live.len(), "duplicate live line ids");
    }
    let nums = numbers(&sim.snapshot());
    let set: BTreeSet<_> = nums.iter().collect();
    assert_eq!(set.len(), nums.len(), "store holds duplicate ids");
    for n in nums {
        assert!(range_of(n).is_some(), "node {n} outside every range");
    }
}

#[test]
fn chat_lines_wrap_to_the_column() {
    let (mut s, _sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    let long = "word ".repeat(60);
    c.push_user_line(long.trim()).unwrap();
    for l in c.chat().lines() {
        assert!(l.text.chars().count() <= CHAT_COLS, "{:?} too wide", l.text);
    }
    assert!(c.chat().lines()[0].text.starts_with("> "));
    assert!(c.chat().lines()[1].text.starts_with("  "));
    assert_eq!(wrap("abc def", 3), vec!["abc", "def"]);
}

#[test]
fn scroll_is_bounded_and_marks_new_messages() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    for i in 0..40 {
        c.push_user_line(format!("line {i}")).unwrap();
    }
    let max = c.chat().total_lines() - CHAT_VISIBLE;
    drag(&mut c, ids::CHAT_INPUT, 0, LINE_PITCH * 1000);
    assert_eq!(c.chat().scroll(), max, "clamped at the oldest line");
    drag(&mut c, ids::CHAT_INPUT, 0, -LINE_PITCH * 1000);
    assert_eq!(c.chat().scroll(), 0, "clamped at the newest line");

    drag(&mut c, ids::CHAT_INPUT, 0, LINE_PITCH * 5);
    assert_eq!(c.chat().scroll(), 5);
    c.push_agent_text("arrived", 9).unwrap();
    assert!(c.chat().unread_below());
    assert_eq!(c.chat().scroll(), 6, "view keeps its place");
    assert!(text_of(&sim.snapshot(), ids::CHAT_MARKER_TEXT).unwrap().contains("new messages below"));
    tap(&mut c, ids::CHAT_MARKER_BG);
    assert!(c.chat().is_at_bottom());
    assert!(!sim.has_node(theme::nid(ids::CHAT_MARKER_TEXT)));
}

#[test]
fn chat_nodes_are_recycled_through_the_range() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    let range_len = (ids::CHAT_LINES.end - ids::CHAT_LINES.start) as usize;
    let mut seen = BTreeSet::new();
    for i in 0..(range_len / 8) {
        c.push_user_line(format!("m{i}")).unwrap();
        let live = c.live_line_ids();
        assert!(live.len() <= CHAT_VISIBLE);
        assert!(live.iter().all(|id| ids::CHAT_LINES.contains(&id.path_hash())));
        seen.extend(live.iter().map(|id| id.path_hash()));
    }
    let chat_nodes: Vec<u32> = numbers(&sim.snapshot()).into_iter().filter(|n| ids::CHAT_LINES.contains(n)).collect();
    assert_eq!(chat_nodes.len(), CHAT_VISIBLE, "only the window is in the store");
    // Every push re-keys the whole window, so the ring advances by a
    // window per line: the ids used cover the range end to end.
    assert!(seen.len() > range_len * 9 / 10, "ids used: {}", seen.len());
    assert!(*seen.iter().next().unwrap() < ids::CHAT_LINES.start + 20);
    assert!(*seen.iter().next_back().unwrap() >= ids::CHAT_LINES.end - 20);
}

fn bitmap_of(scene: &Scene, id: u32) -> Option<Vec<u8>> {
    scene.node(theme::nid(id)).and_then(|node| match &node.primitive {
        Primitive::Bitmap { data, .. } => Some(data.clone()),
        _ => None,
    })
}

#[test]
fn agent_state_maps_to_avatar_pose() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.set_agent_state(AgentState::Listening).unwrap();
    c.tick(40).unwrap();
    let snap = sim.snapshot();
    assert!(text_of(&snap, ID_CAPTION).unwrap().contains("listening"));
    assert!(bitmap_of(&snap, ID_FIELD).is_some(), "listening paints a field tile");

    c.set_agent_state(AgentState::Thinking).unwrap();
    c.tick(80).unwrap();
    assert!(text_of(&sim.snapshot(), ID_CAPTION).unwrap().contains("thinking"));

    c.set_agent_state(AgentState::Speaking).unwrap();
    c.tick(120).unwrap();
    assert!(text_of(&sim.snapshot(), ID_CAPTION).unwrap().contains("speaking"));

    c.set_agent_state(AgentState::Idle).unwrap();
    c.tick(160).unwrap();
    assert!(text_of(&sim.snapshot(), ID_CAPTION).unwrap().contains("idle"));

    c.set_agent_state(AgentState::Blocked("needs approval".into())).unwrap();
    c.tick(200).unwrap();
    let snap = sim.snapshot();
    assert!(text_of(&snap, ID_CAPTION).unwrap().contains("blocked"));
    assert!(text_of(&snap, 170).unwrap().contains("needs approval"));
    assert_eq!(text_of(&snap, 200).unwrap().trim(), "BLOCKED");
    assert!(bitmap_of(&snap, ID_FIELD).is_some(), "blocked still paints the field");
}

#[test]
fn shader_field_changes_across_frames() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.tick(40).unwrap();
    let first = bitmap_of(&sim.snapshot(), ID_FIELD).expect("first tile");
    c.tick(800).unwrap();
    let later = bitmap_of(&sim.snapshot(), ID_FIELD).expect("later tile");
    assert_ne!(first, later, "the field holds one picture");
}

#[test]
fn page_layout_shows_every_block_scrolls_and_swipes_back() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.push_user_line("before").unwrap();
    c.set_layout(Layout::Page).unwrap();
    let snap = sim.snapshot();
    assert_eq!(text_of(&snap, ids::PAGE_TITLE).unwrap(), PageDoc::example().title);
    let texts: Vec<String> = snap.nodes.iter().filter_map(|n| text_of(&snap, n.id.path_hash())).collect();
    assert!(texts.iter().any(|t| t == "MORNING"), "heading drawn upper-case");
    assert!(texts.iter().any(|t| t.starts_with("- 09:30")), "bullet drawn");
    assert!(snap.nodes.iter().any(|n| ids::PAGE_LINES.contains(&n.id.path_hash()) && matches!(n.primitive, Primitive::Line { .. })), "rule drawn");

    let paragraphs: Vec<String> = (0..30).map(|i| format!("paragraph {i} {}", "text ".repeat(30))).collect();
    c.show_page(PageDoc::paragraphs("The plan", paragraphs)).unwrap();
    assert!(c.in_page_mode());
    let total = c.page().unwrap().total_lines();
    assert!(total > PAGE_VISIBLE);
    let nums = numbers(&sim.snapshot());
    assert!(nums.iter().all(|n| ids::PAGE.contains(n) || ids::CHROME.contains(n)), "{nums:?}");

    drag(&mut c, ids::PAGE_INPUT, 0, -LINE_PITCH * 1000);
    assert_eq!(c.page().unwrap().scroll(), total - PAGE_VISIBLE, "clamped at the end");
    c.tick(10_000).unwrap(); // idle gap ends the gesture
    drag(&mut c, ids::PAGE_INPUT, 0, LINE_PITCH * 1000);
    assert_eq!(c.page().unwrap().scroll(), 0);
    c.tick(20_000).unwrap();

    drag(&mut c, ids::PAGE_INPUT, SWIPE_PX, SWIPE_PX);
    assert!(c.in_page_mode(), "a diagonal drag is not a swipe");
    c.tick(30_000).unwrap();
    drag(&mut c, ids::PAGE_INPUT, SWIPE_PX, 4);
    assert_eq!(c.layout(), Layout::Split, "swipe right returns to the split");
    assert_eq!(c.chat().total_lines(), 1, "the log survived the page");
    let nums = numbers(&sim.snapshot());
    assert!(nums.iter().any(|n| ids::CHAT.contains(n)));
    assert!(!nums.iter().any(|n| ids::PAGE.contains(n)));
}

fn checklist() -> Checklist {
    Checklist {
        id: "rel".into(),
        title: "Release".into(),
        items: (0..7)
            .map(|i| ChecklistItem { id: format!("i{i}"), text: format!("item {i}"), done: i < 3 })
            .collect(),
    }
}

#[test]
fn guide_checklist_toggles_by_tap_and_by_agent() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.push_user_line("checklist please").unwrap();
    c.show_checklist(checklist()).unwrap();
    assert_eq!(c.layout(), Layout::Guide);
    let snap = sim.snapshot();
    assert_eq!(text_of(&snap, ids::GUIDE_COUNTER).unwrap(), "3/7");
    let nums = numbers(&snap);
    assert!(nums.iter().any(|n| ids::CHAT.contains(n)), "the log is still there");
    assert!(!nums.iter().any(|n| ids::AVATAR.contains(n)));
    assert!(nums.iter().all(|n| range_of(*n).is_some()));
    // Tap the fourth row through the leaf's own hit-test.
    let y = GUIDE_SIDE.y + 40 + 3 * TOUCH_MIN + 10;
    sim.tap(GUIDE_SIDE.x + 100, y);
    let answers = c.pump_events().unwrap();
    assert_eq!(answers, vec![GuideAnswer::Toggle { checklist_id: "rel".into(), item_id: "i3".into(), done: true }]);
    assert_eq!(text_of(&sim.snapshot(), ids::GUIDE_COUNTER).unwrap(), "4/7");
    c.set_item("rel", "i0", false).unwrap();
    assert_eq!(text_of(&sim.snapshot(), ids::GUIDE_COUNTER).unwrap(), "3/7");
    c.set_item("other", "i1", false).unwrap();
    assert_eq!(text_of(&sim.snapshot(), ids::GUIDE_COUNTER).unwrap(), "3/7");
}

#[test]
fn guide_steps_move_with_next_and_back_within_bounds() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.show_steps(Steps { id: "st".into(), title: "Restart".into(), steps: (0..3).map(|i| format!("step {i}")).collect(), current: 0 }).unwrap();
    assert_eq!(tap(&mut c, ids::GUIDE_BACK), None, "no back from the first step");
    assert_eq!(tap(&mut c, ids::GUIDE_NEXT), Some(GuideAnswer::Step { steps_id: "st".into(), dir: StepDir::Next }));
    assert_eq!(text_of(&sim.snapshot(), ids::GUIDE_COUNTER).unwrap(), "Step 2 of 3");
    tap(&mut c, ids::GUIDE_NEXT);
    assert_eq!(tap(&mut c, ids::GUIDE_NEXT), None, "no next past the last step");
    assert_eq!(tap(&mut c, ids::GUIDE_BACK), Some(GuideAnswer::Step { steps_id: "st".into(), dir: StepDir::Back }));
    assert_eq!(text_of(&sim.snapshot(), ids::GUIDE_COUNTER).unwrap(), "Step 2 of 3");
}

fn survey(n: u8) -> Survey {
    Survey {
        id: "q".into(),
        question: "How did standup land today for the team?".into(),
        explain: "One tap answers.".into(),
        choices: (1..=n).map(|i| Choice { n: i, caption: format!("choice {i}") }).collect(),
        index: 2,
        total: 5,
    }
}

#[test]
fn guide_survey_answers_fill_the_button_and_skip_back_report() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.ask(survey(4)).unwrap();
    let snap = sim.snapshot();
    assert_eq!(text_of(&snap, ids::GUIDE_COUNTER).unwrap(), "Q2/5");
    // Tap button 2 through the leaf's hit-test: the round button is on
    // a full-width 68 px row.
    sim.tap(GUIDE_SIDE.x + 46, GUIDE_SIDE.y + 40 + 68 + 34);
    assert_eq!(c.pump_events().unwrap(), vec![GuideAnswer::Answer { survey_id: "q".into(), choice_n: 2 }]);
    let snap = sim.snapshot();
    let filled = snap.node(theme::nid(weftos_cos_console::guide::row_id(1, 1))).unwrap();
    assert_eq!(filled.style.fill, Some(theme::GREEN), "answered button is filled");
    let other = snap.node(theme::nid(weftos_cos_console::guide::row_id(0, 1))).unwrap();
    assert_eq!(other.style.fill, None);
    assert_eq!(tap(&mut c, ids::GUIDE_NEXT), None, "skip is off once answered");
    assert_eq!(tap(&mut c, ids::GUIDE_BACK), Some(GuideAnswer::Back { survey_id: "q".into() }));

    c.ask(survey(6)).unwrap();
    assert_eq!(tap(&mut c, ids::GUIDE_NEXT), Some(GuideAnswer::Skip { survey_id: "q".into() }));
    let snap = sim.snapshot();
    assert_eq!(snap.nodes.iter().filter(|n| n.input.is_some() && weftos_cos_console::guide::row_of(n.id).is_some()).count(), 6);
}

#[test]
fn guide_touch_targets_are_at_least_64_px() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    for view in 0..3 {
        match view {
            0 => c.show_checklist(checklist()).unwrap(),
            1 => c.show_steps(Steps { id: "st".into(), title: "t".into(), steps: vec!["a".into(); 5], current: 2 }).unwrap(),
            _ => c.ask(survey(6)).unwrap(),
        }
        for n in &sim.snapshot().nodes {
            let num = n.id.path_hash();
            if !ids::GUIDE.contains(&num) || num == ids::GUIDE_HEAD_INPUT {
                continue;
            }
            if let Some(region) = &n.input {
                let HitShape::Aabb { w, h } = region.shape else { panic!("guide targets are boxes") };
                assert!(from_px_q8(w) >= TOUCH_MIN && from_px_q8(h) >= TOUCH_MIN, "node {num} target {}x{}", from_px_q8(w), from_px_q8(h));
            }
        }
    }
}

#[test]
fn guide_swipe_returns_to_the_split_and_layouts_round_trip() {
    let (mut s, sim) = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.show_checklist(checklist()).unwrap();
    drag(&mut c, ids::GUIDE_HEAD_INPUT, SWIPE_PX + 10, 2);
    assert_eq!(c.layout(), Layout::Split);
    c.set_layout(Layout::Guide).unwrap();
    assert_eq!(text_of(&sim.snapshot(), ids::GUIDE_COUNTER).unwrap(), "3/7", "the last guide view comes back");
    c.set_layout(Layout::Page).unwrap();
    assert!(sim.has_node(theme::nid(ids::PAGE_TITLE)));
    c.set_layout(Layout::Split).unwrap();
    let nums = numbers(&sim.snapshot());
    assert!(nums.iter().any(|n| ids::AVATAR.contains(n)));
    assert!(!nums.iter().any(|n| ids::GUIDE.contains(n) || ids::PAGE.contains(n)));
}

#[test]
fn ptt_sensor_lights_the_indicator_and_leaf_touch_scrolls_the_log() {
    let (mut s, sim) = sim();
    sim.ptt(true);
    let mut c = Console::new(&mut s).unwrap();
    for i in 0..40 {
        c.push_agent_text(&format!("l{i}\n"), 1).unwrap();
    }
    c.pump_events().unwrap();
    assert!(c.status().status().ptt_active);
    // Drag down the log with a leaf touch: the chat target owns it.
    let (x, y0) = (CHAT.x + PAD + 100, CHAT.y + 200);
    sim.touch_down(x, y0);
    for i in 1..=6 {
        sim.touch_move(x, y0 + i * LINE_PITCH / 2);
    }
    sim.touch_up(x, y0 + 3 * LINE_PITCH);
    c.pump_events().unwrap();
    assert_eq!(c.chat().scroll(), 3);
    assert_eq!(sim.pixel(0, 0), (0, 0, 0));
}
