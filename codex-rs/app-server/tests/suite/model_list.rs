use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use app_test_support::McpProcess;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::ListModelsParams;
use codex_app_server_protocol::ListModelsResponse;
use codex_app_server_protocol::Model;
use codex_app_server_protocol::ReasoningEffortOption;
use codex_app_server_protocol::RequestId;
use codex_protocol::config_types::ReasoningEffort;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_models_returns_all_models_with_large_limit() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = McpProcess::new(codex_home.path()).await?;

    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_list_models_request(ListModelsParams {
            page_size: Some(100),
            cursor: None,
        })
        .await?;

    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let ListModelsResponse { items, next_cursor } = to_response::<ListModelsResponse>(response)?;

    let expected_models = vec![
        Model {
            id: "gpt-5-codex".to_string(),
            model: "gpt-5-codex".to_string(),
            display_name: "gpt-5-codex".to_string(),
            description: "Optimized for coding tasks with many tools.".to_string(),
            supported_reasoning_efforts: vec![
                ReasoningEffortOption {
                    reasoning_effort: ReasoningEffort::Low,
                    description: "Fastest responses with limited reasoning".to_string(),
                },
                ReasoningEffortOption {
                    reasoning_effort: ReasoningEffort::Medium,
                    description: "Dynamically adjusts reasoning based on the task".to_string(),
                },
                ReasoningEffortOption {
                    reasoning_effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems"
                        .to_string(),
                },
            ],
            default_reasoning_effort: ReasoningEffort::Medium,
            is_default: true,
        },
        Model {
            id: "gpt-5".to_string(),
            model: "gpt-5".to_string(),
            display_name: "gpt-5".to_string(),
            description: "Broad world knowledge with strong general reasoning.".to_string(),
            supported_reasoning_efforts: vec![
                ReasoningEffortOption {
                    reasoning_effort: ReasoningEffort::Minimal,
                    description: "Fastest responses with little reasoning".to_string(),
                },
                ReasoningEffortOption {
                    reasoning_effort: ReasoningEffort::Low,
                    description: "Balances speed with some reasoning; useful for straightforward \
                                   queries and short explanations"
                        .to_string(),
                },
                ReasoningEffortOption {
                    reasoning_effort: ReasoningEffort::Medium,
                    description: "Provides a solid balance of reasoning depth and latency for \
                         general-purpose tasks"
                        .to_string(),
                },
                ReasoningEffortOption {
                    reasoning_effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems"
                        .to_string(),
                },
            ],
            default_reasoning_effort: ReasoningEffort::Medium,
            is_default: false,
        },
    ];

    assert_eq!(items, expected_models);
    assert!(next_cursor.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_models_pagination_works() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = McpProcess::new(codex_home.path()).await?;

    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let first_request = mcp
        .send_list_models_request(ListModelsParams {
            page_size: Some(1),
            cursor: None,
        })
        .await?;

    let first_response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(first_request)),
    )
    .await??;

    let ListModelsResponse {
        items: first_items,
        next_cursor: first_cursor,
    } = to_response::<ListModelsResponse>(first_response)?;

    assert_eq!(first_items.len(), 1);
    assert_eq!(first_items[0].id, "gpt-5-codex");
    let next_cursor = first_cursor.ok_or_else(|| anyhow!("cursor for second page"))?;

    let second_request = mcp
        .send_list_models_request(ListModelsParams {
            page_size: Some(1),
            cursor: Some(next_cursor.clone()),
        })
        .await?;

    let second_response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(second_request)),
    )
    .await??;

    let ListModelsResponse {
        items: second_items,
        next_cursor: second_cursor,
    } = to_response::<ListModelsResponse>(second_response)?;

    assert_eq!(second_items.len(), 1);
    assert_eq!(second_items[0].id, "gpt-5");
    assert!(second_cursor.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_models_rejects_invalid_cursor() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = McpProcess::new(codex_home.path()).await?;

    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_list_models_request(ListModelsParams {
            page_size: None,
            cursor: Some("invalid".to_string()),
        })
        .await?;

    let error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(error.id, RequestId::Integer(request_id));
    assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
    assert_eq!(error.error.message, "invalid cursor: invalid");
    Ok(())
}
