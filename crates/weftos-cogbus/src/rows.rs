//! `cogbus list` rows: every topic, or one session's project (ADR-122
//! section 10).

use super::{Broker, Session, TopicRow};

impl Broker {
    /// Every topic (the operator view).
    pub fn snapshot(&self) -> Vec<TopicRow> {
        [self.rows(|_| true), self.stream_rows(&|_| true)].concat()
    }

    /// Topics of the session's project.
    pub fn list_for(&self, s: &Session) -> Vec<TopicRow> {
        let mine = |p: Option<&String>| p == s.project_id.as_ref();
        [self.rows(mine), self.stream_rows(&mine)].concat()
    }

    fn rows(&self, keep: impl Fn(Option<&String>) -> bool) -> Vec<TopicRow> {
        let g = self.lock();
        g.topics
            .iter()
            .filter(|(_, t)| keep(t.project_id.as_ref()))
            .map(|((kind, src), t)| TopicRow {
                topic: format!("{kind}/{src}"),
                schema: t.schema.clone(),
                class: t.class,
                writer: t.writer.clone(),
                project_id: t.project_id.clone(),
                subscribers: g.subs.values().filter(|s| s.matches(kind, src, t)).count()
                    + usize::from(g.sinks.get(kind).is_some_and(|s| s.schema == t.schema)),
                messages: t.messages,
                drops: t.drops,
            })
            .collect()
    }
}
