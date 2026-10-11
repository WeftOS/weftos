//! `cos-console-demo`: replay a scripted conversation in the simulator
//! and write key frames as PNGs.
//!
//! ```bash
//! scripts/build.sh cos-console-demo            # writes docs/design/cos-console/*.png
//! cargo run --release --bin cos-console-demo -- --out <dir>
//! ```

use std::path::PathBuf;

use weftos_cos_console::session::SimSession;
use weftos_cos_console::theme::CHAT;
use weftos_cos_console::{AgentState, Console, PageDoc, StatusLine};

const FRAME_MS: u32 = 40;

struct Replay<'a> {
    console: Console<'a, SimSession>,
    now_ms: u32,
    out: PathBuf,
}

impl Replay<'_> {
    /// Advance the clock, ticking animation and events every frame.
    fn advance(&mut self, ms: u32) {
        let frames = ms.div_ceil(FRAME_MS).max(1);
        for _ in 0..frames {
            self.now_ms += FRAME_MS;
            self.console.pump_events().expect("events");
            self.console.tick(self.now_ms).expect("tick");
        }
    }

    fn snapshot(&mut self, name: &str) {
        let now = self.now_ms;
        let s = self.console.session();
        s.render(now).expect("render");
        let path = self.out.join(format!("{name}.png"));
        s.save_png(&path).expect("png");
        eprintln!("wrote {}  (t={} ms)", path.display(), now);
    }

    fn stream(&mut self, text: &str, msg_id: u64, chunk: usize, ms_per_chunk: u32) {
        let chars: Vec<char> = text.chars().collect();
        for piece in chars.chunks(chunk) {
            let s: String = piece.iter().collect();
            self.console.push_agent_text(&s, msg_id).expect("agent text");
            self.advance(ms_per_chunk);
        }
    }

    fn drag_chat(&mut self, dy: i32) {
        let x = CHAT.x + 200;
        let y0 = CHAT.y + 160;
        let s = self.console.session();
        s.swipe((x, y0), (x, y0 + dy), 8);
        self.advance(FRAME_MS);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut out = PathBuf::from("docs/design/cos-console");
    while let Some(a) = args.next() {
        if a == "--out" {
            out = PathBuf::from(args.next().expect("--out <dir>"));
        }
    }
    std::fs::create_dir_all(&out).expect("create output dir");

    let mut sim = SimSession::new(800, 480);
    let console = Console::new(&mut sim).expect("console");
    let mut r = Replay {
        console,
        now_ms: 0,
        out,
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
    r.stream(
        "Two cards: the retry on a stale token, and the log line for a refused checkout.",
        11,
        40,
        0,
    );
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
    r.console.session().push_sensor("ptt.down", 1.0);
    r.console.set_agent_state(AgentState::Listening).unwrap();
    r.advance(700);
    r.snapshot("02-listening");

    // Transcript lands, the agent thinks on a frontier model.
    r.console.session().push_sensor("ptt.up", 0.0);
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
    r.drag_chat(140);
    r.console.push_user_line("Yes, show me the plan.").unwrap();
    r.console.set_agent_state(AgentState::Thinking).unwrap();
    r.advance(300);
    r.snapshot("05-scrolled");

    // Page mode.
    r.console
        .show_page(PageDoc {
            title: "Plan for Friday 10 October".into(),
            paragraphs: vec![
                "09:30  Review the display-client session API (lane A). Unblocks the console merge; \
                 fifteen minutes if the trait matches the contract.".into(),
                "10:00  Licence relay: two cards, both one-file changes. Start a developer lane on \
                 each and let the tester close them.".into(),
                "11:00  PG cog host: read the two restart traces from 03:10. If the cause is the \
                 env file reload, the fix from yesterday covers it; otherwise file a card.".into(),
                "Afternoon is clear. Standup notes and the board summary are drafted and waiting \
                 for your word before anything is posted.".into(),
                "Say \"close\" or swipe right to return to the conversation.".into(),
            ],
        })
        .unwrap();
    r.console.set_agent_state(AgentState::Idle).unwrap();
    r.advance(200);
    r.snapshot("06-page");

    // Swipe right to return; a blocked state for the amber pose.
    let s = r.console.session();
    s.swipe((400, 300), (600, 310), 10);
    r.advance(FRAME_MS);
    r.console
        .set_agent_state(AgentState::Blocked("waiting for approval: post standup notes".into()))
        .unwrap();
    r.advance(200);
    r.snapshot("07-blocked");

    eprintln!("done: {} frames of script at {} ms", r.now_ms / FRAME_MS, r.now_ms);
}
