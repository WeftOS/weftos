//! Console behaviour against the in-process simulator: layout boxes,
//! node ranges, wrapping, scroll bounds, recycling, the state-to-avatar
//! mapping, and page mode.

use std::collections::BTreeSet;

use weftos_cos_console::avatar::{
    ID_BARS, ID_BRACKETS, ID_CAPTION, ID_DOTS, ID_HEAD, ID_MOUTH, ID_RINGS, ID_SCAN,
};
use weftos_cos_console::chat_log::ChatLog;
use weftos_cos_console::page::SWIPE_PX;
use weftos_cos_console::session::DisplaySession;
use weftos_cos_console::text::wrap;
use weftos_cos_console::theme::{
    self, ids, range_of, Box, CHAT, CHAT_COLS, CHAT_VISIBLE, LINE_PITCH, PAGE, PAGE_VISIBLE,
    SCREEN_H, SCREEN_W, STATUS, SYSTEM_STRIP, VISUAL,
};
use weftos_cos_console::{AgentState, Console, ObjectEvent, PageDoc, SimSession, StatusLine};
use weftos_leaf_scene::{from_px_q8, Primitive};

fn sim() -> SimSession {
    SimSession::new(800, 480)
}

fn node_numbers(s: &SimSession) -> Vec<u32> {
    s.store()
        .to_snapshot(theme::DISPLAY)
        .nodes
        .iter()
        .map(|n| n.id.path_hash())
        .collect()
}

fn text_of(s: &SimSession, n: u32) -> Option<String> {
    s.store().node(theme::nid(n)).and_then(|node| match &node.primitive {
        Primitive::Text { content, .. } => Some(content.clone()),
        _ => None,
    })
}

#[test]
fn layout_boxes_match_adr_118() {
    assert_eq!(SYSTEM_STRIP, Box::new(0, 0, 800, 24));
    assert_eq!(VISUAL, Box::new(0, 24, 300, 456));
    assert_eq!(STATUS, Box::new(300, 24, 500, 64));
    assert_eq!(CHAT, Box::new(300, 88, 500, 392));
    assert_eq!(PAGE, Box::new(0, 24, 800, 456));
    let regions = [SYSTEM_STRIP, VISUAL, STATUS, CHAT];
    for (i, a) in regions.iter().enumerate() {
        for b in &regions[i + 1..] {
            assert!(!a.overlaps(b), "{a:?} overlaps {b:?}");
        }
    }
    let area: i32 = regions.iter().map(|b| b.w * b.h).sum();
    assert_eq!(area, SCREEN_W * SCREEN_H, "regions tile the screen");
    const { assert!(CHAT_VISIBLE >= 14 && CHAT_COLS >= 40) };
}

#[test]
fn split_view_nodes_sit_in_their_ranges_and_boxes() {
    let mut s = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.push_user_line("hello").unwrap();
    c.push_agent_text("hi there", 1).unwrap();
    c.set_status(StatusLine {
        task: "triage".into(),
        model: "m".into(),
        lanes_running: 1,
        ptt_active: true,
    })
    .unwrap();
    drop(c);
    let snap = s.store().to_snapshot(theme::DISPLAY);
    assert!(!snap.nodes.is_empty());
    for n in &snap.nodes {
        let num = n.id.path_hash();
        let range = range_of(num).unwrap_or_else(|| panic!("node {num} outside every range"));
        assert_ne!(range, "page", "no page nodes in split view");
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
    s.render(100).unwrap();
}

#[test]
fn ranges_never_collide_across_a_long_session() {
    let mut s = sim();
    let mut c = Console::new(&mut s).unwrap();
    let states = [
        AgentState::Listening,
        AgentState::Thinking,
        AgentState::Speaking,
        AgentState::Idle,
        AgentState::Blocked("x".into()),
    ];
    let mut t = 0;
    for i in 0..400u64 {
        c.set_agent_state(states[(i % 5) as usize].clone()).unwrap();
        c.push_user_line(format!("question number {i} with a few extra words to wrap around")).unwrap();
        for k in 0..3 {
            c.push_agent_text(&format!("part {k} of answer {i}; "), i).unwrap();
            t += 50;
            c.tick(t).unwrap();
        }
        if i % 7 == 0 {
            c.on_event(ObjectEvent::Drag { node: theme::nid(ids::CHAT_INPUT), dx: 0, dy: 60 }).unwrap();
        }
        if i % 23 == 0 {
            c.show_page(PageDoc { title: "p".into(), paragraphs: vec!["a b c".into(); 40] }).unwrap();
            assert!(c.in_page_mode());
            c.hide_page().unwrap();
        }
        let live = c.live_line_ids();
        let set: BTreeSet<_> = live.iter().collect();
        assert_eq!(set.len(), live.len(), "duplicate live line ids");
    }
    drop(c);
    let nums = node_numbers(&s);
    let set: BTreeSet<_> = nums.iter().collect();
    assert_eq!(set.len(), nums.len(), "store holds duplicate ids");
    for n in nums {
        assert!(range_of(n).is_some(), "node {n} outside every range");
    }
}

#[test]
fn chat_lines_wrap_to_the_column() {
    let mut s = sim();
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
    let mut s = sim();
    let mut c = Console::new(&mut s).unwrap();
    for i in 0..40 {
        c.push_user_line(format!("line {i}")).unwrap();
    }
    let total = c.chat().total_lines();
    let max = total - CHAT_VISIBLE;
    let chat = theme::nid(ids::CHAT_INPUT);
    c.on_event(ObjectEvent::Drag { node: chat, dx: 0, dy: LINE_PITCH * 1000 }).unwrap();
    assert_eq!(c.chat().scroll(), max, "clamped at the oldest line");
    c.on_event(ObjectEvent::Release { node: chat }).unwrap();
    c.on_event(ObjectEvent::Drag { node: chat, dx: 0, dy: -LINE_PITCH * 1000 }).unwrap();
    assert_eq!(c.chat().scroll(), 0, "clamped at the newest line");

    c.on_event(ObjectEvent::Drag { node: chat, dx: 0, dy: LINE_PITCH * 5 }).unwrap();
    assert_eq!(c.chat().scroll(), 5);
    c.push_agent_text("arrived", 9).unwrap();
    assert!(c.chat().unread_below());
    assert_eq!(c.chat().scroll(), 6, "view keeps its place");
    drop(c);
    assert!(text_of(&s, ids::CHAT_MARKER_TEXT).unwrap().contains("new messages below"));

    let mut c = Console::new(&mut s).unwrap();
    c.on_event(ObjectEvent::Tap { node: theme::nid(ids::CHAT_MARKER_BG) }).unwrap();
    assert!(c.chat().is_at_bottom());
}

#[test]
fn chat_nodes_are_recycled_through_the_range() {
    let mut s = sim();
    let mut c = Console::new(&mut s).unwrap();
    let range_len = (ids::CHAT_LINES.end - ids::CHAT_LINES.start) as usize;
    for i in 0..(range_len + 500) {
        c.push_user_line(format!("m{i}")).unwrap();
        let live = c.live_line_ids();
        assert!(live.len() <= CHAT_VISIBLE);
        assert!(live.iter().all(|id| ids::CHAT_LINES.contains(&id.path_hash())));
    }
    let chat_nodes: Vec<u32> = node_numbers(&s)
        .into_iter()
        .filter(|n| ids::CHAT_LINES.contains(n))
        .collect();
    assert_eq!(chat_nodes.len(), CHAT_VISIBLE, "only the window is in the store");
    let log = ChatLog::new(CHAT_COLS, CHAT_VISIBLE);
    assert_eq!(log.total_lines(), 0);
}

#[test]
fn chat_pool_wraps_around() {
    let mut s = sim();
    let mut c = Console::new(&mut s).unwrap();
    let range_len = (ids::CHAT_LINES.end - ids::CHAT_LINES.start) as u64;
    for i in 0..(range_len as usize + 10) {
        c.push_user_line(format!("m{i}")).unwrap();
    }
    let live = c.live_line_ids();
    // After more allocations than ids, the newest lines carry low ids again.
    assert!(live.iter().any(|id| id.path_hash() < ids::CHAT_LINES.start + CHAT_VISIBLE as u32 + 20));
    let set: BTreeSet<_> = live.iter().collect();
    assert_eq!(set.len(), live.len());
}

fn visible(s: &SimSession, n: u32) -> bool {
    s.store().node(theme::nid(n)).map(|x| x.style.visible).unwrap_or(false)
}

#[test]
fn agent_state_maps_to_avatar_pose() {
    let mut s = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.set_agent_state(AgentState::Listening).unwrap();
    drop(c);
    assert!(ID_RINGS.iter().all(|n| visible(&s, *n)));
    assert!(ID_BRACKETS.iter().all(|n| visible(&s, *n)));
    assert!(!visible(&s, ID_SCAN));
    assert!(visible(&s, ID_MOUTH));

    let mut c = Console::new(&mut s).unwrap();
    c.set_agent_state(AgentState::Thinking).unwrap();
    c.tick(500).unwrap();
    drop(c);
    assert!(visible(&s, ID_SCAN));
    assert!(ID_DOTS.iter().all(|n| visible(&s, *n)));
    assert!(!ID_RINGS.iter().any(|n| visible(&s, *n)));

    let mut c = Console::new(&mut s).unwrap();
    c.set_agent_state(AgentState::Speaking).unwrap();
    c.tick(1000).unwrap();
    drop(c);
    assert!(ID_BARS.iter().all(|n| visible(&s, *n)));
    assert!(!visible(&s, ID_MOUTH));

    let mut c = Console::new(&mut s).unwrap();
    c.set_agent_state(AgentState::Idle).unwrap();
    drop(c);
    assert!(!ID_BARS.iter().any(|n| visible(&s, *n)));
    assert!(visible(&s, ID_MOUTH));
    assert_eq!(s.store().node(theme::nid(ID_HEAD)).unwrap().style.stroke, Some(theme::GREEN));

    let mut c = Console::new(&mut s).unwrap();
    c.set_agent_state(AgentState::Blocked("needs approval".into())).unwrap();
    assert_eq!(c.avatar().clone().tick(5000).len(), 0, "blocked pose is static");
    drop(c);
    assert_eq!(s.store().node(theme::nid(ID_HEAD)).unwrap().style.stroke, Some(theme::AMBER));
    assert!(text_of(&s, ID_CAPTION).unwrap().contains("blocked"));
    assert_eq!(text_of(&s, 200).unwrap().trim(), "BLOCKED");
    s.render(6000).unwrap();
}

#[test]
fn idle_animation_emits_tweens_and_blinks() {
    let mut s = sim();
    let mut c = Console::new(&mut s).unwrap();
    let mut saw_tween = false;
    let mut saw_blink = false;
    for t in (40..8000).step_by(40) {
        let ops = c.avatar().clone().tick(t);
        saw_tween |= ops.iter().any(|o| matches!(o, weftos_leaf_scene::SceneOp::Tween { .. }));
        c.tick(t).unwrap();
        saw_blink |= !visible(c.session(), weftos_cos_console::avatar::ID_EYES[0]);
    }
    assert!(saw_tween, "breathing uses stroke tweens");
    assert!(saw_blink, "eyes close during a blink");
}

#[test]
fn page_mode_expands_scrolls_and_swipes_back() {
    let mut s = sim();
    let mut c = Console::new(&mut s).unwrap();
    c.push_user_line("before").unwrap();
    let paragraphs: Vec<String> = (0..30).map(|i| format!("paragraph {i} {}", "text ".repeat(30))).collect();
    c.show_page(PageDoc { title: "The plan".into(), paragraphs }).unwrap();
    assert!(c.in_page_mode());
    let total = c.page().unwrap().total_lines();
    assert!(total > PAGE_VISIBLE);
    {
        let nums = node_numbers(c.session());
        assert!(nums.iter().all(|n| ids::PAGE.contains(n) || ids::CHROME.contains(n)), "{nums:?}");
        assert!(!nums.iter().any(|n| ids::CHAT.contains(n) || ids::AVATAR.contains(n)));
    }
    let page = theme::nid(ids::PAGE_INPUT);
    c.on_event(ObjectEvent::Drag { node: page, dx: 0, dy: -LINE_PITCH * 1000 }).unwrap();
    assert_eq!(c.page().unwrap().scroll(), total - PAGE_VISIBLE, "clamped at the end");
    c.on_event(ObjectEvent::Release { node: page }).unwrap();
    c.on_event(ObjectEvent::Drag { node: page, dx: 0, dy: LINE_PITCH * 1000 }).unwrap();
    assert_eq!(c.page().unwrap().scroll(), 0);
    c.on_event(ObjectEvent::Release { node: page }).unwrap();

    // A mostly-vertical drag is not a swipe; a rightward one is.
    c.on_event(ObjectEvent::Drag { node: page, dx: SWIPE_PX, dy: SWIPE_PX }).unwrap();
    assert!(c.in_page_mode());
    c.on_event(ObjectEvent::Release { node: page }).unwrap();
    c.on_event(ObjectEvent::Drag { node: page, dx: SWIPE_PX, dy: 4 }).unwrap();
    assert!(!c.in_page_mode(), "swipe right returns to the split");
    assert_eq!(c.chat().total_lines(), 1, "the log survived page mode");
    drop(c);
    let nums = node_numbers(&s);
    assert!(nums.iter().any(|n| ids::CHAT.contains(n)));
    assert!(!nums.iter().any(|n| ids::PAGE.contains(n)));
    s.render(10).unwrap();
}

#[test]
fn ptt_sensor_lights_the_indicator_and_sim_touch_routes_to_nodes() {
    let mut s = sim();
    s.push_sensor("ptt.down", 1.0);
    let mut c = Console::new(&mut s).unwrap();
    for i in 0..40 {
        c.push_agent_text(&format!("l{i}\n"), 1).unwrap();
    }
    assert_eq!(c.pump_events().unwrap(), 1);
    assert!(c.status().status().ptt_active);
    assert_eq!(c.session().size(), (800, 480));
    // Drag down the log with a synthesised touch: the chat target owns it.
    c.session().swipe((500, 300), (500, 300 + LINE_PITCH * 3), 6);
    c.pump_events().unwrap();
    assert_eq!(c.chat().scroll(), 3);
    assert_eq!(c.session().pixel(0, 0), theme::BLACK);
}
