//! Semantic indexing for conversation turns.
//!
//! Indexes user + assistant message pairs as embeddings so that past
//! conversations can be searched by semantic similarity. Supports
//! both cross-session and single-session search.
//!
//! This module is gated behind the `vector-memory` feature flag.

use std::collections::HashMap;

use crate::embeddings::{Embedder, EmbeddingError};
use crate::vector_store::VectorStore;

/// A single conversation turn to be indexed.
#[derive(Debug, Clone)]
pub struct ConversationTurn {
    /// The session this turn belongs to.
    pub session_id: String,
    /// The turn number within the session (0-based).
    pub turn_id: usize,
    /// The user's message text.
    pub user_message: String,
    /// The assistant's response text.
    pub assistant_message: String,
    /// Unix timestamp (seconds since epoch).
    pub timestamp: u64,
    /// The model that generated the response.
    pub model: String,
}

/// A search result from the session indexer.
#[derive(Debug, Clone)]
pub struct TurnMatch {
    /// The session this turn belongs to.
    pub session_id: String,
    /// The turn number within the session.
    pub turn_id: usize,
    /// The user's message text.
    pub user_message: String,
    /// The assistant's response text.
    pub assistant_message: String,
    /// Cosine similarity score.
    pub score: f32,
    /// Unix timestamp of the turn.
    pub timestamp: u64,
}

/// Errors that can occur during session indexing.
#[non_exhaustive]
#[derive(Debug)]
pub enum IndexError {
    /// An embedding operation failed.
    Embedding(EmbeddingError),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::Embedding(e) => write!(f, "index embedding error: {e}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<EmbeddingError> for IndexError {
    fn from(e: EmbeddingError) -> Self {
        IndexError::Embedding(e)
    }
}

/// Indexes conversation turns for semantic search.
///
/// Each turn is embedded as a combination of the user and assistant messages,
/// then stored in a [`VectorStore`]. Search results can be filtered by
/// session ID.
pub struct SessionIndexer {
    store: VectorStore,
    embedder: Box<dyn Embedder>,
}

impl SessionIndexer {
    /// Create a new session indexer with the given embedder.
    pub async fn new(embedder: Box<dyn Embedder>) -> Self {
        Self {
            store: VectorStore::new(),
            embedder,
        }
    }

    /// Index a conversation turn.
    ///
    /// The embedding is generated from the concatenation of user and assistant
    /// messages: `"User: {user_message}\nAssistant: {assistant_message}"`.
    pub async fn index_turn(&mut self, turn: &ConversationTurn) -> Result<(), IndexError> {
        let combined = format!(
            "User: {}\nAssistant: {}",
            turn.user_message, turn.assistant_message
        );
        let embedding = self.embedder.embed(&combined).await?;

        let id = format!("{}:{}", turn.session_id, turn.turn_id);
        let mut metadata = HashMap::new();
        metadata.insert(
            "session_id".into(),
            serde_json::Value::String(turn.session_id.clone()),
        );
        metadata.insert(
            "turn_id".into(),
            serde_json::Value::Number(turn.turn_id.into()),
        );
        metadata.insert(
            "user_message".into(),
            serde_json::Value::String(turn.user_message.clone()),
        );
        metadata.insert(
            "assistant_message".into(),
            serde_json::Value::String(turn.assistant_message.clone()),
        );
        metadata.insert(
            "model".into(),
            serde_json::Value::String(turn.model.clone()),
        );

        self.store.add_with_timestamp(
            id,
            combined,
            embedding,
            vec![turn.session_id.clone()],
            metadata,
            turn.timestamp,
        );

        Ok(())
    }

    /// Search indexed turns by semantic similarity.
    ///
    /// If `session_id` is `Some`, results are filtered to only that session.
    /// The `top_k` parameter limits the number of results returned.
    pub async fn search_turns(
        &self,
        query: &str,
        session_id: Option<&str>,
        top_k: usize,
    ) -> Result<Vec<TurnMatch>, IndexError> {
        let query_embedding = self.embedder.embed_query(query).await?;

        // Search more results than needed if filtering by session,
        // to ensure we get enough matches after filtering.
        let search_k = if session_id.is_some() {
            top_k * 10
        } else {
            top_k
        };

        let results = self.store.search(&query_embedding, search_k);

        let mut matches: Vec<TurnMatch> = results
            .into_iter()
            .filter(|r| {
                if let Some(sid) = session_id {
                    r.tags.contains(&sid.to_string())
                } else {
                    true
                }
            })
            .filter_map(|r| {
                // Parse the ID to extract session_id and turn_id.
                let parts: Vec<&str> = r.id.splitn(2, ':').collect();
                if parts.len() != 2 {
                    return None;
                }
                let sid = parts[0].to_string();
                let tid: usize = parts[1].parse().ok()?;

                // Extract user and assistant messages from the combined text.
                let (user_msg, assistant_msg) = parse_combined_text(&r.text);

                Some(TurnMatch {
                    session_id: sid,
                    turn_id: tid,
                    user_message: user_msg,
                    assistant_message: assistant_msg,
                    score: r.score,
                    timestamp: r.timestamp,
                })
            })
            .take(top_k)
            .collect();

        // Already sorted by score from VectorStore::search.
        let _ = &mut matches;
        Ok(matches)
    }

    /// Return the number of indexed turns.
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// Return `true` if no turns have been indexed.
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }
}

/// Parse a combined "User: ...\nAssistant: ..." text back into components.
fn parse_combined_text(text: &str) -> (String, String) {
    if let Some(idx) = text.find("\nAssistant: ") {
        let user_part = &text[..idx];
        let assistant_part = &text[idx + "\nAssistant: ".len()..];
        let user_msg = user_part.strip_prefix("User: ").unwrap_or(user_part);
        (user_msg.to_string(), assistant_part.to_string())
    } else {
        (text.to_string(), String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::hash_embedder::HashEmbedder;

    /// PLUMBING embedder: equal text maps to equal vectors and nothing more. Tests
    /// that use it check indexing, filtering and counts and must not assert on
    /// ranking or similarity; recall tests use `real_embedder()`.
    fn make_embedder() -> Box<dyn Embedder> {
        Box::new(HashEmbedder::new(64))
    }

    /// Recall judged with a REAL embedding model (skipped with a printed reason
    /// unless `onnx-embeddings` + `WEFTOS_TEST_EMBED_MODEL_DIR`). No query shares
    /// a content word with the turn it must find.
    #[tokio::test]
    #[cfg_attr(not(feature = "real-embedder-harness"), ignore = "needs --features real-embedder-tests and WEFTOS_TEST_EMBED_MODEL_DIR (nomic-embed-text-v1.5 bundle)")]
    async fn real_embedder_recalls_the_right_turn_for_a_paraphrase() {
        let Some(real) = crate::embeddings::test_support::real_embedder() else { return };
        let mut indexer = SessionIndexer::new(Box::new(real)).await;
        let turns = [
            ("s1", 0, "How do I fix the borrow checker complaining about my lifetimes?", "Annotate the lifetimes or clone the value."),
            ("s1", 1, "Give me a pasta recipe for tonight", "Boil water, add spaghetti, cook ten minutes."),
            ("s2", 0, "My database queries are slow", "Add an index on the filtered column."),
            ("s2", 1, "Plan a weekend trip to the coast", "Book a hotel and check the tide times."),
        ];
        for (sess, id, u, a) in turns {
            indexer.index_turn(&make_turn(sess, id, u, a)).await.unwrap();
        }
        for (query, sess, id) in [
            ("compiler error about references living too long", "s1", 0),
            ("what should I cook for dinner?", "s1", 1),
            ("speed up SQL performance", "s2", 0),
            ("holiday by the sea", "s2", 1),
        ] {
            let hits = indexer.search_turns(query, None, 1).await.unwrap();
            assert_eq!(hits.len(), 1, "{query}");
            assert_eq!((hits[0].session_id.as_str(), hits[0].turn_id), (sess, id), "query {query:?}");
        }
        // Session filter is still honoured with a real embedder.
        let hits = indexer.search_turns("holiday by the sea", Some("s1"), 4).await.unwrap();
        assert!(hits.iter().all(|h| h.session_id == "s1"));
    }

    fn make_turn(
        session_id: &str,
        turn_id: usize,
        user: &str,
        assistant: &str,
    ) -> ConversationTurn {
        ConversationTurn {
            session_id: session_id.into(),
            turn_id,
            user_message: user.into(),
            assistant_message: assistant.into(),
            timestamp: 1000 + turn_id as u64,
            model: "test-model".into(),
        }
    }

    #[tokio::test]
    async fn index_and_search_single_turn_plumbing() {
        let mut indexer = SessionIndexer::new(make_embedder()).await;
        let turn = make_turn(
            "sess1",
            0,
            "What is Rust?",
            "Rust is a systems programming language",
        );
        indexer.index_turn(&turn).await.unwrap();

        let results = indexer
            .search_turns("Rust programming", None, 5)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, "sess1");
        assert_eq!(results[0].turn_id, 0);
        assert_eq!(results[0].user_message, "What is Rust?");
        assert_eq!(
            results[0].assistant_message,
            "Rust is a systems programming language"
        );
    }

    #[tokio::test]
    async fn cross_session_search_plumbing() {
        let mut indexer = SessionIndexer::new(make_embedder()).await;

        indexer
            .index_turn(&make_turn(
                "sess1",
                0,
                "What is Rust?",
                "Rust is a systems programming language",
            ))
            .await
            .unwrap();
        indexer
            .index_turn(&make_turn(
                "sess2",
                0,
                "Tell me about Python",
                "Python is an interpreted language",
            ))
            .await
            .unwrap();

        // Search across all sessions.
        let results = indexer
            .search_turns("programming language", None, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        // Both sessions should be represented.
        let session_ids: Vec<&str> = results.iter().map(|r| r.session_id.as_str()).collect();
        assert!(session_ids.contains(&"sess1"));
        assert!(session_ids.contains(&"sess2"));
    }

    #[tokio::test]
    async fn session_filtered_search() {
        let mut indexer = SessionIndexer::new(make_embedder()).await;

        indexer
            .index_turn(&make_turn(
                "sess1",
                0,
                "What is Rust?",
                "Rust is a language",
            ))
            .await
            .unwrap();
        indexer
            .index_turn(&make_turn(
                "sess2",
                0,
                "What is Rust?",
                "Rust is a language",
            ))
            .await
            .unwrap();

        // Filter to sess1 only.
        let results = indexer
            .search_turns("Rust language", Some("sess1"), 10)
            .await
            .unwrap();
        assert!(!results.is_empty());
        for r in &results {
            assert_eq!(r.session_id, "sess1");
        }
    }

    #[tokio::test]
    async fn multiple_turns_indexed_correctly() {
        let mut indexer = SessionIndexer::new(make_embedder()).await;

        indexer
            .index_turn(&make_turn("sess1", 0, "First question", "First answer"))
            .await
            .unwrap();
        indexer
            .index_turn(&make_turn("sess1", 1, "Second question", "Second answer"))
            .await
            .unwrap();
        indexer
            .index_turn(&make_turn("sess1", 2, "Third question", "Third answer"))
            .await
            .unwrap();

        assert_eq!(indexer.len(), 3);
        assert!(!indexer.is_empty());

        let results = indexer.search_turns("question", None, 10).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn empty_indexer_search() {
        let indexer = SessionIndexer::new(make_embedder()).await;
        let results = indexer.search_turns("anything", None, 5).await.unwrap();
        assert!(results.is_empty());
        assert!(indexer.is_empty());
        assert_eq!(indexer.len(), 0);
    }

    /// PLUMBING: checks the descending-score ordering contract only (any embedder).
    #[tokio::test]
    async fn search_results_sorted_by_descending_score_plumbing() {
        let mut indexer = SessionIndexer::new(make_embedder()).await;

        indexer
            .index_turn(&make_turn(
                "s1",
                0,
                "database optimization query planning",
                "index usage and query optimization",
            ))
            .await
            .unwrap();
        indexer
            .index_turn(&make_turn(
                "s1",
                1,
                "unrelated cooking recipe pasta",
                "boil water add pasta cook 10 minutes",
            ))
            .await
            .unwrap();

        let results = indexer
            .search_turns("database query optimization", None, 2)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results[0].score >= results[1].score,
            "results should be sorted by descending score"
        );
    }

    #[test]
    fn parse_combined_text_normal() {
        let (user, assistant) = parse_combined_text("User: hello\nAssistant: hi there");
        assert_eq!(user, "hello");
        assert_eq!(assistant, "hi there");
    }

    #[test]
    fn parse_combined_text_no_assistant() {
        let (user, assistant) = parse_combined_text("just some text");
        assert_eq!(user, "just some text");
        assert!(assistant.is_empty());
    }

    #[tokio::test]
    async fn top_k_limits_results() {
        let mut indexer = SessionIndexer::new(make_embedder()).await;

        for i in 0..10 {
            indexer
                .index_turn(&make_turn(
                    "s1",
                    i,
                    &format!("question {i}"),
                    &format!("answer {i}"),
                ))
                .await
                .unwrap();
        }

        let results = indexer.search_turns("question", None, 3).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn index_error_display() {
        let err = IndexError::Embedding(EmbeddingError::Internal("test".into()));
        assert!(format!("{err}").contains("test"));
    }
}
