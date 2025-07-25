use agent_client_protocol as acp;
use anyhow::{Context as _, Result};
use mcp_types::CallToolResult;
use uuid::Uuid;

use crate::{
    mcp_connection_manager::McpConnectionManager, protocol::ReviewDecision,
    util::strip_bash_lc_and_escape,
};

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
