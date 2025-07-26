use agent_client_protocol as acp;
use anyhow::Context as _;
use anyhow::Result;
use codex_apply_patch::FileSystem;
use codex_apply_patch::StdFileSystem;
use mcp_types::CallToolResult;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

use crate::mcp_connection_manager::McpConnectionManager;
use crate::protocol::FileChange;
use crate::protocol::ReviewDecision;
use crate::util::strip_bash_lc_and_escape;

pub(crate) struct AcpFileSystem<'a> {
    session_id: Uuid,
    mcp_connection_manager: &'a McpConnectionManager,
    tools: &'a acp::ClientTools,
}

impl<'a> AcpFileSystem<'a> {
    pub fn new(
        session_id: Uuid,
        tools: &'a acp::ClientTools,
        mcp_connection_manager: &'a McpConnectionManager,
    ) -> Self {
        Self {
            session_id,
            mcp_connection_manager,
            tools,
        }
    }

    async fn read_text_file_impl(
        &self,
        tool: &acp::McpToolId,
        path: &std::path::Path,
    ) -> Result<String> {
        let arguments = acp::ReadTextFileArguments {
            session_id: acp::SessionId(self.session_id.to_string().into()),
            path: path.to_path_buf(),
            line: None,
            limit: None,
        };

        let CallToolResult {
            structured_content,
            is_error,
            ..
        } = self
            .mcp_connection_manager
            .call_tool(
                &tool.mcp_server,
                &tool.tool_name,
                Some(serde_json::to_value(arguments).unwrap_or_default()),
                Some(Duration::from_secs(15)),
            )
            .await?;

        if is_error.unwrap_or_default() {
            anyhow::bail!("Error reading text file: {:?}", structured_content);
        }

        let output = serde_json::from_value::<acp::ReadTextFileOutput>(
            structured_content.context("No output from read_text_file tool")?,
        )?;

        Ok(output.content)
    }

    async fn write_text_file_impl(
        &self,
        tool: &acp::McpToolId,
        path: &std::path::Path,
        content: String,
    ) -> Result<()> {
        let arguments = acp::WriteTextFileArguments {
            session_id: acp::SessionId(self.session_id.to_string().into()),
            path: path.to_path_buf(),
            content,
        };

        let CallToolResult {
            structured_content,
            is_error,
            ..
        } = self
            .mcp_connection_manager
            .call_tool(
                &tool.mcp_server,
                &tool.tool_name,
                Some(serde_json::to_value(arguments).unwrap_or_default()),
                Some(Duration::from_secs(15)),
            )
            .await?;

        if is_error.unwrap_or_default() {
            anyhow::bail!("Error writing text file: {:?}", structured_content);
        }

        Ok(())
    }
}

impl<'a> FileSystem for AcpFileSystem<'a> {
    async fn read_text_file(&self, path: &std::path::Path) -> std::io::Result<String> {
        let Some(tool) = self.tools.read_text_file.as_ref() else {
            return StdFileSystem.read_text_file(path).await;
        };

        self.read_text_file_impl(tool, path)
            .await
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))
    }

    async fn write_text_file(
        &self,
        path: &std::path::Path,
        contents: String,
    ) -> std::io::Result<()> {
        let Some(tool) = self.tools.write_text_file.as_ref() else {
            return StdFileSystem.write_text_file(path, contents).await;
        };

        self.write_text_file_impl(tool, path, contents)
            .await
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))
    }
}

pub(crate) async fn request_permission(
    permission_tool: &acp::McpToolId,
    tool_call: acp::ToolCall,
    session_id: Uuid,
    mcp_connection_manager: &McpConnectionManager,
) -> Result<ReviewDecision> {
    let approve_for_session_id = acp::PermissionOptionId("approve_for_session".into());
    let approve_id = acp::PermissionOptionId("approve".into());
    let deny_id = acp::PermissionOptionId("deny".into());

    let arguments = acp::RequestPermissionArguments {
        session_id: acp::SessionId(session_id.to_string().into()),
        tool_call: tool_call,
        options: vec![
            acp::PermissionOption {
                id: approve_for_session_id.clone(),
                label: "Approve for Session".into(),
                kind: acp::PermissionOptionKind::AllowAlways,
            },
            acp::PermissionOption {
                id: approve_id.clone(),
                label: "Approve".into(),
                kind: acp::PermissionOptionKind::AllowOnce,
            },
            acp::PermissionOption {
                id: deny_id.clone(),
                label: "Deny".into(),
                kind: acp::PermissionOptionKind::RejectOnce,
            },
        ],
    };

    let CallToolResult {
        structured_content, ..
    } = mcp_connection_manager
        .call_tool(
            &permission_tool.mcp_server,
            &permission_tool.tool_name,
            Some(serde_json::to_value(arguments).unwrap_or_default()),
            None,
        )
        .await?;

    let result = structured_content.context("No output from permission tool")?;
    let result = serde_json::from_value::<acp::RequestPermissionOutput>(result)?;

    use acp::RequestPermissionOutcome::*;
    let decision = match result.outcome {
        Selected { option_id } => {
            if option_id == approve_id {
                ReviewDecision::Approved
            } else if option_id == approve_for_session_id {
                ReviewDecision::ApprovedForSession
            } else if option_id == deny_id {
                ReviewDecision::Denied
            } else {
                anyhow::bail!("Unexpected permission option: {}", option_id);
            }
        }
        Canceled => ReviewDecision::Abort,
    };

    Ok(decision)
}

pub fn new_execute_tool_call(
    call_id: &str,
    command: &[String],
    status: acp::ToolCallStatus,
) -> acp::ToolCall {
    acp::ToolCall {
        id: acp::ToolCallId(call_id.into()),
        label: format!("`{}`", strip_bash_lc_and_escape(&command)),
        kind: acp::ToolKind::Execute,
        status,
        content: vec![],
        locations: vec![],
        structured_content: None,
    }
}

pub fn new_patch_tool_call(
    call_id: &str,
    changes: &HashMap<PathBuf, FileChange>,
    status: acp::ToolCallStatus,
) -> acp::ToolCall {
    let label = if changes.len() == 1 {
        let (path, change) = changes.iter().next().unwrap();
        let file_name = path.file_name().unwrap_or_default().display().to_string();

        match &change {
            FileChange::Delete => {
                // Only delete
                return acp::ToolCall {
                    id: acp::ToolCallId(call_id.into()),
                    label: format!("Delete “`{}`”", file_name),
                    kind: acp::ToolKind::Delete,
                    status,
                    content: vec![],
                    locations: vec![],
                    structured_content: None,
                };
            }
            FileChange::Update {
                move_path: Some(new_path),
                original_content,
                new_content,
                ..
            } if original_content == new_content => {
                // Only move
                return acp::ToolCall {
                    id: acp::ToolCallId(call_id.into()),
                    label: move_path_label(&path, new_path),
                    kind: acp::ToolKind::Move,
                    status,
                    content: vec![],
                    locations: vec![],
                    structured_content: None,
                };
            }
            _ => {}
        }

        format!("Edit “`{}`”", file_name)
    } else {
        format!("Edit {} files", changes.len())
    };

    let mut locations = Vec::with_capacity(changes.len());
    let mut content = Vec::with_capacity(changes.len());

    for (path, change) in changes.iter() {
        match change {
            FileChange::Add {
                content: new_content,
            } => {
                content.push(acp::ToolCallContent::Diff {
                    diff: acp::Diff {
                        path: path.clone(),
                        old_text: None,
                        new_text: new_content.clone(),
                    },
                });

                locations.push(acp::ToolCallLocation {
                    path: path.clone(),
                    line: None,
                });
            }
            FileChange::Delete => {
                content.push(acp::ToolCallContent::ContentBlock(
                    format!(
                        "Delete “`{}`”\n\n",
                        path.file_name().unwrap_or(path.as_os_str()).display()
                    )
                    .into(),
                ));
            }
            FileChange::Update {
                move_path,
                new_content,
                original_content,
                unified_diff: _,
            } => {
                if let Some(new_path) = move_path
                    && changes.len() > 1
                {
                    content.push(acp::ToolCallContent::ContentBlock(
                        move_path_label(&path, &new_path).into(),
                    ));

                    if status == acp::ToolCallStatus::Completed {
                        // Use new path if completed
                        locations.push(acp::ToolCallLocation {
                            path: new_path.clone(),
                            line: None,
                        });
                    } else {
                        locations.push(acp::ToolCallLocation {
                            path: path.clone(),
                            line: None,
                        });
                    }
                } else {
                    locations.push(acp::ToolCallLocation {
                        path: path.clone(),
                        line: None,
                    });
                }

                if original_content != new_content {
                    content.push(acp::ToolCallContent::Diff {
                        diff: acp::Diff {
                            path: path.clone(),
                            old_text: Some(original_content.clone()),
                            new_text: new_content.clone(),
                        },
                    });
                }
            }
        }
    }

    acp::ToolCall {
        id: acp::ToolCallId(call_id.into()),
        label,
        kind: acp::ToolKind::Edit,
        status,
        content: vec![],
        locations,
        structured_content: None,
    }
}

fn move_path_label(old: &Path, new: &Path) -> String {
    if old.parent() == new.parent() {
        let old_name = old.file_name().unwrap_or(old.as_os_str()).display();
        let new_name = new.file_name().unwrap_or(new.as_os_str()).display();

        format!("Rename “`{}`” to “`{}`”", old_name, new_name)
    } else {
        format!("Move “`{}`” to “`{}`”", old.display(), new.display())
    }
}
