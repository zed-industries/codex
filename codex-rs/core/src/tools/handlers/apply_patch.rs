use std::collections::BTreeMap;

use crate::apply_patch;
use crate::apply_patch::InternalApplyPatchInvocation;
use crate::apply_patch::convert_apply_patch_to_protocol;
use crate::client_common::tools::FreeformTool;
use crate::client_common::tools::FreeformToolFormat;
use crate::client_common::tools::ResponsesApiTool;
use crate::client_common::tools::ToolSpec;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::runtimes::apply_patch::ApplyPatchRequest;
use crate::tools::runtimes::apply_patch::ApplyPatchRuntime;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::spec::ApplyPatchToolArgs;
use crate::tools::spec::JsonSchema;
use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;

pub struct ApplyPatchHandler;

const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("tool_apply_patch.lark");

#[async_trait]
impl ToolHandler for ApplyPatchHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            payload,
            ToolPayload::Function { .. } | ToolPayload::Custom { .. }
        )
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
        } = invocation;

        let patch_input = match payload {
            ToolPayload::Function { arguments } => {
                let args: ApplyPatchToolArgs = serde_json::from_str(&arguments).map_err(|e| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to parse function arguments: {e:?}"
                    ))
                })?;
                args.input
            }
            ToolPayload::Custom { input } => input,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "apply_patch handler received unsupported payload".to_string(),
                ));
            }
        };

        // Re-parse and verify the patch so we can compute changes and approval.
        // Avoid building temporary ExecParams/command vectors; derive directly from inputs.
        let cwd = turn.cwd.clone();
        let command = vec!["apply_patch".to_string(), patch_input.clone()];
        match codex_apply_patch::maybe_parse_apply_patch_verified(
            &command,
            &cwd,
            session.fs.as_ref(),
        ) {
            codex_apply_patch::MaybeApplyPatchVerified::Body(changes) => {
                match apply_patch::apply_patch(session.as_ref(), turn.as_ref(), &call_id, changes)
                    .await
                {
                    InternalApplyPatchInvocation::Output(item) => {
                        let content = item?;
                        Ok(ToolOutput::Function {
                            content,
                            success: Some(true),
                        })
                    }
                    InternalApplyPatchInvocation::DelegateToExec(apply) => {
                        let emitter = ToolEmitter::apply_patch(
                            convert_apply_patch_to_protocol(&apply.action),
                            !apply.user_explicitly_approved_this_action,
                        );
                        let event_ctx = ToolEventCtx::new(
                            session.as_ref(),
                            turn.as_ref(),
                            &call_id,
                            Some(&tracker),
                        );
                        emitter.begin(event_ctx).await;

                        let req = ApplyPatchRequest {
                            patch: apply.action.patch.clone(),
                            cwd: apply.action.cwd.clone(),
                            timeout_ms: None,
                            user_explicitly_approved: apply.user_explicitly_approved_this_action,
                            codex_exe: turn.codex_linux_sandbox_exe.clone(),
                        };

                        let mut _orchestrator = ToolOrchestrator::new();
                        let mut _runtime = ApplyPatchRuntime::new();
                        let _tool_ctx = ToolCtx {
                            session: session.as_ref(),
                            turn: turn.as_ref(),
                            call_id: call_id.clone(),
                            tool_name: tool_name.to_string(),
                        };
                        // let out = orchestrator
                        //     .run(&mut runtime, &req, &tool_ctx, &turn, turn.approval_policy)
                        //     .await;
                        let out = process_apply_patch(&req.patch, session.as_ref());
                        let event_ctx = ToolEventCtx::new(
                            session.as_ref(),
                            turn.as_ref(),
                            &call_id,
                            Some(&tracker),
                        );
                        let content = emitter.finish(event_ctx, out).await?;
                        Ok(ToolOutput::Function {
                            content,
                            success: Some(true),
                        })
                    }
                }
            }
            codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
                Err(FunctionCallError::RespondToModel(format!(
                    "apply_patch verification failed: {parse_error}"
                )))
            }
            codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
                tracing::trace!("Failed to parse apply_patch input, {error:?}");
                Err(FunctionCallError::RespondToModel(
                    "apply_patch handler received invalid patch input".to_string(),
                ))
            }
            codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => {
                Err(FunctionCallError::RespondToModel(
                    "apply_patch handler received non-apply_patch input".to_string(),
                ))
            }
        }
    }
}

fn process_apply_patch(
    patch: &str,
    session: &crate::codex::Session,
) -> Result<crate::tools::ExecToolCallOutput, crate::tools::sandboxing::ToolError> {
    use crate::exec::StreamOutput;

    let start = std::time::Instant::now();

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit_code = match codex_apply_patch::apply_patch(
        patch,
        &mut stdout,
        &mut stderr,
        session.fs.as_ref(),
    ) {
        Ok(()) => 0,
        Err(_) => 1,
    };
    let duration = start.elapsed();

    let stdout = StreamOutput::new(String::from_utf8_lossy(&stdout).to_string());
    let stderr = StreamOutput::new(String::from_utf8_lossy(&stderr).to_string());
    let aggregated_output =
        StreamOutput::new([stdout.text.as_str(), stderr.text.as_str()].join(""));
    let exec_output = crate::tools::ExecToolCallOutput {
        exit_code,
        stdout,
        stderr,
        aggregated_output,
        duration,
        timed_out: false,
    };

    Ok(exec_output)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ApplyPatchToolType {
    Freeform,
    Function,
}

/// Returns a custom tool that can be used to edit files. Well-suited for GPT-5 models
/// https://platform.openai.com/docs/guides/function-calling#custom-tools
pub(crate) fn create_apply_patch_freeform_tool() -> ToolSpec {
    ToolSpec::Freeform(FreeformTool {
        name: "apply_patch".to_string(),
        description: "Use the `apply_patch` tool to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.".to_string(),
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition: APPLY_PATCH_LARK_GRAMMAR.to_string(),
        },
    })
}

/// Returns a json tool that can be used to edit files. Should only be used with gpt-oss models
pub(crate) fn create_apply_patch_json_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "input".to_string(),
        JsonSchema::String {
            description: Some(r#"The entire contents of the apply_patch command"#.to_string()),
        },
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "apply_patch".to_string(),
        description: r#"Use the `apply_patch` tool to edit files.
Your patch language is a stripped‑down, file‑oriented diff format designed to be easy to parse and safe to apply. You can think of it as a high‑level envelope:

*** Begin Patch
[ one or more file sections ]
*** End Patch

Within that envelope, you get a sequence of file operations.
You MUST include a header to specify the action you are taking.
Each operation starts with one of three headers:

*** Add File: <path> - create a new file. Every following line is a + line (the initial contents).
*** Delete File: <path> - remove an existing file. Nothing follows.
*** Update File: <path> - patch an existing file in place (optionally with a rename).

May be immediately followed by *** Move to: <new path> if you want to rename the file.
Then one or more “hunks”, each introduced by @@ (optionally followed by a hunk header).
Within a hunk each line starts with:

For instructions on [context_before] and [context_after]:
- By default, show 3 lines of code immediately above and 3 lines immediately below each change. If a change is within 3 lines of a previous change, do NOT duplicate the first change’s [context_after] lines in the second change’s [context_before] lines.
- If 3 lines of context is insufficient to uniquely identify the snippet of code within the file, use the @@ operator to indicate the class or function to which the snippet belongs. For instance, we might have:
@@ class BaseClass
[3 lines of pre-context]
- [old_code]
+ [new_code]
[3 lines of post-context]

- If a code block is repeated so many times in a class or function such that even a single `@@` statement and 3 lines of context cannot uniquely identify the snippet of code, you can use multiple `@@` statements to jump to the right context. For instance:

@@ class BaseClass
@@ 	 def method():
[3 lines of pre-context]
- [old_code]
+ [new_code]
[3 lines of post-context]

The full grammar definition is below:
Patch := Begin { FileOp } End
Begin := "*** Begin Patch" NEWLINE
End := "*** End Patch" NEWLINE
FileOp := AddFile | DeleteFile | UpdateFile
AddFile := "*** Add File: " path NEWLINE { "+" line NEWLINE }
DeleteFile := "*** Delete File: " path NEWLINE
UpdateFile := "*** Update File: " path NEWLINE [ MoveTo ] { Hunk }
MoveTo := "*** Move to: " newPath NEWLINE
Hunk := "@@" [ header ] NEWLINE { HunkLine } [ "*** End of File" NEWLINE ]
HunkLine := (" " | "-" | "+") text NEWLINE

A full patch can combine several operations:

*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch

It is important to remember:

- You must include a header with your intended action (Add/Delete/Update)
- You must prefix new lines with `+` even when creating a new file
- File references can only be relative, NEVER ABSOLUTE.
"#
            .to_string(),
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["input".to_string()]),
            additional_properties: Some(false.into()),
        },
    })
}
