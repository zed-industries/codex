use crate::client_common::tools::ResponsesApiTool;
use crate::client_common::tools::ToolSpec;
use crate::model_family::ModelFamily;
use crate::tools::handlers::PLAN_TOOL;
use crate::tools::handlers::apply_patch::ApplyPatchToolType;
use crate::tools::handlers::apply_patch::create_apply_patch_freeform_tool;
use crate::tools::handlers::apply_patch::create_apply_patch_json_tool;
use crate::tools::registry::ToolRegistryBuilder;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum ConfigShellToolType {
    Default,
    Local,
    Streamable,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolsConfig {
    pub shell_type: ConfigShellToolType,
    pub plan_tool: bool,
    pub apply_patch_tool_type: Option<ApplyPatchToolType>,
    pub web_search_request: bool,
    pub include_view_image_tool: bool,
    pub experimental_unified_exec_tool: bool,
    pub experimental_supported_tools: Vec<String>,
}

pub(crate) struct ToolsConfigParams<'a> {
    pub(crate) model_family: &'a ModelFamily,
    pub(crate) include_plan_tool: bool,
    pub(crate) include_apply_patch_tool: bool,
    pub(crate) include_web_search_request: bool,
    pub(crate) use_streamable_shell_tool: bool,
    pub(crate) include_view_image_tool: bool,
    pub(crate) experimental_unified_exec_tool: bool,
}

impl ToolsConfig {
    pub fn new(params: &ToolsConfigParams) -> Self {
        let ToolsConfigParams {
            model_family,
            include_plan_tool,
            include_apply_patch_tool,
            include_web_search_request,
            use_streamable_shell_tool,
            include_view_image_tool,
            experimental_unified_exec_tool,
        } = params;
        let shell_type = if *use_streamable_shell_tool {
            ConfigShellToolType::Streamable
        } else if model_family.uses_local_shell_tool {
            ConfigShellToolType::Local
        } else {
            ConfigShellToolType::Default
        };

        let apply_patch_tool_type = match model_family.apply_patch_tool_type {
            Some(ApplyPatchToolType::Freeform) => Some(ApplyPatchToolType::Freeform),
            Some(ApplyPatchToolType::Function) => Some(ApplyPatchToolType::Function),
            None => {
                if *include_apply_patch_tool {
                    Some(ApplyPatchToolType::Freeform)
                } else {
                    None
                }
            }
        };

        Self {
            shell_type,
            plan_tool: *include_plan_tool,
            apply_patch_tool_type,
            web_search_request: *include_web_search_request,
            include_view_image_tool: *include_view_image_tool,
            experimental_unified_exec_tool: *experimental_unified_exec_tool,
            experimental_supported_tools: model_family.experimental_supported_tools.clone(),
        }
    }
}

/// Generic JSON‑Schema subset needed for our tool definitions
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum JsonSchema {
    Boolean {
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    String {
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// MCP schema allows "number" | "integer" for Number
    #[serde(alias = "integer")]
    Number {
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    Array {
        items: Box<JsonSchema>,

        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    Object {
        properties: BTreeMap<String, JsonSchema>,
        #[serde(skip_serializing_if = "Option::is_none")]
        required: Option<Vec<String>>,
        #[serde(
            rename = "additionalProperties",
            skip_serializing_if = "Option::is_none"
        )]
        additional_properties: Option<AdditionalProperties>,
    },
}

/// Whether additional properties are allowed, and if so, any required schema
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub(crate) enum AdditionalProperties {
    Boolean(bool),
    Schema(Box<JsonSchema>),
}

impl From<bool> for AdditionalProperties {
    fn from(b: bool) -> Self {
        Self::Boolean(b)
    }
}

impl From<JsonSchema> for AdditionalProperties {
    fn from(s: JsonSchema) -> Self {
        Self::Schema(Box::new(s))
    }
}

fn create_unified_exec_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "input".to_string(),
        JsonSchema::Array {
            items: Box::new(JsonSchema::String { description: None }),
            description: Some(
                "When no session_id is provided, treat the array as the command and arguments \
                 to launch. When session_id is set, concatenate the strings (in order) and write \
                 them to the session's stdin."
                    .to_string(),
            ),
        },
    );
    properties.insert(
        "session_id".to_string(),
        JsonSchema::String {
            description: Some(
                "Identifier for an existing interactive session. If omitted, a new command \
                 is spawned."
                    .to_string(),
            ),
        },
    );
    properties.insert(
        "timeout_ms".to_string(),
        JsonSchema::Number {
            description: Some(
                "Maximum time in milliseconds to wait for output after writing the input."
                    .to_string(),
            ),
        },
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "unified_exec".to_string(),
        description:
            "Runs a command in a PTY. Provide a session_id to reuse an existing interactive session.".to_string(),
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["input".to_string()]),
            additional_properties: Some(false.into()),
        },
    })
}

fn create_shell_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "command".to_string(),
        JsonSchema::Array {
            items: Box::new(JsonSchema::String { description: None }),
            description: Some("The command to execute".to_string()),
        },
    );
    properties.insert(
        "workdir".to_string(),
        JsonSchema::String {
            description: Some("The working directory to execute the command in".to_string()),
        },
    );
    properties.insert(
        "timeout_ms".to_string(),
        JsonSchema::Number {
            description: Some("The timeout for the command in milliseconds".to_string()),
        },
    );

    properties.insert(
        "with_escalated_permissions".to_string(),
        JsonSchema::Boolean {
            description: Some("Whether to request escalated permissions. Set to true if command needs to be run without sandbox restrictions".to_string()),
        },
    );
    properties.insert(
        "justification".to_string(),
        JsonSchema::String {
            description: Some("Only set if with_escalated_permissions is true. 1-sentence explanation of why we want to run this command.".to_string()),
        },
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "shell".to_string(),
        description: "Runs a shell command and returns its output.".to_string(),
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["command".to_string()]),
            additional_properties: Some(false.into()),
        },
    })
}

fn create_view_image_tool() -> ToolSpec {
    // Support only local filesystem path.
    let mut properties = BTreeMap::new();
    properties.insert(
        "path".to_string(),
        JsonSchema::String {
            description: Some("Local filesystem path to an image file".to_string()),
        },
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "view_image".to_string(),
        description:
            "Attach a local image (by filesystem path) to the conversation context for this turn."
                .to_string(),
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["path".to_string()]),
            additional_properties: Some(false.into()),
        },
    })
}

fn create_test_sync_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "sleep_before_ms".to_string(),
        JsonSchema::Number {
            description: Some("Optional delay in milliseconds before any other action".to_string()),
        },
    );
    properties.insert(
        "sleep_after_ms".to_string(),
        JsonSchema::Number {
            description: Some(
                "Optional delay in milliseconds after completing the barrier".to_string(),
            ),
        },
    );

    let mut barrier_properties = BTreeMap::new();
    barrier_properties.insert(
        "id".to_string(),
        JsonSchema::String {
            description: Some(
                "Identifier shared by concurrent calls that should rendezvous".to_string(),
            ),
        },
    );
    barrier_properties.insert(
        "participants".to_string(),
        JsonSchema::Number {
            description: Some(
                "Number of tool calls that must arrive before the barrier opens".to_string(),
            ),
        },
    );
    barrier_properties.insert(
        "timeout_ms".to_string(),
        JsonSchema::Number {
            description: Some("Maximum time in milliseconds to wait at the barrier".to_string()),
        },
    );

    properties.insert(
        "barrier".to_string(),
        JsonSchema::Object {
            properties: barrier_properties,
            required: Some(vec!["id".to_string(), "participants".to_string()]),
            additional_properties: Some(false.into()),
        },
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "test_sync_tool".to_string(),
        description: "Internal synchronization helper used by Codex integration tests.".to_string(),
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: None,
            additional_properties: Some(false.into()),
        },
    })
}

fn create_read_file_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "file_path".to_string(),
        JsonSchema::String {
            description: Some("Absolute path to the file".to_string()),
        },
    );
    properties.insert(
        "offset".to_string(),
        JsonSchema::Number {
            description: Some(
                "The line number to start reading from. Must be 1 or greater.".to_string(),
            ),
        },
    );
    properties.insert(
        "limit".to_string(),
        JsonSchema::Number {
            description: Some("The maximum number of lines to return.".to_string()),
        },
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "read_file".to_string(),
        description:
            "Reads a local file with 1-indexed line numbers and returns up to the requested number of lines."
                .to_string(),
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["file_path".to_string()]),
            additional_properties: Some(false.into()),
        },
    })
}
/// TODO(dylan): deprecate once we get rid of json tool
#[derive(Serialize, Deserialize)]
pub(crate) struct ApplyPatchToolArgs {
    pub(crate) input: String,
}

/// Returns JSON values that are compatible with Function Calling in the
/// Responses API:
/// https://platform.openai.com/docs/guides/function-calling?api-mode=responses
pub fn create_tools_json_for_responses_api(
    tools: &[ToolSpec],
) -> crate::error::Result<Vec<serde_json::Value>> {
    let mut tools_json = Vec::new();

    for tool in tools {
        let json = serde_json::to_value(tool)?;
        tools_json.push(json);
    }

    Ok(tools_json)
}
/// Returns JSON values that are compatible with Function Calling in the
/// Chat Completions API:
/// https://platform.openai.com/docs/guides/function-calling?api-mode=chat
pub(crate) fn create_tools_json_for_chat_completions_api(
    tools: &[ToolSpec],
) -> crate::error::Result<Vec<serde_json::Value>> {
    // We start with the JSON for the Responses API and than rewrite it to match
    // the chat completions tool call format.
    let responses_api_tools_json = create_tools_json_for_responses_api(tools)?;
    let tools_json = responses_api_tools_json
        .into_iter()
        .filter_map(|mut tool| {
            if tool.get("type") != Some(&serde_json::Value::String("function".to_string())) {
                return None;
            }

            if let Some(map) = tool.as_object_mut() {
                // Remove "type" field as it is not needed in chat completions.
                map.remove("type");
                Some(json!({
                    "type": "function",
                    "function": map,
                }))
            } else {
                None
            }
        })
        .collect::<Vec<serde_json::Value>>();
    Ok(tools_json)
}

pub(crate) fn mcp_tool_to_openai_tool(
    fully_qualified_name: String,
    tool: mcp_types::Tool,
) -> Result<ResponsesApiTool, serde_json::Error> {
    let mcp_types::Tool {
        description,
        mut input_schema,
        ..
    } = tool;

    // OpenAI models mandate the "properties" field in the schema. The Agents
    // SDK fixed this by inserting an empty object for "properties" if it is not
    // already present https://github.com/openai/openai-agents-python/issues/449
    // so here we do the same.
    if input_schema.properties.is_none() {
        input_schema.properties = Some(serde_json::Value::Object(serde_json::Map::new()));
    }

    // Serialize to a raw JSON value so we can sanitize schemas coming from MCP
    // servers. Some servers omit the top-level or nested `type` in JSON
    // Schemas (e.g. using enum/anyOf), or use unsupported variants like
    // `integer`. Our internal JsonSchema is a small subset and requires
    // `type`, so we coerce/sanitize here for compatibility.
    let mut serialized_input_schema = serde_json::to_value(input_schema)?;
    sanitize_json_schema(&mut serialized_input_schema);
    let input_schema = serde_json::from_value::<JsonSchema>(serialized_input_schema)?;

    Ok(ResponsesApiTool {
        name: fully_qualified_name,
        description: description.unwrap_or_default(),
        strict: false,
        parameters: input_schema,
    })
}

/// Sanitize a JSON Schema (as serde_json::Value) so it can fit our limited
/// JsonSchema enum. This function:
/// - Ensures every schema object has a "type". If missing, infers it from
///   common keywords (properties => object, items => array, enum/const/format => string)
///   and otherwise defaults to "string".
/// - Fills required child fields (e.g. array items, object properties) with
///   permissive defaults when absent.
fn sanitize_json_schema(value: &mut JsonValue) {
    match value {
        JsonValue::Bool(_) => {
            // JSON Schema boolean form: true/false. Coerce to an accept-all string.
            *value = json!({ "type": "string" });
        }
        JsonValue::Array(arr) => {
            for v in arr.iter_mut() {
                sanitize_json_schema(v);
            }
        }
        JsonValue::Object(map) => {
            // First, recursively sanitize known nested schema holders
            if let Some(props) = map.get_mut("properties")
                && let Some(props_map) = props.as_object_mut()
            {
                for (_k, v) in props_map.iter_mut() {
                    sanitize_json_schema(v);
                }
            }
            if let Some(items) = map.get_mut("items") {
                sanitize_json_schema(items);
            }
            // Some schemas use oneOf/anyOf/allOf - sanitize their entries
            for combiner in ["oneOf", "anyOf", "allOf", "prefixItems"] {
                if let Some(v) = map.get_mut(combiner) {
                    sanitize_json_schema(v);
                }
            }

            // Normalize/ensure type
            let mut ty = map.get("type").and_then(|v| v.as_str()).map(str::to_string);

            // If type is an array (union), pick first supported; else leave to inference
            if ty.is_none()
                && let Some(JsonValue::Array(types)) = map.get("type")
            {
                for t in types {
                    if let Some(tt) = t.as_str()
                        && matches!(
                            tt,
                            "object" | "array" | "string" | "number" | "integer" | "boolean"
                        )
                    {
                        ty = Some(tt.to_string());
                        break;
                    }
                }
            }

            // Infer type if still missing
            if ty.is_none() {
                if map.contains_key("properties")
                    || map.contains_key("required")
                    || map.contains_key("additionalProperties")
                {
                    ty = Some("object".to_string());
                } else if map.contains_key("items") || map.contains_key("prefixItems") {
                    ty = Some("array".to_string());
                } else if map.contains_key("enum")
                    || map.contains_key("const")
                    || map.contains_key("format")
                {
                    ty = Some("string".to_string());
                } else if map.contains_key("minimum")
                    || map.contains_key("maximum")
                    || map.contains_key("exclusiveMinimum")
                    || map.contains_key("exclusiveMaximum")
                    || map.contains_key("multipleOf")
                {
                    ty = Some("number".to_string());
                }
            }
            // If we still couldn't infer, default to string
            let ty = ty.unwrap_or_else(|| "string".to_string());
            map.insert("type".to_string(), JsonValue::String(ty.to_string()));

            // Ensure object schemas have properties map
            if ty == "object" {
                if !map.contains_key("properties") {
                    map.insert(
                        "properties".to_string(),
                        JsonValue::Object(serde_json::Map::new()),
                    );
                }
                // If additionalProperties is an object schema, sanitize it too.
                // Leave booleans as-is, since JSON Schema allows boolean here.
                if let Some(ap) = map.get_mut("additionalProperties") {
                    let is_bool = matches!(ap, JsonValue::Bool(_));
                    if !is_bool {
                        sanitize_json_schema(ap);
                    }
                }
            }

            // Ensure array schemas have items
            if ty == "array" && !map.contains_key("items") {
                map.insert("items".to_string(), json!({ "type": "string" }));
            }
        }
        _ => {}
    }
}

/// Builds the tool registry builder while collecting tool specs for later serialization.
pub(crate) fn build_specs(
    config: &ToolsConfig,
    mcp_tools: Option<HashMap<String, mcp_types::Tool>>,
) -> ToolRegistryBuilder {
    use crate::exec_command::EXEC_COMMAND_TOOL_NAME;
    use crate::exec_command::WRITE_STDIN_TOOL_NAME;
    use crate::exec_command::create_exec_command_tool_for_responses_api;
    use crate::exec_command::create_write_stdin_tool_for_responses_api;
    use crate::tools::handlers::ApplyPatchHandler;
    use crate::tools::handlers::ExecStreamHandler;
    use crate::tools::handlers::McpHandler;
    use crate::tools::handlers::PlanHandler;
    use crate::tools::handlers::ReadFileHandler;
    use crate::tools::handlers::ShellHandler;
    use crate::tools::handlers::TestSyncHandler;
    use crate::tools::handlers::UnifiedExecHandler;
    use crate::tools::handlers::ViewImageHandler;
    use std::sync::Arc;

    let mut builder = ToolRegistryBuilder::new();

    let shell_handler = Arc::new(ShellHandler);
    let exec_stream_handler = Arc::new(ExecStreamHandler);
    let unified_exec_handler = Arc::new(UnifiedExecHandler);
    let plan_handler = Arc::new(PlanHandler);
    let apply_patch_handler = Arc::new(ApplyPatchHandler);
    let view_image_handler = Arc::new(ViewImageHandler);
    let mcp_handler = Arc::new(McpHandler);

    if config.experimental_unified_exec_tool {
        builder.push_spec(create_unified_exec_tool());
        builder.register_handler("unified_exec", unified_exec_handler);
    } else {
        match &config.shell_type {
            ConfigShellToolType::Default => {
                builder.push_spec(create_shell_tool());
            }
            ConfigShellToolType::Local => {
                builder.push_spec(ToolSpec::LocalShell {});
            }
            ConfigShellToolType::Streamable => {
                builder.push_spec(ToolSpec::Function(
                    create_exec_command_tool_for_responses_api(),
                ));
                builder.push_spec(ToolSpec::Function(
                    create_write_stdin_tool_for_responses_api(),
                ));
                builder.register_handler(EXEC_COMMAND_TOOL_NAME, exec_stream_handler.clone());
                builder.register_handler(WRITE_STDIN_TOOL_NAME, exec_stream_handler);
            }
        }
    }

    // Always register shell aliases so older prompts remain compatible.
    builder.register_handler("shell", shell_handler.clone());
    builder.register_handler("container.exec", shell_handler.clone());
    builder.register_handler("local_shell", shell_handler);

    if config.plan_tool {
        builder.push_spec(PLAN_TOOL.clone());
        builder.register_handler("update_plan", plan_handler);
    }

    if let Some(apply_patch_tool_type) = &config.apply_patch_tool_type {
        match apply_patch_tool_type {
            ApplyPatchToolType::Freeform => {
                builder.push_spec(create_apply_patch_freeform_tool());
            }
            ApplyPatchToolType::Function => {
                builder.push_spec(create_apply_patch_json_tool());
            }
        }
        builder.register_handler("apply_patch", apply_patch_handler);
    }

    if config
        .experimental_supported_tools
        .iter()
        .any(|tool| tool == "read_file")
    {
        let read_file_handler = Arc::new(ReadFileHandler);
        builder.push_spec_with_parallel_support(create_read_file_tool(), true);
        builder.register_handler("read_file", read_file_handler);
    }

    if config
        .experimental_supported_tools
        .iter()
        .any(|tool| tool == "test_sync_tool")
    {
        let test_sync_handler = Arc::new(TestSyncHandler);
        builder.push_spec_with_parallel_support(create_test_sync_tool(), true);
        builder.register_handler("test_sync_tool", test_sync_handler);
    }

    if config.web_search_request {
        builder.push_spec(ToolSpec::WebSearch {});
    }

    if config.include_view_image_tool {
        builder.push_spec_with_parallel_support(create_view_image_tool(), true);
        builder.register_handler("view_image", view_image_handler);
    }

    if let Some(mcp_tools) = mcp_tools {
        let mut entries: Vec<(String, mcp_types::Tool)> = mcp_tools.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, tool) in entries.into_iter() {
            match mcp_tool_to_openai_tool(name.clone(), tool.clone()) {
                Ok(converted_tool) => {
                    builder.push_spec(ToolSpec::Function(converted_tool));
                    builder.register_handler(name, mcp_handler.clone());
                }
                Err(e) => {
                    tracing::error!("Failed to convert {name:?} MCP tool to OpenAI tool: {e:?}");
                }
            }
        }
    }

    builder
}

#[cfg(test)]
mod tests {
    use crate::client_common::tools::FreeformTool;
    use crate::model_family::find_family_for_model;
    use crate::tools::registry::ConfiguredToolSpec;
    use mcp_types::ToolInputSchema;
    use pretty_assertions::assert_eq;

    use super::*;

    fn tool_name(tool: &ToolSpec) -> &str {
        match tool {
            ToolSpec::Function(ResponsesApiTool { name, .. }) => name,
            ToolSpec::LocalShell {} => "local_shell",
            ToolSpec::WebSearch {} => "web_search",
            ToolSpec::Freeform(FreeformTool { name, .. }) => name,
        }
    }

    fn assert_eq_tool_names(tools: &[ConfiguredToolSpec], expected_names: &[&str]) {
        let tool_names = tools
            .iter()
            .map(|tool| tool_name(&tool.spec))
            .collect::<Vec<_>>();

        assert_eq!(
            tool_names.len(),
            expected_names.len(),
            "tool_name mismatch, {tool_names:?}, {expected_names:?}",
        );
        for (name, expected_name) in tool_names.iter().zip(expected_names.iter()) {
            assert_eq!(
                name, expected_name,
                "tool_name mismatch, {name:?}, {expected_name:?}"
            );
        }
    }

    fn find_tool<'a>(
        tools: &'a [ConfiguredToolSpec],
        expected_name: &str,
    ) -> &'a ConfiguredToolSpec {
        tools
            .iter()
            .find(|tool| tool_name(&tool.spec) == expected_name)
            .unwrap_or_else(|| panic!("expected tool {expected_name}"))
    }

    #[test]
    fn test_build_specs() {
        let model_family = find_family_for_model("codex-mini-latest")
            .expect("codex-mini-latest should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: true,
            include_apply_patch_tool: false,
            include_web_search_request: true,
            use_streamable_shell_tool: false,
            include_view_image_tool: true,
            experimental_unified_exec_tool: true,
        });
        let (tools, _) = build_specs(&config, Some(HashMap::new())).build();

        assert_eq_tool_names(
            &tools,
            &["unified_exec", "update_plan", "web_search", "view_image"],
        );
    }

    #[test]
    fn test_build_specs_default_shell() {
        let model_family = find_family_for_model("o3").expect("o3 should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: true,
            include_apply_patch_tool: false,
            include_web_search_request: true,
            use_streamable_shell_tool: false,
            include_view_image_tool: true,
            experimental_unified_exec_tool: true,
        });
        let (tools, _) = build_specs(&config, Some(HashMap::new())).build();

        assert_eq_tool_names(
            &tools,
            &["unified_exec", "update_plan", "web_search", "view_image"],
        );
    }

    #[test]
    #[ignore]
    fn test_parallel_support_flags() {
        let model_family = find_family_for_model("gpt-5-codex")
            .expect("codex-mini-latest should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: false,
            include_apply_patch_tool: false,
            include_web_search_request: false,
            use_streamable_shell_tool: false,
            include_view_image_tool: false,
            experimental_unified_exec_tool: true,
        });
        let (tools, _) = build_specs(&config, None).build();

        assert!(!find_tool(&tools, "unified_exec").supports_parallel_tool_calls);
        assert!(find_tool(&tools, "read_file").supports_parallel_tool_calls);
    }

    #[test]
    fn test_test_model_family_includes_sync_tool() {
        let model_family = find_family_for_model("test-gpt-5-codex")
            .expect("test-gpt-5-codex should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: false,
            include_apply_patch_tool: false,
            include_web_search_request: false,
            use_streamable_shell_tool: false,
            include_view_image_tool: false,
            experimental_unified_exec_tool: false,
        });
        let (tools, _) = build_specs(&config, None).build();

        assert!(
            tools
                .iter()
                .any(|tool| tool_name(&tool.spec) == "test_sync_tool")
        );
        assert!(
            tools
                .iter()
                .any(|tool| tool_name(&tool.spec) == "read_file")
        );
    }

    #[test]
    fn test_build_specs_mcp_tools() {
        let model_family = find_family_for_model("o3").expect("o3 should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: false,
            include_apply_patch_tool: false,
            include_web_search_request: true,
            use_streamable_shell_tool: false,
            include_view_image_tool: true,
            experimental_unified_exec_tool: true,
        });
        let (tools, _) = build_specs(
            &config,
            Some(HashMap::from([(
                "test_server/do_something_cool".to_string(),
                mcp_types::Tool {
                    name: "do_something_cool".to_string(),
                    input_schema: ToolInputSchema {
                        properties: Some(serde_json::json!({
                            "string_argument": {
                                "type": "string",
                            },
                            "number_argument": {
                                "type": "number",
                            },
                            "object_argument": {
                                "type": "object",
                                "properties": {
                                    "string_property": { "type": "string" },
                                    "number_property": { "type": "number" },
                                },
                                "required": [
                                    "string_property",
                                    "number_property",
                                ],
                                "additionalProperties": Some(false),
                            },
                        })),
                        required: None,
                        r#type: "object".to_string(),
                    },
                    output_schema: None,
                    title: None,
                    annotations: None,
                    description: Some("Do something cool".to_string()),
                },
            )])),
        )
        .build();

        assert_eq_tool_names(
            &tools,
            &[
                "unified_exec",
                "web_search",
                "view_image",
                "test_server/do_something_cool",
            ],
        );

        assert_eq!(
            tools[3].spec,
            ToolSpec::Function(ResponsesApiTool {
                name: "test_server/do_something_cool".to_string(),
                parameters: JsonSchema::Object {
                    properties: BTreeMap::from([
                        (
                            "string_argument".to_string(),
                            JsonSchema::String { description: None }
                        ),
                        (
                            "number_argument".to_string(),
                            JsonSchema::Number { description: None }
                        ),
                        (
                            "object_argument".to_string(),
                            JsonSchema::Object {
                                properties: BTreeMap::from([
                                    (
                                        "string_property".to_string(),
                                        JsonSchema::String { description: None }
                                    ),
                                    (
                                        "number_property".to_string(),
                                        JsonSchema::Number { description: None }
                                    ),
                                ]),
                                required: Some(vec![
                                    "string_property".to_string(),
                                    "number_property".to_string(),
                                ]),
                                additional_properties: Some(false.into()),
                            },
                        ),
                    ]),
                    required: None,
                    additional_properties: None,
                },
                description: "Do something cool".to_string(),
                strict: false,
            })
        );
    }

    #[test]
    fn test_build_specs_mcp_tools_sorted_by_name() {
        let model_family = find_family_for_model("o3").expect("o3 should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: false,
            include_apply_patch_tool: false,
            include_web_search_request: false,
            use_streamable_shell_tool: false,
            include_view_image_tool: true,
            experimental_unified_exec_tool: true,
        });

        // Intentionally construct a map with keys that would sort alphabetically.
        let tools_map: HashMap<String, mcp_types::Tool> = HashMap::from([
            (
                "test_server/do".to_string(),
                mcp_types::Tool {
                    name: "a".to_string(),
                    input_schema: ToolInputSchema {
                        properties: Some(serde_json::json!({})),
                        required: None,
                        r#type: "object".to_string(),
                    },
                    output_schema: None,
                    title: None,
                    annotations: None,
                    description: Some("a".to_string()),
                },
            ),
            (
                "test_server/something".to_string(),
                mcp_types::Tool {
                    name: "b".to_string(),
                    input_schema: ToolInputSchema {
                        properties: Some(serde_json::json!({})),
                        required: None,
                        r#type: "object".to_string(),
                    },
                    output_schema: None,
                    title: None,
                    annotations: None,
                    description: Some("b".to_string()),
                },
            ),
            (
                "test_server/cool".to_string(),
                mcp_types::Tool {
                    name: "c".to_string(),
                    input_schema: ToolInputSchema {
                        properties: Some(serde_json::json!({})),
                        required: None,
                        r#type: "object".to_string(),
                    },
                    output_schema: None,
                    title: None,
                    annotations: None,
                    description: Some("c".to_string()),
                },
            ),
        ]);

        let (tools, _) = build_specs(&config, Some(tools_map)).build();
        // Expect unified_exec first, followed by MCP tools sorted by fully-qualified name.
        assert_eq_tool_names(
            &tools,
            &[
                "unified_exec",
                "view_image",
                "test_server/cool",
                "test_server/do",
                "test_server/something",
            ],
        );
    }

    #[test]
    fn test_mcp_tool_property_missing_type_defaults_to_string() {
        let model_family = find_family_for_model("gpt-5-codex")
            .expect("gpt-5-codex should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: false,
            include_apply_patch_tool: false,
            include_web_search_request: true,
            use_streamable_shell_tool: false,
            include_view_image_tool: true,
            experimental_unified_exec_tool: true,
        });

        let (tools, _) = build_specs(
            &config,
            Some(HashMap::from([(
                "dash/search".to_string(),
                mcp_types::Tool {
                    name: "search".to_string(),
                    input_schema: ToolInputSchema {
                        properties: Some(serde_json::json!({
                            "query": {
                                "description": "search query"
                            }
                        })),
                        required: None,
                        r#type: "object".to_string(),
                    },
                    output_schema: None,
                    title: None,
                    annotations: None,
                    description: Some("Search docs".to_string()),
                },
            )])),
        )
        .build();

        assert_eq_tool_names(
            &tools,
            &[
                "unified_exec",
                "apply_patch",
                "web_search",
                "view_image",
                "dash/search",
            ],
        );

        assert_eq!(
            tools[4].spec,
            ToolSpec::Function(ResponsesApiTool {
                name: "dash/search".to_string(),
                parameters: JsonSchema::Object {
                    properties: BTreeMap::from([(
                        "query".to_string(),
                        JsonSchema::String {
                            description: Some("search query".to_string())
                        }
                    )]),
                    required: None,
                    additional_properties: None,
                },
                description: "Search docs".to_string(),
                strict: false,
            })
        );
    }

    #[test]
    fn test_mcp_tool_integer_normalized_to_number() {
        let model_family = find_family_for_model("gpt-5-codex")
            .expect("gpt-5-codex should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: false,
            include_apply_patch_tool: false,
            include_web_search_request: true,
            use_streamable_shell_tool: false,
            include_view_image_tool: true,
            experimental_unified_exec_tool: true,
        });

        let (tools, _) = build_specs(
            &config,
            Some(HashMap::from([(
                "dash/paginate".to_string(),
                mcp_types::Tool {
                    name: "paginate".to_string(),
                    input_schema: ToolInputSchema {
                        properties: Some(serde_json::json!({
                            "page": { "type": "integer" }
                        })),
                        required: None,
                        r#type: "object".to_string(),
                    },
                    output_schema: None,
                    title: None,
                    annotations: None,
                    description: Some("Pagination".to_string()),
                },
            )])),
        )
        .build();

        assert_eq_tool_names(
            &tools,
            &[
                "unified_exec",
                "apply_patch",
                "web_search",
                "view_image",
                "dash/paginate",
            ],
        );
        assert_eq!(
            tools[4].spec,
            ToolSpec::Function(ResponsesApiTool {
                name: "dash/paginate".to_string(),
                parameters: JsonSchema::Object {
                    properties: BTreeMap::from([(
                        "page".to_string(),
                        JsonSchema::Number { description: None }
                    )]),
                    required: None,
                    additional_properties: None,
                },
                description: "Pagination".to_string(),
                strict: false,
            })
        );
    }

    #[test]
    fn test_mcp_tool_array_without_items_gets_default_string_items() {
        let model_family = find_family_for_model("gpt-5-codex")
            .expect("gpt-5-codex should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: false,
            include_apply_patch_tool: true,
            include_web_search_request: true,
            use_streamable_shell_tool: false,
            include_view_image_tool: true,
            experimental_unified_exec_tool: true,
        });

        let (tools, _) = build_specs(
            &config,
            Some(HashMap::from([(
                "dash/tags".to_string(),
                mcp_types::Tool {
                    name: "tags".to_string(),
                    input_schema: ToolInputSchema {
                        properties: Some(serde_json::json!({
                            "tags": { "type": "array" }
                        })),
                        required: None,
                        r#type: "object".to_string(),
                    },
                    output_schema: None,
                    title: None,
                    annotations: None,
                    description: Some("Tags".to_string()),
                },
            )])),
        )
        .build();

        assert_eq_tool_names(
            &tools,
            &[
                "unified_exec",
                "apply_patch",
                "web_search",
                "view_image",
                "dash/tags",
            ],
        );
        assert_eq!(
            tools[4].spec,
            ToolSpec::Function(ResponsesApiTool {
                name: "dash/tags".to_string(),
                parameters: JsonSchema::Object {
                    properties: BTreeMap::from([(
                        "tags".to_string(),
                        JsonSchema::Array {
                            items: Box::new(JsonSchema::String { description: None }),
                            description: None
                        }
                    )]),
                    required: None,
                    additional_properties: None,
                },
                description: "Tags".to_string(),
                strict: false,
            })
        );
    }

    #[test]
    fn test_mcp_tool_anyof_defaults_to_string() {
        let model_family = find_family_for_model("gpt-5-codex")
            .expect("gpt-5-codex should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: false,
            include_apply_patch_tool: false,
            include_web_search_request: true,
            use_streamable_shell_tool: false,
            include_view_image_tool: true,
            experimental_unified_exec_tool: true,
        });

        let (tools, _) = build_specs(
            &config,
            Some(HashMap::from([(
                "dash/value".to_string(),
                mcp_types::Tool {
                    name: "value".to_string(),
                    input_schema: ToolInputSchema {
                        properties: Some(serde_json::json!({
                            "value": { "anyOf": [ { "type": "string" }, { "type": "number" } ] }
                        })),
                        required: None,
                        r#type: "object".to_string(),
                    },
                    output_schema: None,
                    title: None,
                    annotations: None,
                    description: Some("AnyOf Value".to_string()),
                },
            )])),
        )
        .build();

        assert_eq_tool_names(
            &tools,
            &[
                "unified_exec",
                "apply_patch",
                "web_search",
                "view_image",
                "dash/value",
            ],
        );
        assert_eq!(
            tools[4].spec,
            ToolSpec::Function(ResponsesApiTool {
                name: "dash/value".to_string(),
                parameters: JsonSchema::Object {
                    properties: BTreeMap::from([(
                        "value".to_string(),
                        JsonSchema::String { description: None }
                    )]),
                    required: None,
                    additional_properties: None,
                },
                description: "AnyOf Value".to_string(),
                strict: false,
            })
        );
    }

    #[test]
    fn test_shell_tool() {
        let tool = super::create_shell_tool();
        let ToolSpec::Function(ResponsesApiTool {
            description, name, ..
        }) = &tool
        else {
            panic!("expected function tool");
        };
        assert_eq!(name, "shell");

        let expected = "Runs a shell command and returns its output.";
        assert_eq!(description, expected);
    }

    #[test]
    fn test_get_openai_tools_mcp_tools_with_additional_properties_schema() {
        let model_family = find_family_for_model("gpt-5-codex")
            .expect("gpt-5-codex should be a valid model family");
        let config = ToolsConfig::new(&ToolsConfigParams {
            model_family: &model_family,
            include_plan_tool: false,
            include_apply_patch_tool: false,
            include_web_search_request: true,
            use_streamable_shell_tool: false,
            include_view_image_tool: true,
            experimental_unified_exec_tool: true,
        });
        let (tools, _) = build_specs(
            &config,
            Some(HashMap::from([(
                "test_server/do_something_cool".to_string(),
                mcp_types::Tool {
                    name: "do_something_cool".to_string(),
                    input_schema: ToolInputSchema {
                        properties: Some(serde_json::json!({
                            "string_argument": {
                                "type": "string",
                            },
                            "number_argument": {
                                "type": "number",
                            },
                            "object_argument": {
                                "type": "object",
                                "properties": {
                                    "string_property": { "type": "string" },
                                    "number_property": { "type": "number" },
                                },
                                "required": [
                                    "string_property",
                                    "number_property",
                                ],
                                "additionalProperties": {
                                    "type": "object",
                                    "properties": {
                                        "addtl_prop": { "type": "string" },
                                    },
                                    "required": [
                                        "addtl_prop",
                                    ],
                                    "additionalProperties": false,
                                },
                            },
                        })),
                        required: None,
                        r#type: "object".to_string(),
                    },
                    output_schema: None,
                    title: None,
                    annotations: None,
                    description: Some("Do something cool".to_string()),
                },
            )])),
        )
        .build();

        assert_eq_tool_names(
            &tools,
            &[
                "unified_exec",
                "apply_patch",
                "web_search",
                "view_image",
                "test_server/do_something_cool",
            ],
        );

        assert_eq!(
            tools[4].spec,
            ToolSpec::Function(ResponsesApiTool {
                name: "test_server/do_something_cool".to_string(),
                parameters: JsonSchema::Object {
                    properties: BTreeMap::from([
                        (
                            "string_argument".to_string(),
                            JsonSchema::String { description: None }
                        ),
                        (
                            "number_argument".to_string(),
                            JsonSchema::Number { description: None }
                        ),
                        (
                            "object_argument".to_string(),
                            JsonSchema::Object {
                                properties: BTreeMap::from([
                                    (
                                        "string_property".to_string(),
                                        JsonSchema::String { description: None }
                                    ),
                                    (
                                        "number_property".to_string(),
                                        JsonSchema::Number { description: None }
                                    ),
                                ]),
                                required: Some(vec![
                                    "string_property".to_string(),
                                    "number_property".to_string(),
                                ]),
                                additional_properties: Some(
                                    JsonSchema::Object {
                                        properties: BTreeMap::from([(
                                            "addtl_prop".to_string(),
                                            JsonSchema::String { description: None }
                                        ),]),
                                        required: Some(vec!["addtl_prop".to_string(),]),
                                        additional_properties: Some(false.into()),
                                    }
                                    .into()
                                ),
                            },
                        ),
                    ]),
                    required: None,
                    additional_properties: None,
                },
                description: "Do something cool".to_string(),
                strict: false,
            })
        );
    }
}
