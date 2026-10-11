//! `cos-console-demo`: replay a scripted conversation through a real
//! `DisplaySession` on the simulated leaf, in real time (so the
//! session's op budget is respected), and write key frames as PNGs
//! rendered from the leaf's snapshot.
//!
//! ```bash
//! scripts/build.sh cos-console-demo            # writes docs/design/cos-console/*.png
//! cargo run --release --bin cos-console-demo -- --out <dir> [--fast]
//! ```

use std::path::PathBuf;
use std::time::Duration;

use weftos_cos_console::frame::{open_sim, open_sim_unmetered, render_png};
use weftos_cos_console::theme::{CHAT, GUIDE_HEAD, GUIDE_SIDE, PAD, TOUCH_MIN};
use weftos_cos_console::{
    AgentState, Checklist, ChecklistItem, Choice, Console, LeafSim, Layout, PageDoc, StatusLine,
    Steps, Survey,
};

const FRAME_MS: u32 = 40;

struct Replay<'a> {
    console: Console<'a>,
    sim: LeafSim,
    now_ms: u32,
    out: PathBuf,
    realtime: bool,
}

impl Replay<'_> {
    /// Advance the clock, ticking animation and events every frame.
    fn advance(&mut self, ms: u32) {
        let frames = ms.div_ceil(FRAME_MS).max(1);
        for _ in 0..frames {
            self.now_ms += FRAME_MS;
            if self.realtime {
                std::thread::sleep(Duration::from_millis(FRAME_MS as u64));
            }
            for a in self.console.pump_events().expect("events") {
                eprintln!("  guide answer: {a:?}");
            }
            self.console.tick(self.now_ms).expect("tick");
        }
    }

    fn snapshot(&mut self, name: &str) {
        let path = self.out.join(format!("{name}.png"));
        render_png(&self.sim.snapshot(), &path).expect("png");
        eprintln!("wrote {}  (t={} ms)", path.display(), self.now_ms);
    }

    fn stream(&mut self, text: &str, msg_id: u64, chunk: usize, ms_per_chunk: u32) {
        let chars: Vec<char> = text.chars().collect();
        for piece in chars.chunks(chunk) {
            let s: String = piece.iter().collect();
            self.console.push_agent_text(&s, msg_id).expect("agent text");
            self.advance(ms_per_chunk);
        }
    }

    /// A finger gesture from `from` to `to` in `steps` moves.
    fn swipe(&mut self, from: (i32, i32), to: (i32, i32), steps: i32) {
        self.sim.touch_down(from.0, from.1);
        for i in 1..=steps {
            let x = from.0 + (to.0 - from.0) * i / steps;
            let y = from.1 + (to.1 - from.1) * i / steps;
            self.sim.touch_move(x, y);
        }
        self.sim.touch_up(to.0, to.1);
        self.advance(FRAME_MS);
    }

    fn tap(&mut self, x: i32, y: i32) {
        self.sim.tap(x, y);
        self.advance(FRAME_MS);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut out = PathBuf::from("docs/design/cos-console");
    let mut realtime = true;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(args.next().expect("--out <dir>")),
            "--fast" => realtime = false,
            _ => {}
        }
    }
    std::fs::create_dir_all(&out).expect("create output dir");

    let (mut session, sim) = if realtime { open_sim() } else { open_sim_unmetered() }.expect("session");
    let console = Console::new(&mut session).expect("console");
    let mut r = Replay {
        console,
        sim,
        now_ms: 0,
        out,
        realtime,
    };
    r.console.set_time("09:14").unwrap();
    r.console
        .set_status(StatusLine {
            task: String::new(),
            model: "hermes-3-8b (local)".into(),
            lanes_running: 2,
            ptt_active: false,
        })
        .unwrap();

    // Earlier exchanges, so the log has history to dim and to scroll.
    r.console.push_user_line("Did the arm lane finish?").unwrap();
    r.stream(
        "Yes. The aarch64 build passed on the OrbStack VM and the artifacts are signed. \
         Nothing left to do there.",
        10,
        40,
        0,
    );
    r.console.push_user_line("Remind me what is still open on the licence relay.").unwrap();
    r.stream("Two cards: the retry on a stale token, and the log line for a refused checkout.", 11, 40, 0);
    r.console.push_user_line("Morning. Anything overnight?").unwrap();
    r.stream(
        "Quiet night. The cog-repo lane finished its x86 build and the dashboard reporter \
         posted two notes. Nothing needs you before standup.",
        1,
        40,
        0,
    );
    r.advance(1300);
    r.snapshot("01-idle");

    // Push to talk, listen.
    r.sim.ptt(true);
    r.console.set_agent_state(AgentState::Listening).unwrap();
    r.advance(700);
    r.snapshot("02-listening");

    // Transcript lands, the agent thinks on a frontier model.
    r.sim.ptt(false);
    r.console
        .push_user_line("What's on my plate this morning, and is anything blocked?")
        .unwrap();
    r.console.set_agent_state(AgentState::Thinking).unwrap();
    r.console
        .set_status(StatusLine {
            task: "triage: board + lanes".into(),
            model: "claude-sonnet-5-5".into(),
            lanes_running: 3,
            ptt_active: false,
        })
        .unwrap();
    r.advance(900);
    r.snapshot("03-thinking");

    // Streaming reply.
    r.console.set_agent_state(AgentState::Speaking).unwrap();
    let reply = "Three things. First, the display-client lane is waiting on your review of the \
                 session API; that is the only blocker. Second, two cards are ready for the \
                 licence relay, both small. Third, PG's cog host restarted twice at 03:10 and \
                 recovered; I filed a note on the board.\n\nShall I open the plan for the day?";
    let chars: Vec<char> = reply.chars().collect();
    let half = chars.len() / 2;
    let first: String = chars[..half].iter().collect();
    let second: String = chars[half..].iter().collect();
    r.stream(&first, 2, 6, 40);
    r.snapshot("04-streaming");
    r.stream(&second, 2, 6, 40);
    r.console.set_agent_state(AgentState::Idle).unwrap();
    r.console
        .set_status(StatusLine {
            task: String::new(),
            model: "hermes-3-8b (local)".into(),
            lanes_running: 3,
            ptt_active: false,
        })
        .unwrap();
    r.advance(400);

    // Scroll back, then a new message arrives below.
    let x = CHAT.x + 200;
    r.swipe((x, CHAT.y + 160), (x, CHAT.y + 300), 8);
    r.console.push_user_line("Yes, show me the plan.").unwrap();
    r.console.set_agent_state(AgentState::Thinking).unwrap();
    r.advance(300);
    r.snapshot("05-scrolled");

    // Page layout with the example page, then swipe back.
    r.console.show_page(PageDoc::example()).unwrap();
    r.console.set_agent_state(AgentState::Idle).unwrap();
    r.advance(200);
    r.snapshot("06-page");
    r.swipe((400, 300), (600, 310), 10);
    assert_eq!(r.console.layout(), Layout::Split);

    // Back at the bottom of the log (a tap on the marker), then the
    // guide layout: a checklist, with one item toggled by touch.
    r.tap(CHAT.x + PAD + 100, CHAT.bottom() - 12);
    r.console.push_user_line("Walk me through the release checklist.").unwrap();
    r.console
        .show_checklist(Checklist {
            id: "rel-0.8.4".into(),
            title: "Release 0.8.4 checklist".into(),
            items: [
                ("gate", "Phase gate green on target-0.8.4", true),
                ("notes", "Release notes drafted", true),
                ("sign", "Signing key present in the release environment", true),
                ("pg", "PG rollout pinned in Terraform", false),
                ("mac", "Mac binaries verified from the archive", false),
                ("board", "Board cards moved to done", false),
                ("tag", "Tag pushed and release published", false),
            ]
            .into_iter()
            .map(|(id, text, done)| ChecklistItem { id: id.into(), text: text.into(), done })
            .collect(),
        })
        .unwrap();
    r.stream("Seven items; three are done. Tap one to toggle it, or tell me.", 3, 40, 0);
    r.advance(200);
    r.tap(GUIDE_SIDE.x + 150, GUIDE_SIDE.y + 40 + 3 * TOUCH_MIN + 32); // fourth row
    r.advance(200);
    r.snapshot("08-guide-checklist");

    // Guide: steps, advanced once by the next button.
    r.console
        .show_steps(Steps {
            id: "pg-cog-host".into(),
            title: "Restart the PG cog host".into(),
            steps: vec![
                "Drain the queue: weaver cog host drain --wait".into(),
                "Stop the unit: systemctl stop weft-cog-host".into(),
                "Swap the binary from the verified archive".into(),
                "Start the unit and watch the first heartbeat".into(),
                "Confirm the catalog shows the new version".into(),
            ],
            current: 0,
        })
        .unwrap();
    r.stream("Five steps. Say next when each one is done.", 4, 40, 0);
    r.advance(200);
    r.tap(GUIDE_HEAD.right() - PAD - 60, GUIDE_HEAD.bottom() - 10 - TOUCH_MIN / 2); // next
    r.advance(200);
    r.snapshot("09-guide-steps");

    // Guide: a three-question survey; the first answer is on screen.
    let questions = [
        ("How did standup land?", "One tap. Your answer goes to the agent as a user turn.", vec![
            (1, "Clear, nothing to follow up"),
            (2, "Fine, one open question"),
            (3, "Rough, needs a note"),
        ]),
        ("Priority for the afternoon?", "Pick one; the board gets re-ordered to match.", vec![
            (1, "Licence relay cards"),
            (2, "PG cog host traces"),
            (3, "Display client review"),
            (4, "Nothing, keep it clear"),
        ]),
        ("Post the standup notes now?", "They are drafted; nothing goes out until you say so.", vec![
            (1, "Yes, post them"),
            (2, "Hold until tomorrow"),
        ]),
    ];
    let answers = [2, 3, 1];
    for (i, (q, explain, choices)) in questions.iter().enumerate() {
        r.console
            .ask(Survey {
                id: format!("fri-{}", i + 1),
                question: (*q).into(),
                explain: (*explain).into(),
                choices: choices
                    .iter()
                    .map(|(n, c)| Choice { n: *n, caption: (*c).into() })
                    .collect(),
                index: i as u32 + 1,
                total: 3,
            })
            .unwrap();
        r.advance(200);
        let row = answers[i] - 1;
        r.tap(GUIDE_SIDE.x + 46, GUIDE_SIDE.y + 40 + row * 68 + 34);
        r.advance(200);
        if i == 0 {
            r.snapshot("10-guide-survey");
        }
    }

    // Back to the split; a blocked state for the amber pose.
    r.console.set_layout(Layout::Split).unwrap();
    r.console
        .set_agent_state(AgentState::Blocked("waiting for approval: post standup notes".into()))
        .unwrap();
    r.advance(200);
    r.snapshot("07-blocked");

    eprintln!("done: {} frames of script at {} ms", r.now_ms / FRAME_MS, r.now_ms);
}
