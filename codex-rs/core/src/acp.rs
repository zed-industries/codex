use agent_client_protocol as acp;
use anyhow::Context as _;
use anyhow::Result;
use codex_apply_patch::FileSystem;
use codex_apply_patch::StdFileSystem;
use mcp_types::CallToolResult;
use std::time::Duration;
use uuid::Uuid;

use crate::mcp_connection_manager::McpConnectionManager;
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
        let arguments = acp::ReadTextFileToolArguments {
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

        let output = serde_json::from_value::<acp::ReadTextFileToolOutput>(
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
        let arguments = acp::WriteTextFileToolArguments {
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

    let arguments = acp::RequestPermissionToolArguments {
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
    let result = serde_json::from_value::<acp::RequestPermissionToolOutput>(result)?;

    use acp::RequestPermissionOutcome::*;
    let decision = match result.outcome {
        Selected { option_id } => {
            if option_id == approve_for_session_id {
                ReviewDecision::Approved
            } else if option_id == approve_id {
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

pub fn new_patch_tool_call(call_id: &str, status: acp::ToolCallStatus) -> acp::ToolCall {
    acp::ToolCall {
        id: acp::ToolCallId(call_id.into()),
        label: "Edit".into(),
        kind: acp::ToolKind::Edit,
        status,
        content: vec![],
        locations: vec![],
        structured_content: None,
    }
}
