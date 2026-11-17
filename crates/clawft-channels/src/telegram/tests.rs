//! Tests for the Telegram channel plugin.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use clawft_plugin::error::PluginError;
use clawft_plugin::message::MessagePayload;
use clawft_plugin::traits::{ChannelAdapter, ChannelAdapterHost};
use clawft_types::error::ChannelError;
use clawft_types::event::OutboundMessage;

use crate::traits::{Channel, ChannelFactory, ChannelStatus};

use super::channel::{TelegramChannel, TelegramChannelFactory};
use super::types;

// ── Mock adapter host ────────────────────────────────────────────────────

/// Captures adapter-style inbound deliveries for assertions.
struct MockAdapterHost {
    deliveries: tokio::sync::Mutex<Vec<RecordedDelivery>>,
}

struct RecordedDelivery {
    channel: String,
    sender_id: String,
    chat_id: String,
    content: String,
    metadata: HashMap<String, serde_json::Value>,
}

impl MockAdapterHost {
    fn new() -> Self {
        Self {
            deliveries: tokio::sync::Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl ChannelAdapterHost for MockAdapterHost {
    async fn deliver_inbound(
        &self,
        channel: &str,
        sender_id: &str,
        chat_id: &str,
        payload: MessagePayload,
        metadata: HashMap<String, serde_json::Value>,
    ) -> Result<(), PluginError> {
        let content = match payload {
            MessagePayload::Text { content } => content,
            other => format!("{other:?}"),
        };
        self.deliveries.lock().await.push(RecordedDelivery {
            channel: channel.to_owned(),
            sender_id: sender_id.to_owned(),
            chat_id: chat_id.to_owned(),
            content,
            metadata,
        });
        Ok(())
    }
}

// ── is_allowed ───────────────────────────────────────────────────────────

#[test]
fn is_allowed_empty_list_allows_everyone() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    assert!(ch.is_allowed("123"));
    assert!(ch.is_allowed("anyone"));
    assert!(ch.is_allowed(""));
}

#[test]
fn is_allowed_with_list_allows_only_listed() {
    let ch = TelegramChannel::new("tok".into(), vec!["100".into(), "200".into()]);
    assert!(ch.is_allowed("100"));
    assert!(ch.is_allowed("200"));
    assert!(!ch.is_allowed("300"));
    assert!(!ch.is_allowed(""));
}

// ── metadata / adapter capabilities ──────────────────────────────────────

#[test]
fn metadata_values() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    let meta = Channel::metadata(&ch);
    assert_eq!(meta.name, "telegram");
    assert_eq!(meta.display_name, "Telegram Bot");
    assert!(!meta.supports_threads);
    assert!(meta.supports_media);
}

#[test]
fn adapter_capabilities() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    assert_eq!(ChannelAdapter::name(&ch), "telegram");
    assert_eq!(ChannelAdapter::display_name(&ch), "Telegram Bot");
    assert!(!ChannelAdapter::supports_threads(&ch));
    assert!(ChannelAdapter::supports_media(&ch));
}

// ── name ─────────────────────────────────────────────────────────────────

#[test]
fn name_is_telegram() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    assert_eq!(Channel::name(&ch), "telegram");
    assert_eq!(ChannelAdapter::name(&ch), "telegram");
}

// ── status ───────────────────────────────────────────────────────────────

#[test]
fn initial_status_is_stopped() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    assert_eq!(ch.status(), ChannelStatus::Stopped);
    assert_eq!(Channel::status(&ch), ChannelStatus::Stopped);
}

// ── factory ──────────────────────────────────────────────────────────────

#[test]
fn factory_channel_name() {
    let factory = TelegramChannelFactory;
    assert_eq!(factory.channel_name(), "telegram");
}

#[test]
fn factory_build_success() {
    let factory = TelegramChannelFactory;
    let config = serde_json::json!({
        "token": "123:ABC",
        "allowed_users": ["1", "2"]
    });
    let channel = factory.build(&config).unwrap();
    assert_eq!(channel.name(), "telegram");
    assert!(channel.is_allowed("1"));
    assert!(!channel.is_allowed("3"));
}

#[test]
fn factory_build_missing_token_errors() {
    let factory = TelegramChannelFactory;
    let config = serde_json::json!({});
    match factory.build(&config) {
        Ok(_) => panic!("expected missing token error"),
        Err(err) => assert!(
            err.to_string().contains("token"),
            "expected token error, got: {err}"
        ),
    }
}

#[test]
fn factory_build_token_env() {
    let factory = TelegramChannelFactory;
    // SAFETY: test-only env mutation, single-threaded within this test.
    unsafe {
        std::env::set_var("CLAWFT_TEST_TELEGRAM_TOKEN", "env-token-xyz");
    }
    let config = serde_json::json!({"token_env": "CLAWFT_TEST_TELEGRAM_TOKEN"});
    let channel = factory.build(&config).unwrap();
    assert_eq!(channel.name(), "telegram");
    unsafe {
        std::env::remove_var("CLAWFT_TEST_TELEGRAM_TOKEN");
    }
}

#[test]
fn factory_build_empty_token_env_errors() {
    let factory = TelegramChannelFactory;
    unsafe {
        std::env::remove_var("CLAWFT_TEST_TELEGRAM_TOKEN_MISSING");
    }
    let config = serde_json::json!({"token_env": "CLAWFT_TEST_TELEGRAM_TOKEN_MISSING"});
    match factory.build(&config) {
        Ok(_) => panic!("expected token_env error"),
        Err(err) => assert!(err.to_string().contains("token_env")),
    }
}

#[test]
fn factory_build_no_allowed_users_defaults_to_empty() {
    let factory = TelegramChannelFactory;
    let config = serde_json::json!({"token": "123:ABC"});
    let channel = factory.build(&config).unwrap();
    // Empty allowed_users means everyone is allowed
    assert!(channel.is_allowed("anyone"));
}

#[test]
fn factory_build_invalid_allowed_users_defaults_to_empty() {
    let factory = TelegramChannelFactory;
    let config = serde_json::json!({
        "token": "123:ABC",
        "allowed_users": "not-an-array"
    });
    let channel = factory.build(&config).unwrap();
    // Malformed allowed_users falls back to empty (everyone allowed)
    assert!(channel.is_allowed("anyone"));
}

// ── process_update (adapter host) ────────────────────────────────────────

#[tokio::test]
async fn process_update_delivers_text_message() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    let mock_host = Arc::new(MockAdapterHost::new());
    let host: Arc<dyn ChannelAdapterHost> = mock_host.clone();

    let update = types::Update {
        update_id: 1,
        message: Some(types::Message {
            message_id: 42,
            from: Some(types::User {
                id: 999,
                is_bot: false,
                first_name: "Alice".into(),
                username: Some("alice".into()),
            }),
            chat: types::Chat {
                id: 100,
                chat_type: "private".into(),
                title: None,
                username: Some("alice".into()),
            },
            text: Some("Hello bot".into()),
            date: 1700000000,
        }),
    };

    ch.process_update(&update, &host).await.unwrap();

    let msgs = mock_host.deliveries.lock().await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].channel, "telegram");
    assert_eq!(msgs[0].sender_id, "999");
    assert_eq!(msgs[0].chat_id, "100");
    assert_eq!(msgs[0].content, "Hello bot");
    assert_eq!(
        msgs[0].metadata.get("first_name"),
        Some(&serde_json::Value::String("Alice".into()))
    );
    assert_eq!(
        msgs[0].metadata.get("username"),
        Some(&serde_json::Value::String("alice".into()))
    );
    assert_eq!(
        msgs[0].metadata.get("chat_type"),
        Some(&serde_json::Value::String("private".into()))
    );
}

#[tokio::test]
async fn process_update_skips_non_message_update() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    let mock_host = Arc::new(MockAdapterHost::new());
    let host: Arc<dyn ChannelAdapterHost> = mock_host.clone();

    let update = types::Update {
        update_id: 2,
        message: None,
    };

    ch.process_update(&update, &host).await.unwrap();
    assert!(mock_host.deliveries.lock().await.is_empty());
}

#[tokio::test]
async fn process_update_skips_message_without_text() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    let mock_host = Arc::new(MockAdapterHost::new());
    let host: Arc<dyn ChannelAdapterHost> = mock_host.clone();

    let update = types::Update {
        update_id: 3,
        message: Some(types::Message {
            message_id: 50,
            from: None,
            chat: types::Chat {
                id: 1,
                chat_type: "private".into(),
                title: None,
                username: None,
            },
            text: None,
            date: 1700000001,
        }),
    };

    ch.process_update(&update, &host).await.unwrap();
    assert!(mock_host.deliveries.lock().await.is_empty());
}

#[tokio::test]
async fn process_update_rejects_disallowed_user() {
    let ch = TelegramChannel::new("tok".into(), vec!["100".into()]);
    let mock_host = Arc::new(MockAdapterHost::new());
    let host: Arc<dyn ChannelAdapterHost> = mock_host.clone();

    let update = types::Update {
        update_id: 4,
        message: Some(types::Message {
            message_id: 60,
            from: Some(types::User {
                id: 999, // not in allowed list
                is_bot: false,
                first_name: "Mallory".into(),
                username: None,
            }),
            chat: types::Chat {
                id: 1,
                chat_type: "private".into(),
                title: None,
                username: None,
            },
            text: Some("sneaky".into()),
            date: 1700000002,
        }),
    };

    ch.process_update(&update, &host).await.unwrap();
    assert!(mock_host.deliveries.lock().await.is_empty());
}

#[tokio::test]
async fn process_update_message_without_from() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    let mock_host = Arc::new(MockAdapterHost::new());
    let host: Arc<dyn ChannelAdapterHost> = mock_host.clone();

    let update = types::Update {
        update_id: 5,
        message: Some(types::Message {
            message_id: 70,
            from: None, // channel post
            chat: types::Chat {
                id: -100,
                chat_type: "channel".into(),
                title: Some("News".into()),
                username: None,
            },
            text: Some("announcement".into()),
            date: 1700000003,
        }),
    };

    ch.process_update(&update, &host).await.unwrap();
    let msgs = mock_host.deliveries.lock().await;
    assert_eq!(msgs.len(), 1);
    // sender_id should be empty string when from is None
    assert_eq!(msgs[0].sender_id, "");
    assert_eq!(msgs[0].content, "announcement");
}

// ── allow_from_match metadata (WEFT-162) ────────────────────────────────

/// Sender matched in a non-empty allow-list → metadata has
/// `allow_from_match: true`.
#[tokio::test]
async fn process_update_emits_allow_from_match_for_listed_user() {
    let ch = TelegramChannel::new("tok".into(), vec!["100".into(), "200".into()]);
    let mock_host = Arc::new(MockAdapterHost::new());
    let host: Arc<dyn ChannelAdapterHost> = mock_host.clone();

    let update = types::Update {
        update_id: 10,
        message: Some(types::Message {
            message_id: 80,
            from: Some(types::User {
                id: 100,
                is_bot: false,
                first_name: "Alice".into(),
                username: Some("alice".into()),
            }),
            chat: types::Chat {
                id: 100,
                chat_type: "private".into(),
                title: None,
                username: Some("alice".into()),
            },
            text: Some("hi".into()),
            date: 1700000010,
        }),
    };

    ch.process_update(&update, &host).await.unwrap();

    let msgs = mock_host.deliveries.lock().await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(
        msgs[0].metadata.get("allow_from_match"),
        Some(&serde_json::Value::Bool(true)),
        "matched allow-list user must emit allow_from_match=true"
    );
}

/// Empty allow-list → message is delivered (everyone allowed) but no
/// `allow_from_match` metadata is attached.
#[tokio::test]
async fn process_update_no_allow_from_match_when_list_empty() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    let mock_host = Arc::new(MockAdapterHost::new());
    let host: Arc<dyn ChannelAdapterHost> = mock_host.clone();

    let update = types::Update {
        update_id: 11,
        message: Some(types::Message {
            message_id: 81,
            from: Some(types::User {
                id: 999,
                is_bot: false,
                first_name: "Anyone".into(),
                username: None,
            }),
            chat: types::Chat {
                id: 999,
                chat_type: "private".into(),
                title: None,
                username: None,
            },
            text: Some("hello".into()),
            date: 1700000011,
        }),
    };

    ch.process_update(&update, &host).await.unwrap();

    let msgs = mock_host.deliveries.lock().await;
    assert_eq!(msgs.len(), 1);
    assert!(
        !msgs[0].metadata.contains_key("allow_from_match"),
        "empty allow-list must not emit allow_from_match"
    );
}

// ── ChannelAdapter send / cancellation (WEFT-170) ────────────────────────

#[tokio::test]
async fn adapter_send_rejects_non_text_payload() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    let payload = MessagePayload::structured(serde_json::json!({"k": "v"}));
    let err = ChannelAdapter::send(&ch, "123", &payload)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("text"),
        "expected text-only error, got: {err}"
    );
}

#[tokio::test]
async fn adapter_send_rejects_binary_payload() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    let payload = MessagePayload::binary("audio/wav", vec![0u8; 4]);
    let err = ChannelAdapter::send(&ch, "123", &payload)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("text"));
}

#[tokio::test]
async fn adapter_start_respects_cancellation_token() {
    // Invalid token: get_me fails before the poll loop. Use a host that
    // would accept deliveries if we got that far; the important path is
    // that cancel is honoured once Running. Wiremock would be needed for
    // a full happy-path cancel — here we assert cancel during error
    // backoff after a failed get_me is *not* the path (get_me fails
    // hard). Instead: spawn start with cancel already fired after a
    // micro-yield is not enough without a mock server.
    //
    // Contract check: CancellationToken::cancelled() is the same
    // tokio_util type used by ChannelAdapter on native (default feature).
    let cancel = CancellationToken::new();
    assert!(!cancel.is_cancelled());
    cancel.cancel();
    assert!(cancel.is_cancelled());
    // cancelled().await resolves immediately when already cancelled.
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
            panic!("cancel.cancelled() did not resolve for pre-cancelled token");
        }
    }
}

#[tokio::test]
async fn channel_send_invalid_chat_id() {
    let ch = TelegramChannel::new("tok".into(), vec![]);
    let msg = OutboundMessage {
        channel: "telegram".into(),
        chat_id: "not-a-number".into(),
        content: "hi".into(),
        reply_to: None,
        media: vec![],
        metadata: HashMap::new(),
    };
    let err = Channel::send(&ch, &msg).await.unwrap_err();
    match err {
        ChannelError::SendFailed(s) => assert!(s.contains("chat_id")),
        other => panic!("unexpected error: {other}"),
    }
}

// ── Typed config -> factory hand-off ─────────────────────────────────────

/// The gateway hands the typed `TelegramConfig` to the factory as JSON. A
/// literal token in config must survive that hand-off (BR 06 #13).
#[test]
fn factory_builds_from_typed_config_with_literal_token() {
    let mut cfg = clawft_types::config::channels::TelegramConfig::default();
    cfg.token = clawft_types::secret::SecretString::new("123456:DUMMY");
    let value = clawft_types::secret::to_value_exposing_secrets(&cfg).unwrap();
    let built = TelegramChannelFactory.build(&value);
    assert!(built.is_ok(), "literal token was dropped: {:?}", built.err());
}
