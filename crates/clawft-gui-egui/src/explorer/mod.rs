//! Ontology Explorer — left-tree / right-detail panel for walking the
//! substrate namespace live. Spec: `.planning/explorer/PROJECT-PLAN.md`.
//!
//! Architecture:
//! - [`Explorer`] owns panel state: expansion set, selection, cached
//!   tree children, per-path activity timestamps, and the RPC
//!   subscription lifecycle.
//! - Tree expansion fires `substrate.list { prefix, depth: 1 }` via
//!   [`Live::submit`](crate::live::Live::submit) and caches the result.
//! - Selection fires `substrate.read` for the selected path, with a
//!   slow re-poll so the viewer updates in near-real-time. When
//!   selection changes, the prior poll handle is dropped — the next
//!   tick simply reads the new path.
//!
//! Graceful degradation: if `substrate.list` comes back with
//! "method not allowed" (backend worker hasn't landed the RPC yet),
//! the tree renders just the virtual root and shows a small
//! backend-unavailable hint. No synthetic in-memory children.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use serde_json::Value;

use crate::live::{self, Command, Live, ReplyRx};
use crate::wasm_time::epoch_minus;

pub mod chat;
pub mod control_toggle;
/// ADR-067 conversation graph view (WEFT-630 G1–G5 scaffold).
pub mod conversation_graph;
pub mod terminal;
pub mod tree;
pub mod viewers;
pub mod workshop;

/// A path is considered "live" if its value has changed within this
/// window. Used for the ● activity dot in the tree.
pub const ACTIVITY_WINDOW: Duration = Duration::from_secs(3);

/// Hard cap on the per-path activity map. Long-running webview
/// sessions on a busy substrate can otherwise grow this map without
/// bound — we'd record one entry per ever-published path and never
/// drop them. WEFT-243.
///
/// `Activity` here means a `last_update` `Instant` per path. Eviction
/// is an LRU-on-insert policy: when the map is at cap, drop the
/// least-recently-touched entry before inserting the new one.
pub const ACTIVITY_MAX_ENTRIES: usize = 256;

/// Stale-entry TTL. Any path that hasn't seen a value change in this
/// long is dropped on the next eviction sweep, even if we're below
/// `ACTIVITY_MAX_ENTRIES`. Keeps the map small after a burst of
/// distinct paths goes quiet. The dot is only "active" for
/// [`ACTIVITY_WINDOW`] anyway, so anything older than this TTL is
/// purely overhead. WEFT-243.
pub const ACTIVITY_TTL: Duration = Duration::from_secs(60 * 30);

/// How often to re-list every currently-expanded prefix, so newly
/// appearing paths show up without the user re-clicking.
pub const SLOW_TICK: Duration = Duration::from_millis(1000);

/// How often to re-read the selected path's value so the right pane
/// tracks updates. In place of `substrate.subscribe` streaming frames
/// (which Command::Raw can't model as a single oneshot), we poll the
/// one selected path. Cheap: a single read per tick.
pub const SELECT_POLL: Duration = Duration::from_millis(400);

/// How long the transient "copied" confirmation label stays visible
/// after a Copy Path / Copy Pubkey / Export Snapshot click.
/// WEFT-273. Long enough to read; short enough that it doesn't linger
/// after the user moves on. Matches the cadence of [`SELECT_POLL`] × 4
/// — a single re-poll cycle plus a beat — so the label naturally
/// drops on the next paint that picks up new data.
pub const COPY_TOAST_DURATION: Duration = Duration::from_millis(1500);

/// egui memory key used by viewers to request a tree navigation.
///
/// The Explorer drains this on every `show()` and, when populated,
/// runs the same `on_select` path as a tree click. This keeps viewer
/// code stateless (no `&mut Explorer` plumbing through every viewer
/// signature) while still giving them a return channel for navigation
/// intent. WEFT-272.
pub const NAV_INTENT_KEY: &str = "weft-explorer-nav-intent";

/// Push a "navigate to this substrate path" intent onto the egui
/// memory stash. Called from inside viewers that paint breadcrumb
/// buttons (HealthViewer, SensorViewer). The Explorer drains the
/// intent on its next `show()` and triggers the same selection path
/// a tree click would.
///
/// Idempotent within a frame: a viewer that paints the same breadcrumb
/// twice in one frame and the user clicks both copies still results in
/// one navigation — egui's memory stash overwrites the value, and the
/// Explorer drains it once. WEFT-272.
pub fn request_navigation(ctx: &egui::Context, path: String) {
    let id = egui::Id::new(NAV_INTENT_KEY);
    ctx.data_mut(|d| d.insert_temp(id, path));
}

/// Drain the most recent navigation intent posted by a viewer. Returns
/// the path the Explorer should switch its selection to, or `None`
/// when no viewer requested navigation since the last drain. WEFT-272.
pub fn take_navigation_request(ctx: &egui::Context) -> Option<String> {
    let id = egui::Id::new(NAV_INTENT_KEY);
    ctx.data_mut(|d| d.remove_temp::<String>(id))
}

/// Opaque subscription handle. Today: the one-path we're re-polling.
/// Wrapping this in a type lets us drop it atomically when selection
/// changes — no dangling in-flight reads on the wrong path.
pub struct SubscriptionHandle {
    /// The path this handle is tracking.
    pub path: String,
    /// The in-flight reply channel, if a read is currently outstanding.
    /// Dropped when the handle itself is dropped — back-pressure at
    /// the channel level.
    pub pending: Option<ReplyRx>,
    /// When the last successful read landed, used to space out polls.
    pub last_poll: web_time::Instant,
}

impl SubscriptionHandle {
    fn new(path: String) -> Self {
        Self {
            path,
            pending: None,
            // Epoch-ish: first poll fires immediately. The
            // `epoch_minus` helper handles the WASM cold-load
            // time-origin underflow (otherwise unchecked `Sub` panics
            // with "overflow when subtracting duration from instant").
            // WEFT-247.
            last_poll: epoch_minus(SELECT_POLL * 2),
        }
    }
}

/// Bounded `path → last-activity Instant` map.
///
/// Replaces a raw `HashMap<String, Instant>` so a long-lived webview
/// session can't accumulate entries forever. Eviction policy:
///
/// 1. **Insert / touch** — LRU bookkeeping moves the path to the
///    "most recently touched" end via a parallel queue.
/// 2. **TTL sweep** — entries older than [`ACTIVITY_TTL`] are dropped
///    opportunistically on insert. Activity dots are only "live"
///    within [`ACTIVITY_WINDOW`] (3s) so anything past TTL is pure
///    overhead.
/// 3. **Cap sweep** — if the map is still at [`ACTIVITY_MAX_ENTRIES`]
///    after the TTL sweep, the oldest entry by touch order is
///    evicted before the new one is inserted.
///
/// We don't pull in a full `LruCache` crate for this — the map only
/// exists for an O(1) `is-active` lookup against a hard cap, and the
/// "oldest by touch" find is naturally bounded by
/// `ACTIVITY_MAX_ENTRIES` (256). WEFT-243.
pub struct ActivityMap {
    inner: HashMap<String, web_time::Instant>,
    /// Touch order — front = least recently touched, back = most
    /// recent. Each path appears at most once; on touch we push the
    /// fresh copy and the next sweep drops the stale earlier copy.
    /// VecDeque rather than `LinkedList` because the cap is small
    /// and contiguous storage wins on cache.
    order: VecDeque<String>,
}

impl ActivityMap {
    /// Create an empty bounded activity map.
    pub fn new() -> Self {
        Self {
            inner: HashMap::with_capacity(ACTIVITY_MAX_ENTRIES.min(64)),
            order: VecDeque::with_capacity(ACTIVITY_MAX_ENTRIES.min(64)),
        }
    }

    /// Record a fresh activity instant for `path`. Evicts a stale or
    /// LRU entry if the map is at cap.
    pub fn insert(&mut self, path: String, when: web_time::Instant) {
        // (1) TTL sweep — drop everything older than TTL.
        self.evict_stale(when);

        // (2) If we're updating an existing entry, drop its prior
        //     position from `order` before re-pushing at the back.
        //     Linear scan is fine: cap is 256 and this is per-update
        //     not per-frame.
        if self.inner.contains_key(&path) {
            if let Some(pos) = self.order.iter().position(|p| p == &path) {
                self.order.remove(pos);
            }
        } else if self.inner.len() >= ACTIVITY_MAX_ENTRIES {
            // (3) Cap sweep — evict LRU.
            if let Some(oldest) = self.order.pop_front() {
                self.inner.remove(&oldest);
            }
        }
        self.inner.insert(path.clone(), when);
        self.order.push_back(path);
    }

    /// Look up the last activity instant for a path, if any.
    pub fn get(&self, path: &str) -> Option<&web_time::Instant> {
        self.inner.get(path)
    }

    /// Number of tracked paths. Useful for tests and metrics.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True iff the map currently tracks no paths.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Drop entries older than [`ACTIVITY_TTL`]. Called on every
    /// insert and exposed for tests.
    fn evict_stale(&mut self, now: web_time::Instant) {
        // Walk from the front (oldest by touch order). Since `order`
        // is touch-ordered, once we hit a non-stale entry the rest
        // are also non-stale. But the wall-clock time of an entry
        // *can* be older than the head of `order` if a series of
        // inserts went out of monotonic order on wasm — guard
        // defensively by checking each entry's recorded instant
        // rather than assuming touch order ≡ time order.
        while let Some(front) = self.order.front() {
            match self.inner.get(front) {
                Some(t) if now.duration_since(*t) > ACTIVITY_TTL => {
                    let key = self.order.pop_front().unwrap();
                    self.inner.remove(&key);
                }
                Some(_) => break,
                None => {
                    // Defensive: order had a key with no entry.
                    // Drop and continue.
                    self.order.pop_front();
                }
            }
        }
    }
}

impl Default for ActivityMap {
    fn default() -> Self {
        Self::new()
    }
}

/// One child entry in the cached tree. Mirrors the `substrate.list`
/// response row shape.
#[derive(Clone, Debug)]
pub struct TreeNode {
    pub path: String,
    pub has_value: bool,
    pub child_count: u64,
}

/// Pending list request we've fired but haven't yet received a reply for.
struct PendingList {
    prefix: String,
    /// `None` until [`Explorer::tick`] dispatches the RPC; once fired,
    /// `Some(rx)` drains through [`Explorer::drain_replies`]. Splitting
    /// into two states (vs. probing a closed placeholder rx) keeps us
    /// from accidentally consuming a real reply in the dispatch probe.
    rx: Option<ReplyRx>,
}

/// Panel state. One instance lives on the [`Desktop`](crate::shell::desktop::Desktop)
/// and is shown when the user toggles the Explorer tray chip.
pub struct Explorer {
    /// Set of prefixes the user has expanded in the left tree.
    pub expanded: HashSet<String>,
    /// Currently selected substrate path (right pane focus).
    pub selected: Option<String>,
    /// Cache of `substrate.list` results, keyed by the prefix requested.
    /// An entry of `Some(vec)` means we have children; `None` means we
    /// haven't fetched yet (renders as "loading…").
    pub tree_children: HashMap<String, Vec<TreeNode>>,
    /// Active subscription for the currently-selected path. `None` when
    /// nothing is selected. Swapped (NOT mutated) when selection
    /// changes, so the old handle's [`ReplyRx`] drops cleanly.
    pub subscription_handle: Option<SubscriptionHandle>,
    /// Last-activity instant per path, used to drive the ● activity
    /// dot. Bounded by [`ACTIVITY_MAX_ENTRIES`] / [`ACTIVITY_TTL`] to
    /// stop a long-running webview session from leaking one entry per
    /// ever-published substrate path. WEFT-243.
    pub activity: ActivityMap,
    /// Most recent value retrieved for the selected path, used by the
    /// right-hand detail pane. Replaced on every successful read.
    pub selected_value: Option<Value>,
    /// Populated when the backend's `substrate.list` isn't available
    /// (method not allowed, connection error, …). Shown as a small
    /// header chip in the tree. Cleared on the first successful list.
    pub backend_hint: Option<String>,

    /// In-flight list requests waiting for a reply.
    pending_lists: Vec<PendingList>,
    /// Last time we re-polled every expanded prefix for changes.
    last_slow_tick: web_time::Instant,
    /// Live Workshop-composition state, reused across frames so
    /// per-panel subscriptions don't restart every paint. The view
    /// is keyed to the currently-selected Workshop path; changing
    /// selection clears it via [`Explorer::on_select`] so the new
    /// Workshop (if any) starts from a clean slate.
    workshop_view: workshop::WorkshopView,
    /// Live chat-window state (WEFT-254 multi-conversation panel).
    /// Holds one or more in-memory sessions (history + draft + in-flight
    /// reply) across frames so a paint doesn't lose a pending
    /// `agent.chat_stream` reply. Cleared on selection change for the
    /// same reason as `workshop_view` — a fresh chat sentinel selection
    /// should start with an empty session list rather than inherit the
    /// previous panel's turns.
    chat_view: chat::ChatPanel,
    /// Live multi-tab terminal panel state (WEFT-263). Reset (and all
    /// daemon-side sessions closed) when selection moves off the
    /// terminal sentinel — see [`Explorer::on_select`] and
    /// [`Explorer::close`]. `HashMap<SessionKey, Terminal>` lives
    /// inside [`terminal::TerminalPanel`]; each tab owns its own PTY.
    terminal_view: terminal::TerminalPanel,
    /// Most recent copy-action confirmation, paired with the instant at
    /// which it fired. Rendered as a small label next to the action row
    /// for [`COPY_TOAST_DURATION`]; cleared on the first paint after
    /// it expires. WEFT-273.
    last_copy_msg: Option<(web_time::Instant, String)>,
    /// Tree filter state: the chip row above the substrate tree narrows
    /// what rows are rendered. Filters persist within a session
    /// (in-memory only) so a user who picks "active only" while
    /// triaging doesn't lose the choice across panel toggles.
    /// WEFT-270.
    pub tree_filters: tree::TreeFilters,
}

impl Default for Explorer {
    fn default() -> Self {
        let mut expanded = HashSet::new();
        // Seed with the substrate root expanded so the slow tick
        // re-fetches the top-level node list. `tree::paint` also
        // re-asserts this every frame; this seed just avoids one
        // wasted frame at startup before the first paint.
        expanded.insert(crate::explorer::tree::ROOT_PREFIX.to_string());
        Self {
            expanded,
            selected: None,
            tree_children: HashMap::new(),
            subscription_handle: None,
            activity: ActivityMap::new(),
            selected_value: None,
            backend_hint: None,
            pending_lists: Vec::new(),
            // `now - slow tick` so the first update() fires the slow
            // refresh immediately. `epoch_minus` handles the WASM
            // cold-load time-origin underflow (see WEFT-247); the
            // fallback means the first slow tick fires a `SLOW_TICK`
            // later instead of immediately.
            last_slow_tick: epoch_minus(SLOW_TICK * 2),
            workshop_view: workshop::WorkshopView::default(),
            chat_view: chat::ChatPanel::default(),
            terminal_view: terminal::TerminalPanel::default(),
            last_copy_msg: None,
            tree_filters: tree::TreeFilters::default(),
        }
    }
}

impl Explorer {
    /// Paint the two-pane Explorer layout inside `ui`. `live` is the
    /// RPC transport handle used to fire substrate.list / substrate.read.
    pub fn show(&mut self, ui: &mut egui::Ui, live: &Arc<Live>) {
        // 0. Drain any navigation intent posted by a viewer in the
        //    previous frame (or earlier this frame, in the rare case
        //    of two paints per frame). Treated identically to a tree
        //    click — runs through `on_select` so the subscription
        //    handle swaps cleanly. WEFT-272.
        if let Some(target) = take_navigation_request(ui.ctx()) {
            self.on_select(target, live);
        }

        // 1. Tick: fire any due list re-polls and selected-path reads.
        self.tick(live);

        // 2. Drain any completed RPC replies so the UI reflects fresh
        //    data before we paint.
        self.drain_replies();

        // 3. Layout: left SidePanel for tree, CentralPanel for detail.
        //    ~40/60 split per the spec.
        let total_w = ui.available_width();
        let tree_w = (total_w * 0.4).clamp(220.0, 480.0);

        egui::Panel::left("weft_explorer_tree")
            .resizable(true)
            .default_size(tree_w)
            .size_range(180.0..=640.0)
            .show_inside(ui, |ui| {
                ui.heading("Substrate");
                ui.separator();
                if let Some(newly) = tree::paint(ui, self) {
                    self.on_select(newly, live);
                }
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.paint_detail(ui, live);
        });
    }

    /// Called by the layout code after a new path is selected. Drops
    /// the previous subscription and starts tracking the new one.
    fn on_select(&mut self, path: String, live: &Arc<Live>) {
        // Dropping the existing `SubscriptionHandle` closes its pending
        // ReplyRx — no leaked in-flight reads on the old path.
        self.subscription_handle = Some(SubscriptionHandle::new(path.clone()));
        self.selected = Some(path);
        self.selected_value = None;
        // Reset Workshop state when selection moves. If the new path
        // is also a Workshop, its subscriptions rebuild on the next
        // paint; if not, the old per-panel polls stop immediately.
        self.workshop_view = workshop::WorkshopView::default();
        // Reset chat state too. A pending `llm.prompt` reply against
        // the previous selection is dropped (the ReplyRx falls out of
        // scope, the daemon's response is ignored). In-memory only —
        // there is no on-disk conversation to restore.
        self.chat_view = chat::ChatPanel::default();
        // Tear down every multi-tab terminal session if we were on the
        // terminal sentinel and are navigating away. If we're
        // navigating to a (different) terminal sentinel the next paint
        // re-spawns; if we're navigating to anything else the
        // daemon-side child shells die promptly rather than waiting
        // for the daemon to shut down.
        self.terminal_view.close(live);
        self.terminal_view = terminal::TerminalPanel::default();
    }

    /// Clear the subscription handle. Called by the mount site when
    /// the Explorer panel is closed so no poll is left running against
    /// a hidden panel.
    pub fn close(&mut self, live: &Arc<Live>) {
        self.subscription_handle = None;
        self.selected_value = None;
        // Drop all Workshop per-panel subscriptions so hidden panels
        // stop polling.
        self.workshop_view = workshop::WorkshopView::default();
        // Drop chat state on close — same reason as Workshop: hidden
        // panels shouldn't keep an in-flight RPC against the daemon.
        self.chat_view = chat::ChatPanel::default();
        // Same teardown as `on_select`: kill every daemon-side shell
        // when the Explorer is hidden so a forgotten multi-tab
        // terminal panel doesn't leak sessions.
        self.terminal_view.close(live);
        self.terminal_view = terminal::TerminalPanel::default();
        // Keep expanded + tree_children so reopening is instant.
    }

    /// Enqueue a `substrate.list` request. Public for tree::paint.
    pub fn queue_list(&mut self, prefix: String) {
        // Using `Live::submit` would require the &Arc<Live>, which
        // paint-time borrow rules forbid here. Instead we stash the
        // prefix and let `tick` dispatch it on the next frame. This
        // also naturally coalesces duplicate requests within a frame.
        if !self.pending_lists.iter().any(|p| p.prefix == prefix) {
            self.pending_lists.push(PendingList { prefix, rx: None });
        }
    }

    /// Per-frame tick: fire any newly-queued list requests, schedule
    /// slow-tick re-polls, and re-read the selected path if its poll
    /// interval has elapsed.
    fn tick(&mut self, live: &Arc<Live>) {
        // (a) Dispatch list requests that don't have an rx yet.
        for p in self.pending_lists.iter_mut() {
            if p.rx.is_none() {
                let (tx, rx) = live::reply_channel();
                p.rx = Some(rx);
                live.submit(Command::Raw {
                    method: "substrate.list".into(),
                    params: serde_json::json!({
                        "prefix": p.prefix,
                        "depth": 1,
                    }),
                    reply: Some(tx),
                });
            }
        }

        // (b) Slow tick: re-list every currently-expanded prefix. This
        //     is how newly appearing paths show up without user action.
        if self.last_slow_tick.elapsed() >= SLOW_TICK {
            self.last_slow_tick = web_time::Instant::now();
            let prefixes: Vec<String> = self.expanded.iter().cloned().collect();
            for prefix in prefixes {
                self.queue_list(prefix);
            }
        }

        // (c) Selected-path re-poll. `substrate.read` oneshots are the
        //     simplest thing that tracks a single value — streaming
        //     subscribe would demand new plumbing through Live.
        if let Some(handle) = self.subscription_handle.as_mut()
            && handle.pending.is_none()
            && handle.last_poll.elapsed() >= SELECT_POLL
        {
            let (tx, rx) = live::reply_channel();
            handle.pending = Some(rx);
            handle.last_poll = web_time::Instant::now();
            live.submit(Command::Raw {
                method: "substrate.read".into(),
                params: serde_json::json!({ "path": handle.path }),
                reply: Some(tx),
            });
        }
    }

    /// Drain completed RPC replies. Updates caches + activity.
    fn drain_replies(&mut self) {
        // (a) Tree listings.
        let mut still = Vec::with_capacity(self.pending_lists.len());
        let pending = std::mem::take(&mut self.pending_lists);
        for mut p in pending {
            // Entries without an rx yet haven't been dispatched — leave
            // them for the next tick() pass to fire.
            let Some(rx) = p.rx.as_mut() else {
                still.push(p);
                continue;
            };
            match live::try_recv_reply(rx) {
                live::TryReply::Done(Ok(value)) => {
                    let kids = tree::parse_list_response(&value);
                    self.tree_children.insert(p.prefix.clone(), kids);
                    // First success clears any prior backend hint.
                    self.backend_hint = None;
                }
                live::TryReply::Done(Err(err)) => {
                    // Only stash the hint if we've never seen success
                    // yet — if we once had children for this prefix,
                    // a transient error shouldn't flip the UI into
                    // "backend unavailable" mode.
                    if self.tree_children.is_empty() {
                        self.backend_hint =
                            Some(format!("substrate.list not yet available: {err}"));
                    }
                    // Record an empty cache so the row shows `(empty)`
                    // instead of an infinite "loading…".
                    self.tree_children.entry(p.prefix.clone()).or_default();
                }
                live::TryReply::Empty => {
                    // Still in flight — keep it.
                    still.push(p);
                }
                live::TryReply::Closed => {
                    // The transport dropped its end without replying —
                    // treat like a transient error: record an empty
                    // cache and let the slow tick re-list.
                    self.tree_children.entry(p.prefix.clone()).or_default();
                }
            }
        }
        self.pending_lists = still;

        // (b) Selected-path read.
        if let Some(handle) = self.subscription_handle.as_mut()
            && let Some(rx) = handle.pending.as_mut()
        {
            match live::try_recv_reply(rx) {
                live::TryReply::Done(Ok(value)) => {
                    handle.pending = None;
                    // substrate.read shape: { value, tick, sensitivity }
                    let new_value = value.get("value").cloned().unwrap_or(Value::Null);
                    let changed = self.selected_value.as_ref() != Some(&new_value);
                    if changed {
                        self.activity
                            .insert(handle.path.clone(), web_time::Instant::now());
                    }
                    self.selected_value = Some(new_value);
                }
                live::TryReply::Done(Err(_)) | live::TryReply::Closed => {
                    handle.pending = None;
                    // No value update; tick() will retry on next cycle.
                }
                live::TryReply::Empty => { /* still in flight */ }
            }
        }
    }

    /// Right-pane detail rendering. Picks a viewer via [`viewers::dispatch`]
    /// and paints it.
    ///
    /// Workshop shortcut: when the selected value shape-matches a
    /// Workshop (see [`workshop::matches`]), the detail pane renders
    /// it as a composition — nested panels with their own substrate
    /// subscriptions — instead of falling through to the generic
    /// viewer. This is what makes config-driven hot-reload work:
    /// publishing a new Workshop JSON replaces `selected_value`, which
    /// the Workshop renderer re-parses on the next frame.
    fn paint_detail(&mut self, ui: &mut egui::Ui, live: &Arc<Live>) {
        let Some(path) = self.selected.clone() else {
            ui.vertical_centered(|ui| {
                ui.add_space(64.0);
                ui.label(
                    egui::RichText::new("Select a path from the tree")
                        .italics()
                        .color(crate::theming::palette::TEXT_DISCLOSURE),
                );
            });
            return;
        };
        // Copy-actions row sits above any badge / viewer so it's
        // discoverable without scrolling. Painted whether or not a
        // value has landed yet — copying just the path while a read is
        // in flight is legitimate. WEFT-273.
        //
        // Clone selected_value into a local so paint_copy_actions can
        // take &mut self without aliasing — self.selected_value is read
        // again below to drive viewer dispatch.
        let value_for_actions = self.selected_value.clone();
        self.paint_copy_actions(ui, &path, value_for_actions.as_ref());
        match self.selected_value.clone() {
            Some(v) => {
                // Object Type badge: shape-infer a type and render a
                // small label above the viewer. When no type is
                // inferred we render nothing — viewer dispatch stays
                // exactly as Phase 1 shipped it.
                if let Some(inferred) = crate::ontology::infer(&v) {
                    paint_object_type_badge(ui, inferred);
                }
                // Terminal sentinel dispatch: when the selected
                // value is the terminal surface sentinel
                // (`{ "kind": "terminal" }` published by the daemon at
                // `substrate/<daemon-node>/ui/terminal`), render the
                // PTY-backed terminal panel. Shape-match precedes
                // Workshop and the generic viewer cascade — see
                // [`terminal::matches`].
                if terminal::matches(&v) > 0 {
                    self.terminal_view.paint(ui, live);
                    return;
                }
                // Workshop dispatch: if the value shape-matches a
                // Workshop, render the composition primitive instead
                // of the generic viewer cascade. Shape-only; no path
                // whitelist. ADOPTION §8 Step 3: "The shape of a value
                // at a substrate path determines which Object Type it
                // instantiates, which Viewers render it…"
                if workshop::matches(&v) > 0 {
                    self.workshop_view.paint(ui, &v, live);
                    return;
                }
                // Chat sentinel: dispatched ahead of control_toggle so
                // a `{kind:"chat"}` value lands in the chat panel even
                // if some future control intent shape brushes against
                // it. Needs the Live RPC handle to fire `llm.prompt`
                // and the persistent `chat_view` state for history +
                // in-flight reply tracking.
                if chat::matches(&v) > 0 {
                    chat::paint(ui, &path, &v, &mut self.chat_view, live);
                    return;
                }
                // Control-intent toggle: shape-match precedes the
                // generic viewer cascade. Lives outside the
                // SubstrateViewer trait because it needs the Live
                // RPC handle to fire `control.set_enabled` on click.
                if control_toggle::matches(&v) > 0 {
                    control_toggle::paint(ui, &path, &v, live);
                    return;
                }
                viewers::dispatch(ui, &path, &v);
            }
            None => {
                ui.horizontal(|ui| {
                    ui.monospace(&path);
                    ui.separator();
                    ui.label(
                        egui::RichText::new("reading…")
                            .italics()
                            .color(crate::theming::palette::TEXT_DISCLOSURE),
                    );
                });
            }
        }
    }

    /// Render the copy-actions chip row above the detail viewer.
    ///
    /// Emits three potential buttons:
    /// 1. **Copy Path** — always visible; copies the substrate path the
    ///    detail pane is showing. Useful for cross-referencing in chat,
    ///    docs, or scripts.
    /// 2. **Copy Pubkey** — visible iff the current value carries an
    ///    obvious pubkey-shaped field at the top level (see
    ///    [`extract_pubkey_like`]). Copies the value, not the key.
    /// 3. **Export Snapshot** — visible iff a value has landed; copies
    ///    a pretty-printed JSON snapshot of the current detail-pane
    ///    value. The "snapshot" framing matches the language the audit
    ///    used — same effect as the JSON-fallback's `copy` button, but
    ///    surfaces it before the viewer dispatch decides what to render.
    ///
    /// A transient confirmation label appears next to the buttons for
    /// [`COPY_TOAST_DURATION`] after a click, then drops on the next
    /// paint. WEFT-273.
    fn paint_copy_actions(&mut self, ui: &mut egui::Ui, path: &str, value: Option<&Value>) {
        // Drop a stale toast before painting — keeps this row visually
        // quiet when no recent copy has happened.
        if let Some((when, _)) = self.last_copy_msg
            && when.elapsed() >= COPY_TOAST_DURATION
        {
            self.last_copy_msg = None;
        }

        ui.horizontal(|ui| {
            if ui
                .small_button("Copy Path")
                .on_hover_text("Copy the substrate path to the clipboard")
                .clicked()
            {
                ui.ctx().copy_text(path.to_string());
                self.last_copy_msg = Some((web_time::Instant::now(), "path copied".to_string()));
            }

            if let Some(v) = value
                && let Some((field, key)) = extract_pubkey_like(v)
                && ui
                    .small_button("Copy Pubkey")
                    .on_hover_text(format!("Copy `{field}` value to the clipboard"))
                    .clicked()
            {
                ui.ctx().copy_text(key.clone());
                self.last_copy_msg = Some((web_time::Instant::now(), format!("{field} copied")));
            }

            if let Some(v) = value
                && ui
                    .small_button("Export Snapshot")
                    .on_hover_text("Copy a JSON snapshot of the current value")
                    .clicked()
            {
                let snapshot = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
                ui.ctx().copy_text(snapshot);
                self.last_copy_msg =
                    Some((web_time::Instant::now(), "snapshot copied".to_string()));
            }

            if let Some((_, ref msg)) = self.last_copy_msg {
                ui.label(
                    egui::RichText::new(msg)
                        .small()
                        .italics()
                        .color(crate::theming::palette::TERM_OK_HINT),
                );
            }
        });
        ui.add_space(2.0);
    }
}

/// Best-effort pubkey extractor for the Copy Pubkey affordance.
///
/// Looks for a small set of obvious top-level string fields on the
/// value's root object. Order matters — the first match wins, so a
/// value that has both `pubkey` and `node_id` reports `pubkey`.
///
/// Returns `(field-name, value)` so the toast can name *what* was
/// copied. The field name is `'static` (one of the literal keys we
/// probe) so callers don't have to worry about lifetimes.
///
/// Deliberately narrow: pubkeys live at well-known shapes (Mesh node
/// records, identity bundles); deeper traversal would copy random
/// strings from arbitrary substrate values.
pub(super) fn extract_pubkey_like(value: &Value) -> Option<(&'static str, String)> {
    let obj = value.as_object()?;
    // Order = priority. `pubkey` is the canonical identity-system
    // field; `peer_id` / `node_id` / `device_id` cover Mesh + identity
    // bundle shapes that appear in the substrate today.
    for field in ["pubkey", "peer_id", "node_id", "device_id"] {
        if let Some(s) = obj.get(field).and_then(Value::as_str)
            && !s.is_empty()
        {
            return Some((field, s.to_string()));
        }
    }
    None
}

/// Render the Object Type badge for an inferred type.
///
/// Visual: a muted-blue pill with `[DisplayName]` above whatever the
/// viewer registry paints. Kept deliberately small + passive — the
/// badge is informational, not interactive. If/when property panels
/// or Action affordances arrive, they attach here.
fn paint_object_type_badge(ui: &mut egui::Ui, inferred: crate::ontology::InferredType) {
    ui.horizontal(|ui| {
        let label = egui::RichText::new(format!("[{}]", inferred.display))
            .monospace()
            .small()
            .color(crate::theming::palette::ACCENT_STEEL);
        ui.label(label);
    });
    ui.add_space(2.0);
}

#[cfg(test)]
mod activity_map_tests {
    //! WEFT-243: bounded activity HashMap. Confirms eviction kicks in
    //! at the cap so a long-lived session doesn't accumulate one
    //! entry per ever-published substrate path.

    use super::*;

    #[test]
    fn insert_under_cap_keeps_all_entries() {
        let mut m = ActivityMap::new();
        let now = web_time::Instant::now();
        for i in 0..16 {
            m.insert(format!("/path/{i}"), now);
        }
        assert_eq!(m.len(), 16);
        assert!(m.get("/path/0").is_some());
        assert!(m.get("/path/15").is_some());
    }

    #[test]
    fn insert_at_cap_evicts_lru() {
        let mut m = ActivityMap::new();
        let now = web_time::Instant::now();
        // Fill to cap.
        for i in 0..ACTIVITY_MAX_ENTRIES {
            m.insert(format!("/path/{i}"), now);
        }
        assert_eq!(m.len(), ACTIVITY_MAX_ENTRIES);

        // Push one more. The first inserted (LRU) must drop.
        m.insert("/path/new".into(), now);
        assert_eq!(m.len(), ACTIVITY_MAX_ENTRIES);
        assert!(
            m.get("/path/0").is_none(),
            "LRU entry should have been evicted"
        );
        assert!(m.get("/path/new").is_some());
        // Tail entry must survive.
        assert!(
            m.get(&format!("/path/{}", ACTIVITY_MAX_ENTRIES - 1))
                .is_some()
        );
    }

    #[test]
    fn touching_existing_path_refreshes_lru_position() {
        let mut m = ActivityMap::new();
        let now = web_time::Instant::now();
        for i in 0..ACTIVITY_MAX_ENTRIES {
            m.insert(format!("/path/{i}"), now);
        }
        // Touch /path/0 — should move it to the back of the LRU
        // queue, so the next insert evicts /path/1 instead.
        m.insert("/path/0".into(), now);
        m.insert("/path/new".into(), now);
        assert!(m.get("/path/0").is_some(), "touched entry should survive");
        assert!(
            m.get("/path/1").is_none(),
            "next-oldest should have been evicted"
        );
    }

    #[test]
    fn ttl_sweep_drops_stale_entries_on_insert() {
        let mut m = ActivityMap::new();
        // Fake a "stale" instant: now - (TTL + slack). On wasm this
        // could underflow, so use the safe `epoch_minus` helper.
        let stale = epoch_minus(ACTIVITY_TTL + Duration::from_secs(5));
        for i in 0..4 {
            m.insert(format!("/old/{i}"), stale);
        }
        // Now drop a fresh entry — TTL sweep should evict the
        // stale ones.
        let fresh = web_time::Instant::now();
        m.insert("/fresh".into(), fresh);
        assert!(m.get("/fresh").is_some());
        for i in 0..4 {
            assert!(
                m.get(&format!("/old/{i}")).is_none(),
                "stale entry /old/{i} should have been evicted"
            );
        }
    }

    #[test]
    fn empty_after_construct() {
        let m = ActivityMap::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }
}

#[cfg(test)]
mod copy_actions_tests {
    //! WEFT-273: pubkey-shaped field detection for the Copy Pubkey
    //! action. The field-priority order is observable behaviour
    //! (toasts name the field) so it's pinned here.

    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_canonical_pubkey_field() {
        let v = json!({ "pubkey": "abc123", "extra": "ignored" });
        let (field, key) = extract_pubkey_like(&v).expect("pubkey-shaped");
        assert_eq!(field, "pubkey");
        assert_eq!(key, "abc123");
    }

    #[test]
    fn falls_back_to_peer_id() {
        let v = json!({ "peer_id": "12D3KooW..." });
        let (field, key) = extract_pubkey_like(&v).expect("peer_id-shaped");
        assert_eq!(field, "peer_id");
        assert_eq!(key, "12D3KooW...");
    }

    #[test]
    fn falls_back_to_node_id() {
        let v = json!({ "node_id": "node-42" });
        let (field, _) = extract_pubkey_like(&v).expect("node_id-shaped");
        assert_eq!(field, "node_id");
    }

    #[test]
    fn falls_back_to_device_id() {
        let v = json!({ "device_id": "dev-7" });
        let (field, _) = extract_pubkey_like(&v).expect("device_id-shaped");
        assert_eq!(field, "device_id");
    }

    #[test]
    fn priority_pubkey_over_peer_id() {
        let v = json!({ "peer_id": "second", "pubkey": "first" });
        let (field, key) = extract_pubkey_like(&v).expect("priority pick");
        assert_eq!(field, "pubkey");
        assert_eq!(key, "first");
    }

    #[test]
    fn rejects_empty_string_field() {
        // An empty pubkey isn't useful to copy and would surface a
        // misleading affordance; treat as absent.
        let v = json!({ "pubkey": "" });
        assert!(extract_pubkey_like(&v).is_none());
    }

    #[test]
    fn rejects_non_string_field() {
        let v = json!({ "pubkey": 42 });
        assert!(extract_pubkey_like(&v).is_none());
    }

    #[test]
    fn rejects_non_object_value() {
        assert!(extract_pubkey_like(&json!([])).is_none());
        assert!(extract_pubkey_like(&json!("just-a-string")).is_none());
        assert!(extract_pubkey_like(&Value::Null).is_none());
    }

    #[test]
    fn rejects_object_without_known_fields() {
        let v = json!({ "name": "foo", "value": 7 });
        assert!(extract_pubkey_like(&v).is_none());
    }
}
