//! Asynchronous worker that executes a **ACP** tool-call inside a spawned
//! Tokio task. Separated from `message_processor.rs` to keep that file small
//! and to make future feature-growth easier to manage.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::ToolCallUpdateFields;
use anyhow::Result;
use codex_core::Codex;
use codex_core::codex_wrapper::init_codex;
use codex_core::config::Config as CodexConfig;
use codex_core::protocol::EventMsg;
use codex_core::protocol::InputItem;
use codex_core::protocol::Op;
use codex_core::protocol::ReviewDecision;
use mcp_types::CallToolResult;
use mcp_types::ContentBlock;
use mcp_types::RequestId;
use mcp_types::TextContent;
use shlex::try_join;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::outgoing_message::OutgoingMessageSender;

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

pub async fn prompt(
    acp_session_id: acp::SessionId,
    codex: Arc<Codex>,
    prompt: Vec<acp::ContentBlock>,
    outgoing: Arc<OutgoingMessageSender>,
) -> Result<()> {
    let submission_id = codex
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
            EventMsg::AgentMessageDelta(event) => {
                Some(acp::SessionUpdate::AgentMessageChunk(event.delta.into()))
            }
            EventMsg::AgentReasoningDelta(event) => {
                Some(acp::SessionUpdate::AgentThoughtChunk(event.delta.into()))
            }
            EventMsg::McpToolCallBegin(mcp_tool_call_begin_event) => {
                Some(acp::SessionUpdate::ToolCall(acp::ToolCall {
                    id: acp::ToolCallId(mcp_tool_call_begin_event.call_id.into()),
                    label: format!(
                        "{}: {}",
                        mcp_tool_call_begin_event.server, mcp_tool_call_begin_event.tool
                    ),
                    kind: acp::ToolKind::Other,
                    status: acp::ToolCallStatus::InProgress,
                    content: vec![],
                    locations: vec![],
                    structured_content: mcp_tool_call_begin_event.arguments,
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
                                    .map(|content| {
                                        acp::ToolCallContent::ContentBlock(to_acp_content_block(
                                            content,
                                        ))
                                    })
                                    .collect(),
                            ),
                            Err(err) => Some(vec![err.into()]),
                        },
                        ..Default::default()
                    },
                }))
            }
            EventMsg::ExecApprovalRequest(_) => {
                // todo!
                codex
                    .submit(Op::ExecApproval {
                        id: submission_id.clone(),
                        decision: ReviewDecision::Approved,
                    })
                    .await?;

                None
            }
            EventMsg::ExecCommandBegin(exec_command_begin_event) => {
                Some(acp::SessionUpdate::ToolCall(acp::ToolCall {
                    id: acp::ToolCallId(exec_command_begin_event.call_id.into()),
                    label: format!(
                        "Run {}",
                        strip_bash_lc_and_escape(&exec_command_begin_event.command)
                    ),
                    kind: acp::ToolKind::Execute,
                    status: acp::ToolCallStatus::InProgress,
                    content: vec![],
                    locations: vec![],
                    structured_content: None,
                }))
            }
            EventMsg::ExecCommandEnd(exec_command_end_event) => {
                Some(acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate {
                    id: acp::ToolCallId(exec_command_end_event.call_id.into()),
                    fields: ToolCallUpdateFields {
                        status: if exec_command_end_event.exit_code == 0 {
                            Some(acp::ToolCallStatus::Completed)
                        } else {
                            Some(acp::ToolCallStatus::Failed)
                        },
                        content: Some(vec![
                            exec_command_end_event.stdout.into(),
                            exec_command_end_event.stderr.into(),
                        ]),
                        ..Default::default()
                    },
                }))
            }
            EventMsg::PatchApplyBegin(_) => {
                // todo!
                None
            }
            EventMsg::PatchApplyEnd(_) => {
                // todo!
                None
            }
            EventMsg::TaskComplete(_) => return Ok(()),
            EventMsg::ApplyPatchApprovalRequest(_)
            | EventMsg::SessionConfigured(_)
            | EventMsg::TokenCount(_)
            | EventMsg::TaskStarted
            | EventMsg::GetHistoryEntryResponse(_)
            | EventMsg::BackgroundEvent(_)
            | EventMsg::ShutdownComplete => None,
        };

        if let Some(update) = acp_update {
            outgoing
                .send_notification(
                    acp::SESSION_UPDATE_METHOD_NAME,
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
            text: link.uri.to_string(),
        }),
        acp::ContentBlock::Image(image_content) => Some(InputItem::Image {
            image_url: image_content.data,
        }),
        // todo! fail?
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

// todo: share with TUI
pub(crate) fn escape_command(command: &[String]) -> String {
    try_join(command.iter().map(|s| s.as_str())).unwrap_or_else(|_| command.join(" "))
}

pub(crate) fn strip_bash_lc_and_escape(command: &[String]) -> String {
    match command {
        // exactly three items
        [first, second, third]
            // first two must be "bash", "-lc"
            if first == "bash" && second == "-lc" =>
        {
            third.clone()        // borrow `third`
        }
        _ => escape_command(command),
    }
}
