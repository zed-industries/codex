//! Asynchronous worker that executes a **ACP** tool-call inside a spawned
//! Tokio task. Separated from `message_processor.rs` to keep that file small
//! and to make future feature-growth easier to manage.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::ToolCallUpdateFields;
use anyhow::Result;
use codex_core::Codex;
use codex_core::codex_wrapper::CodexConversation;
use codex_core::codex_wrapper::init_codex;
use codex_core::config::Config as CodexConfig;
use codex_core::protocol::EventMsg;
use codex_core::protocol::InputItem;
use codex_core::protocol::Op;
use mcp_types::CallToolResult;
use mcp_types::ContentBlock;
use mcp_types::RequestId;
use mcp_types::TextContent;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::outgoing_message::OutgoingMessageSender;

pub async fn new_session(
    id: RequestId,
    config: CodexConfig,
    outgoing: Arc<OutgoingMessageSender>,
    session_map: Arc<Mutex<HashMap<Uuid, Arc<Codex>>>>,
) -> Option<Uuid> {
    let CodexConversation {
        codex, session_id, ..
    } = match init_codex(config).await {
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

    Some(session_id)
}

pub async fn prompt(
    acp_session_id: acp::SessionId,
    codex: Arc<Codex>,
    prompt: Vec<acp::ContentBlock>,
    outgoing: Arc<OutgoingMessageSender>,
) -> Result<()> {
    let _submission_id = codex
        .submit(Op::UserInput {
            items: prompt
                .into_iter()
                .filter_map(acp_content_block_to_item)
                .collect(),
        })
        .await?;

    // Stream events until the task needs to pause for user interaction or
    // completes.
    loop {
        let event = codex.next_event().await?;

        let acp_update = match event.msg {
            EventMsg::Error(error_event) => {
                anyhow::bail!("Error: {}", error_event.message);
            }
            EventMsg::AgentMessage(_) | EventMsg::AgentReasoning(_) => None,
            EventMsg::AgentMessageDelta(event) => Some(acp::SessionUpdate::AgentMessageChunk {
                content: event.delta.into(),
            }),
            EventMsg::AgentReasoningDelta(event) => Some(acp::SessionUpdate::AgentThoughtChunk {
                content: event.delta.into(),
            }),
            EventMsg::McpToolCallBegin(event) => {
                Some(acp::SessionUpdate::ToolCall(acp::ToolCall {
                    id: acp::ToolCallId(event.call_id.into()),
                    label: format!("{}: {}", event.server, event.tool),
                    kind: acp::ToolKind::Other,
                    status: acp::ToolCallStatus::InProgress,
                    content: vec![],
                    locations: vec![],
                    raw_input: event.arguments,
                }))
            }
            EventMsg::McpToolCallEnd(event) => {
                Some(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate {
                    id: acp::ToolCallId(event.call_id.clone().into()),
                    fields: acp::ToolCallUpdateFields {
                        status: if event.is_success() {
                            Some(acp::ToolCallStatus::Completed)
                        } else {
                            Some(acp::ToolCallStatus::Failed)
                        },
                        content: match event.result {
                            Ok(content) => Some(
                                content
                                    .content
                                    .into_iter()
                                    .map(|content| to_acp_content_block(content).into())
                                    .collect(),
                            ),
                            Err(err) => Some(vec![err.into()]),
                        },
                        ..Default::default()
                    },
                }))
            }
            EventMsg::ExecApprovalRequest(_) | EventMsg::ApplyPatchApprovalRequest(_) => {
                // Handled by core
                None
            }
            EventMsg::ExecCommandBegin(event) => Some(acp::SessionUpdate::ToolCall(
                codex_core::acp::new_execute_tool_call(
                    &event.call_id,
                    &event.command,
                    acp::ToolCallStatus::InProgress,
                ),
            )),
            EventMsg::ExecCommandEnd(event) => {
                Some(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate {
                    id: acp::ToolCallId(event.call_id.into()),
                    fields: ToolCallUpdateFields {
                        status: if event.exit_code == 0 {
                            Some(acp::ToolCallStatus::Completed)
                        } else {
                            Some(acp::ToolCallStatus::Failed)
                        },
                        content: Some(vec![event.stdout.into(), event.stderr.into()]),
                        ..Default::default()
                    },
                }))
            }
            EventMsg::PatchApplyBegin(event) => Some(acp::SessionUpdate::ToolCall(
                codex_core::acp::new_patch_tool_call(
                    &event.call_id,
                    &event.changes,
                    acp::ToolCallStatus::InProgress,
                ),
            )),
            EventMsg::PatchApplyEnd(event) => {
                Some(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate {
                    id: acp::ToolCallId(event.call_id.into()),
                    fields: ToolCallUpdateFields {
                        status: if event.success {
                            Some(acp::ToolCallStatus::Completed)
                        } else {
                            Some(acp::ToolCallStatus::Failed)
                        },
                        ..Default::default()
                    },
                }))
            }
            EventMsg::TaskComplete(_) => return Ok(()),
            EventMsg::SessionConfigured(_)
            | EventMsg::TokenCount(_)
            | EventMsg::TaskStarted
            | EventMsg::GetHistoryEntryResponse(_)
            | EventMsg::BackgroundEvent(_)
            | EventMsg::ShutdownComplete => None,
        };

        if let Some(update) = acp_update {
            outgoing
                .send_notification(
                    acp::AGENT_METHODS.session_update,
                    Some(
                        serde_json::to_value(acp::SessionNotification {
                            session_id: acp_session_id.clone(),
                            update,
                        })
                        .unwrap_or_default(),
                    ),
                )
                .await;
        }
    }
}

fn acp_content_block_to_item(block: acp::ContentBlock) -> Option<InputItem> {
    match block {
        acp::ContentBlock::Text(text_content) => Some(InputItem::Text {
            text: text_content.text,
        }),
        acp::ContentBlock::ResourceLink(link) => Some(InputItem::Text {
            text: format!("@{}", link.uri),
        }),
        acp::ContentBlock::Image(image_content) => Some(InputItem::Image {
            image_url: image_content.data,
        }),
        acp::ContentBlock::Audio(_) | acp::ContentBlock::Resource(_) => None,
    }
}

fn to_acp_annotations(annotations: mcp_types::Annotations) -> acp::Annotations {
    acp::Annotations {
        audience: annotations.audience.map(|roles| {
            roles
                .into_iter()
                .map(|role| match role {
                    mcp_types::Role::User => acp::Role::User,
                    mcp_types::Role::Assistant => acp::Role::Assistant,
                })
                .collect()
        }),
        last_modified: annotations.last_modified,
        priority: annotations.priority,
    }
}

fn to_acp_embedded_resource_resource(
    resource: mcp_types::EmbeddedResourceResource,
) -> acp::EmbeddedResourceResource {
    match resource {
        mcp_types::EmbeddedResourceResource::TextResourceContents(text_contents) => {
            acp::EmbeddedResourceResource::TextResourceContents(acp::TextResourceContents {
                mime_type: text_contents.mime_type,
                text: text_contents.text,
                uri: text_contents.uri,
            })
        }
        mcp_types::EmbeddedResourceResource::BlobResourceContents(blob_contents) => {
            acp::EmbeddedResourceResource::BlobResourceContents(acp::BlobResourceContents {
                blob: blob_contents.blob,
                mime_type: blob_contents.mime_type,
                uri: blob_contents.uri,
            })
        }
    }
}

fn to_acp_content_block(block: mcp_types::ContentBlock) -> acp::ContentBlock {
    match block {
        ContentBlock::TextContent(text_content) => acp::ContentBlock::Text(acp::TextContent {
            annotations: text_content.annotations.map(to_acp_annotations),
            text: text_content.text,
        }),
        ContentBlock::ImageContent(image_content) => acp::ContentBlock::Image(acp::ImageContent {
            annotations: image_content.annotations.map(to_acp_annotations),
            data: image_content.data,
            mime_type: image_content.mime_type,
        }),
        ContentBlock::AudioContent(audio_content) => acp::ContentBlock::Audio(acp::AudioContent {
            annotations: audio_content.annotations.map(to_acp_annotations),
            data: audio_content.data,
            mime_type: audio_content.mime_type,
        }),
        ContentBlock::ResourceLink(resource_link) => {
            acp::ContentBlock::ResourceLink(acp::ResourceLink {
                annotations: resource_link.annotations.map(to_acp_annotations),
                uri: resource_link.uri,
                description: resource_link.description,
                mime_type: resource_link.mime_type,
                name: resource_link.name,
                size: resource_link.size,
                title: resource_link.title,
            })
        }
        ContentBlock::EmbeddedResource(embedded_resource) => {
            acp::ContentBlock::Resource(acp::EmbeddedResource {
                annotations: embedded_resource.annotations.map(to_acp_annotations),
                resource: to_acp_embedded_resource_resource(embedded_resource.resource),
            })
        }
    }
}
