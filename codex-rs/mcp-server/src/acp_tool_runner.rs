//! Asynchronous worker that executes a **ACP** tool-call inside a spawned
//! Tokio task. Separated from `message_processor.rs` to keep that file small
//! and to make future feature-growth easier to manage.

use std::collections::HashMap;
use std::sync::Arc;

use codex_core::Codex;
use codex_core::codex_wrapper::init_codex;
use codex_core::config::Config as CodexConfig;
use mcp_types::CallToolResult;
use mcp_types::ContentBlock;
use mcp_types::RequestId;
use mcp_types::TextContent;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::outgoing_message::OutgoingMessageSender;

pub(crate) const INVALID_PARAMS_ERROR_CODE: i64 = -32602;

pub async fn new_session(
    id: RequestId,
    config: CodexConfig,
    outgoing: Arc<OutgoingMessageSender>,
    session_map: Arc<Mutex<HashMap<Uuid, Arc<Codex>>>>,
) -> Option<Uuid> {
    let (codex, _first_event, _ctrl_c, session_id) = match init_codex(config).await {
        Ok(res) => res,
        Err(e) => {
            let result = CallToolResult {
                content: vec![ContentBlock::TextContent(TextContent {
                    r#type: "text".to_string(),
                    text: format!("Failed to start Codex session: {e}"),
                    annotations: None,
                })],
                is_error: Some(true),
                structured_content: None,
            };
            outgoing.send_response(id.clone(), result.into()).await;
            return None;
        }
    };
    let codex = Arc::new(codex);

    session_map.lock().await.insert(session_id, codex.clone());
    // todo! do something like this but convert to acp::SessionUpdate
    // outgoing.send_event_as_notification(&first_event).await;
    Some(session_id)
}
