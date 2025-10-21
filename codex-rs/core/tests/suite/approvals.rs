#![allow(clippy::unwrap_used, clippy::expect_used)]

use anyhow::Result;
use codex_core::model_family::find_family_for_model;
use codex_core::protocol::ApplyPatchApprovalRequestEvent;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ExecApprovalRequestEvent;
use codex_core::protocol::Op;
use codex_core::protocol::SandboxPolicy;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_apply_patch_function_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Clone, Copy)]
enum TargetPath {
    Workspace(&'static str),
    OutsideWorkspace(&'static str),
}

impl TargetPath {
    fn resolve_for_patch(self, test: &TestCodex) -> (PathBuf, String) {
        match self {
            TargetPath::Workspace(name) => {
                let path = test.cwd.path().join(name);
                (path, name.to_string())
            }
            TargetPath::OutsideWorkspace(name) => {
                let path = env::current_dir()
                    .expect("current dir should be available")
                    .join(name);
                (path.clone(), path.display().to_string())
            }
        }
    }
}

#[derive(Clone)]
enum ActionKind {
    WriteFile {
        target: TargetPath,
        content: &'static str,
    },
    FetchUrl {
        endpoint: &'static str,
        response_body: &'static str,
    },
    RunCommand {
        command: &'static [&'static str],
    },
    ApplyPatchFunction {
        target: TargetPath,
        content: &'static str,
    },
    ApplyPatchShell {
        target: TargetPath,
        content: &'static str,
    },
}

impl ActionKind {
    async fn prepare(
        &self,
        test: &TestCodex,
        server: &MockServer,
        call_id: &str,
        with_escalated_permissions: bool,
    ) -> Result<(Value, Option<Vec<String>>)> {
        match self {
            ActionKind::WriteFile { target, content } => {
                let (path, _) = target.resolve_for_patch(test);
                let _ = fs::remove_file(&path);
                let command = vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "printf {content:?} > {path:?} && cat {path:?}",
                        content = content,
                        path = path
                    ),
                ];
                let event = shell_event(call_id, &command, 1_000, with_escalated_permissions)?;
                Ok((event, Some(command)))
            }
            ActionKind::FetchUrl {
                endpoint,
                response_body,
            } => {
                Mock::given(method("GET"))
                    .and(path(*endpoint))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_string(response_body.to_string()),
                    )
                    .mount(server)
                    .await;

                let url = format!("{}{}", server.uri(), endpoint);
                let script = format!(
                    "import sys\nimport urllib.request\nurl = {url:?}\ntry:\n    data = urllib.request.urlopen(url, timeout=2).read().decode()\n    print('OK:' + data.strip())\nexcept Exception as exc:\n    print('ERR:' + exc.__class__.__name__)\n    sys.exit(1)",
                );

                let command = vec!["python3".to_string(), "-c".to_string(), script];
                let event = shell_event(call_id, &command, 1_000, with_escalated_permissions)?;
                Ok((event, Some(command)))
            }
            ActionKind::RunCommand { command } => {
                let command: Vec<String> = command
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect();
                let event = shell_event(call_id, &command, 1_000, with_escalated_permissions)?;
                Ok((event, Some(command)))
            }
            ActionKind::ApplyPatchFunction { target, content } => {
                let (path, patch_path) = target.resolve_for_patch(test);
                let _ = fs::remove_file(&path);
                let patch = build_add_file_patch(&patch_path, content);
                Ok((ev_apply_patch_function_call(call_id, &patch), None))
            }
            ActionKind::ApplyPatchShell { target, content } => {
                let (path, patch_path) = target.resolve_for_patch(test);
                let _ = fs::remove_file(&path);
                let patch = build_add_file_patch(&patch_path, content);
                let command = shell_apply_patch_command(&patch);
                let event = shell_event(call_id, &command, 5_000, with_escalated_permissions)?;
                Ok((event, Some(command)))
            }
        }
    }
}

fn build_add_file_patch(patch_path: &str, content: &str) -> String {
    format!("*** Begin Patch\n*** Add File: {patch_path}\n+{content}\n*** End Patch\n")
}

fn shell_apply_patch_command(patch: &str) -> Vec<String> {
    let mut script = String::from("apply_patch <<'PATCH'\n");
    script.push_str(patch);
    if !patch.ends_with('\n') {
        script.push('\n');
    }
    script.push_str("PATCH\n");
    vec!["bash".to_string(), "-lc".to_string(), script]
}

fn shell_event(
    call_id: &str,
    command: &[String],
    timeout_ms: u64,
    with_escalated_permissions: bool,
) -> Result<Value> {
    let mut args = json!({
        "command": command,
        "timeout_ms": timeout_ms,
    });
    if with_escalated_permissions {
        args["with_escalated_permissions"] = json!(true);
    }
    let args_str = serde_json::to_string(&args)?;
    Ok(ev_function_call(call_id, "shell", &args_str))
}

#[derive(Clone)]
enum Expectation {
    FileCreated {
        target: TargetPath,
        content: &'static str,
    },
    PatchApplied {
        target: TargetPath,
        content: &'static str,
    },
    FileNotCreated {
        target: TargetPath,
        message_contains: &'static [&'static str],
    },
    NetworkSuccess {
        body_contains: &'static str,
    },
    NetworkFailure {
        expect_tag: &'static str,
    },
    CommandSuccess {
        stdout_contains: &'static str,
    },
}

impl Expectation {
    fn verify(&self, test: &TestCodex, result: &CommandResult) -> Result<()> {
        match self {
            Expectation::FileCreated { target, content } => {
                let (path, _) = target.resolve_for_patch(test);
                assert_eq!(
                    result.exit_code,
                    Some(0),
                    "expected successful exit for {:?}",
                    path
                );
                assert!(
                    result.stdout.contains(content),
                    "stdout missing {content:?}: {}",
                    result.stdout
                );
                let file_contents = fs::read_to_string(&path)?;
                assert!(
                    file_contents.contains(content),
                    "file contents missing {content:?}: {file_contents}"
                );
                let _ = fs::remove_file(path);
            }
            Expectation::PatchApplied { target, content } => {
                let (path, _) = target.resolve_for_patch(test);
                match result.exit_code {
                    Some(0) | None => {
                        if result.exit_code.is_none() {
                            assert!(
                                result.stdout.contains("Success."),
                                "patch output missing success indicator: {}",
                                result.stdout
                            );
                        }
                    }
                    Some(code) => panic!(
                        "expected successful patch exit for {:?}, got {code} with stdout {}",
                        path, result.stdout
                    ),
                }
                let file_contents = fs::read_to_string(&path)?;
                assert!(
                    file_contents.contains(content),
                    "patched file missing {content:?}: {file_contents}"
                );
                let _ = fs::remove_file(path);
            }
            Expectation::FileNotCreated {
                target,
                message_contains,
            } => {
                let (path, _) = target.resolve_for_patch(test);
                assert_ne!(
                    result.exit_code,
                    Some(0),
                    "expected non-zero exit for {path:?}"
                );
                for needle in *message_contains {
                    assert!(
                        result.stdout.contains(needle),
                        "stdout missing {needle:?}: {}",
                        result.stdout
                    );
                }
                assert!(
                    !path.exists(),
                    "command should not create {path:?}, but file exists"
                );
            }
            Expectation::NetworkSuccess { body_contains } => {
                assert_eq!(
                    result.exit_code,
                    Some(0),
                    "expected successful network exit: {}",
                    result.stdout
                );
                assert!(
                    result.stdout.contains("OK:"),
                    "stdout missing OK prefix: {}",
                    result.stdout
                );
                assert!(
                    result.stdout.contains(body_contains),
                    "stdout missing body text {body_contains:?}: {}",
                    result.stdout
                );
            }
            Expectation::NetworkFailure { expect_tag } => {
                assert_ne!(
                    result.exit_code,
                    Some(0),
                    "expected non-zero exit for network failure: {}",
                    result.stdout
                );
                assert!(
                    result.stdout.contains("ERR:"),
                    "stdout missing ERR prefix: {}",
                    result.stdout
                );
                assert!(
                    result.stdout.contains(expect_tag),
                    "stdout missing expected tag {expect_tag:?}: {}",
                    result.stdout
                );
            }
            Expectation::CommandSuccess { stdout_contains } => {
                assert_eq!(
                    result.exit_code,
                    Some(0),
                    "expected successful trusted command exit: {}",
                    result.stdout
                );
                assert!(
                    result.stdout.contains(stdout_contains),
                    "trusted command stdout missing {stdout_contains:?}: {}",
                    result.stdout
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
enum Outcome {
    Auto,
    ExecApproval {
        decision: ReviewDecision,
        expected_reason: Option<&'static str>,
    },
    PatchApproval {
        decision: ReviewDecision,
        expected_reason: Option<&'static str>,
    },
}

#[derive(Clone)]
struct ScenarioSpec {
    name: &'static str,
    approval_policy: AskForApproval,
    sandbox_policy: SandboxPolicy,
    action: ActionKind,
    with_escalated_permissions: bool,
    requires_apply_patch_tool: bool,
    model_override: Option<&'static str>,
    outcome: Outcome,
    expectation: Expectation,
}

struct CommandResult {
    exit_code: Option<i64>,
    stdout: String,
}

async fn submit_turn(
    test: &TestCodex,
    prompt: &str,
    approval_policy: AskForApproval,
    sandbox_policy: SandboxPolicy,
) -> Result<()> {
    let session_model = test.session_configured.model.clone();

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: prompt.into(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd.path().to_path_buf(),
            approval_policy,
            sandbox_policy,
            model: session_model,
            effort: None,
            summary: ReasoningSummary::Auto,
        })
        .await?;

    Ok(())
}

fn parse_result(item: &Value) -> CommandResult {
    let output_str = item
        .get("output")
        .and_then(Value::as_str)
        .expect("shell output payload");
    match serde_json::from_str::<Value>(output_str) {
        Ok(parsed) => {
            let exit_code = parsed["metadata"]["exit_code"].as_i64();
            let stdout = parsed["output"].as_str().unwrap_or_default().to_string();
            CommandResult { exit_code, stdout }
        }
        Err(_) => CommandResult {
            exit_code: None,
            stdout: output_str.to_string(),
        },
    }
}

async fn expect_exec_approval(
    test: &TestCodex,
    expected_command: &[String],
) -> ExecApprovalRequestEvent {
    let event = wait_for_event_with_timeout(
        &test.codex,
        |event| {
            matches!(
                event,
                EventMsg::ExecApprovalRequest(_) | EventMsg::TaskComplete(_)
            )
        },
        Duration::from_secs(5),
    )
    .await;

    match event {
        EventMsg::ExecApprovalRequest(approval) => {
            assert_eq!(approval.command, expected_command);
            approval
        }
        EventMsg::TaskComplete(_) => panic!("expected approval request before completion"),
        other => panic!("unexpected event: {other:?}"),
    }
}

async fn expect_patch_approval(
    test: &TestCodex,
    expected_call_id: &str,
) -> ApplyPatchApprovalRequestEvent {
    let event = wait_for_event_with_timeout(
        &test.codex,
        |event| {
            matches!(
                event,
                EventMsg::ApplyPatchApprovalRequest(_) | EventMsg::TaskComplete(_)
            )
        },
        Duration::from_secs(5),
    )
    .await;

    match event {
        EventMsg::ApplyPatchApprovalRequest(approval) => {
            assert_eq!(approval.call_id, expected_call_id);
            approval
        }
        EventMsg::TaskComplete(_) => panic!("expected patch approval request before completion"),
        other => panic!("unexpected event: {other:?}"),
    }
}

async fn wait_for_completion_without_approval(test: &TestCodex) {
    let event = wait_for_event_with_timeout(
        &test.codex,
        |event| {
            matches!(
                event,
                EventMsg::ExecApprovalRequest(_) | EventMsg::TaskComplete(_)
            )
        },
        Duration::from_secs(5),
    )
    .await;

    match event {
        EventMsg::TaskComplete(_) => {}
        EventMsg::ExecApprovalRequest(event) => {
            panic!("unexpected approval request: {:?}", event.command)
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

async fn wait_for_completion(test: &TestCodex) {
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TaskComplete(_))
    })
    .await;
}

fn scenarios() -> Vec<ScenarioSpec> {
    use AskForApproval::*;

    let workspace_write = |network_access| SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
    };

    vec![
        ScenarioSpec {
            name: "danger_full_access_on_request_allows_outside_write",
            approval_policy: OnRequest,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            action: ActionKind::WriteFile {
                target: TargetPath::OutsideWorkspace("dfa_on_request.txt"),
                content: "danger-on-request",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::FileCreated {
                target: TargetPath::OutsideWorkspace("dfa_on_request.txt"),
                content: "danger-on-request",
            },
        },
        ScenarioSpec {
            name: "danger_full_access_on_request_allows_network",
            approval_policy: OnRequest,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            action: ActionKind::FetchUrl {
                endpoint: "/dfa/network",
                response_body: "danger-network-ok",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::NetworkSuccess {
                body_contains: "danger-network-ok",
            },
        },
        ScenarioSpec {
            name: "trusted_command_unless_trusted_runs_without_prompt",
            approval_policy: UnlessTrusted,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            action: ActionKind::RunCommand {
                command: &["echo", "trusted-unless"],
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::CommandSuccess {
                stdout_contains: "trusted-unless",
            },
        },
        ScenarioSpec {
            name: "danger_full_access_on_failure_allows_outside_write",
            approval_policy: OnFailure,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            action: ActionKind::WriteFile {
                target: TargetPath::OutsideWorkspace("dfa_on_failure.txt"),
                content: "danger-on-failure",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::FileCreated {
                target: TargetPath::OutsideWorkspace("dfa_on_failure.txt"),
                content: "danger-on-failure",
            },
        },
        ScenarioSpec {
            name: "danger_full_access_unless_trusted_requests_approval",
            approval_policy: UnlessTrusted,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            action: ActionKind::WriteFile {
                target: TargetPath::OutsideWorkspace("dfa_unless_trusted.txt"),
                content: "danger-unless-trusted",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::ExecApproval {
                decision: ReviewDecision::Approved,
                expected_reason: None,
            },
            expectation: Expectation::FileCreated {
                target: TargetPath::OutsideWorkspace("dfa_unless_trusted.txt"),
                content: "danger-unless-trusted",
            },
        },
        ScenarioSpec {
            name: "danger_full_access_never_allows_outside_write",
            approval_policy: Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            action: ActionKind::WriteFile {
                target: TargetPath::OutsideWorkspace("dfa_never.txt"),
                content: "danger-never",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::FileCreated {
                target: TargetPath::OutsideWorkspace("dfa_never.txt"),
                content: "danger-never",
            },
        },
        ScenarioSpec {
            name: "read_only_on_request_requires_approval",
            approval_policy: OnRequest,
            sandbox_policy: SandboxPolicy::ReadOnly,
            action: ActionKind::WriteFile {
                target: TargetPath::Workspace("ro_on_request.txt"),
                content: "read-only-approval",
            },
            with_escalated_permissions: true,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::ExecApproval {
                decision: ReviewDecision::Approved,
                expected_reason: None,
            },
            expectation: Expectation::FileCreated {
                target: TargetPath::Workspace("ro_on_request.txt"),
                content: "read-only-approval",
            },
        },
        ScenarioSpec {
            name: "trusted_command_on_request_read_only_runs_without_prompt",
            approval_policy: OnRequest,
            sandbox_policy: SandboxPolicy::ReadOnly,
            action: ActionKind::RunCommand {
                command: &["echo", "trusted-read-only"],
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::CommandSuccess {
                stdout_contains: "trusted-read-only",
            },
        },
        ScenarioSpec {
            name: "read_only_on_request_blocks_network",
            approval_policy: OnRequest,
            sandbox_policy: SandboxPolicy::ReadOnly,
            action: ActionKind::FetchUrl {
                endpoint: "/ro/network-blocked",
                response_body: "should-not-see",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::NetworkFailure { expect_tag: "ERR:" },
        },
        ScenarioSpec {
            name: "read_only_on_request_denied_blocks_execution",
            approval_policy: OnRequest,
            sandbox_policy: SandboxPolicy::ReadOnly,
            action: ActionKind::WriteFile {
                target: TargetPath::Workspace("ro_on_request_denied.txt"),
                content: "should-not-write",
            },
            with_escalated_permissions: true,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::ExecApproval {
                decision: ReviewDecision::Denied,
                expected_reason: None,
            },
            expectation: Expectation::FileNotCreated {
                target: TargetPath::Workspace("ro_on_request_denied.txt"),
                message_contains: &["exec command rejected by user"],
            },
        },
        #[cfg(not(target_os = "linux"))] // TODO (pakrym): figure out why linux behaves differently
        ScenarioSpec {
            name: "read_only_on_failure_escalates_after_sandbox_error",
            approval_policy: OnFailure,
            sandbox_policy: SandboxPolicy::ReadOnly,
            action: ActionKind::WriteFile {
                target: TargetPath::Workspace("ro_on_failure.txt"),
                content: "read-only-on-failure",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::ExecApproval {
                decision: ReviewDecision::Approved,
                expected_reason: Some("command failed; retry without sandbox?"),
            },
            expectation: Expectation::FileCreated {
                target: TargetPath::Workspace("ro_on_failure.txt"),
                content: "read-only-on-failure",
            },
        },
        ScenarioSpec {
            name: "read_only_on_request_network_escalates_when_approved",
            approval_policy: OnRequest,
            sandbox_policy: SandboxPolicy::ReadOnly,
            action: ActionKind::FetchUrl {
                endpoint: "/ro/network-approved",
                response_body: "read-only-network-ok",
            },
            with_escalated_permissions: true,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::ExecApproval {
                decision: ReviewDecision::Approved,
                expected_reason: None,
            },
            expectation: Expectation::NetworkSuccess {
                body_contains: "read-only-network-ok",
            },
        },
        ScenarioSpec {
            name: "apply_patch_shell_requires_patch_approval",
            approval_policy: UnlessTrusted,
            sandbox_policy: workspace_write(false),
            action: ActionKind::ApplyPatchShell {
                target: TargetPath::Workspace("apply_patch_shell.txt"),
                content: "shell-apply-patch",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: true,
            model_override: None,
            outcome: Outcome::PatchApproval {
                decision: ReviewDecision::Approved,
                expected_reason: None,
            },
            expectation: Expectation::PatchApplied {
                target: TargetPath::Workspace("apply_patch_shell.txt"),
                content: "shell-apply-patch",
            },
        },
        ScenarioSpec {
            name: "apply_patch_function_auto_inside_workspace",
            approval_policy: OnRequest,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            action: ActionKind::ApplyPatchFunction {
                target: TargetPath::Workspace("apply_patch_function.txt"),
                content: "function-apply-patch",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: true,
            model_override: Some("gpt-5-codex"),
            outcome: Outcome::Auto,
            expectation: Expectation::PatchApplied {
                target: TargetPath::Workspace("apply_patch_function.txt"),
                content: "function-apply-patch",
            },
        },
        ScenarioSpec {
            name: "apply_patch_function_danger_allows_outside_workspace",
            approval_policy: OnRequest,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            action: ActionKind::ApplyPatchFunction {
                target: TargetPath::OutsideWorkspace("apply_patch_function_danger.txt"),
                content: "function-patch-danger",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: true,
            model_override: Some("gpt-5-codex"),
            outcome: Outcome::Auto,
            expectation: Expectation::PatchApplied {
                target: TargetPath::OutsideWorkspace("apply_patch_function_danger.txt"),
                content: "function-patch-danger",
            },
        },
        ScenarioSpec {
            name: "apply_patch_function_outside_requires_patch_approval",
            approval_policy: OnRequest,
            sandbox_policy: workspace_write(false),
            action: ActionKind::ApplyPatchFunction {
                target: TargetPath::OutsideWorkspace("apply_patch_function_outside.txt"),
                content: "function-patch-outside",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: true,
            model_override: Some("gpt-5-codex"),
            outcome: Outcome::PatchApproval {
                decision: ReviewDecision::Approved,
                expected_reason: None,
            },
            expectation: Expectation::PatchApplied {
                target: TargetPath::OutsideWorkspace("apply_patch_function_outside.txt"),
                content: "function-patch-outside",
            },
        },
        ScenarioSpec {
            name: "apply_patch_function_outside_denied_blocks_patch",
            approval_policy: OnRequest,
            sandbox_policy: workspace_write(false),
            action: ActionKind::ApplyPatchFunction {
                target: TargetPath::OutsideWorkspace("apply_patch_function_outside_denied.txt"),
                content: "function-patch-outside-denied",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: true,
            model_override: Some("gpt-5-codex"),
            outcome: Outcome::PatchApproval {
                decision: ReviewDecision::Denied,
                expected_reason: None,
            },
            expectation: Expectation::FileNotCreated {
                target: TargetPath::OutsideWorkspace("apply_patch_function_outside_denied.txt"),
                message_contains: &["patch rejected by user"],
            },
        },
        ScenarioSpec {
            name: "apply_patch_shell_outside_requires_patch_approval",
            approval_policy: OnRequest,
            sandbox_policy: workspace_write(false),
            action: ActionKind::ApplyPatchShell {
                target: TargetPath::OutsideWorkspace("apply_patch_shell_outside.txt"),
                content: "shell-patch-outside",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: true,
            model_override: None,
            outcome: Outcome::PatchApproval {
                decision: ReviewDecision::Approved,
                expected_reason: None,
            },
            expectation: Expectation::PatchApplied {
                target: TargetPath::OutsideWorkspace("apply_patch_shell_outside.txt"),
                content: "shell-patch-outside",
            },
        },
        ScenarioSpec {
            name: "apply_patch_function_unless_trusted_requires_patch_approval",
            approval_policy: UnlessTrusted,
            sandbox_policy: workspace_write(false),
            action: ActionKind::ApplyPatchFunction {
                target: TargetPath::Workspace("apply_patch_function_unless_trusted.txt"),
                content: "function-patch-unless-trusted",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: true,
            model_override: Some("gpt-5-codex"),
            outcome: Outcome::PatchApproval {
                decision: ReviewDecision::Approved,
                expected_reason: None,
            },
            expectation: Expectation::PatchApplied {
                target: TargetPath::Workspace("apply_patch_function_unless_trusted.txt"),
                content: "function-patch-unless-trusted",
            },
        },
        ScenarioSpec {
            name: "apply_patch_function_never_rejects_outside_workspace",
            approval_policy: Never,
            sandbox_policy: workspace_write(false),
            action: ActionKind::ApplyPatchFunction {
                target: TargetPath::OutsideWorkspace("apply_patch_function_never.txt"),
                content: "function-patch-never",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: true,
            model_override: Some("gpt-5-codex"),
            outcome: Outcome::Auto,
            expectation: Expectation::FileNotCreated {
                target: TargetPath::OutsideWorkspace("apply_patch_function_never.txt"),
                message_contains: &[
                    "patch rejected: writing outside of the project; rejected by user approval settings",
                ],
            },
        },
        ScenarioSpec {
            name: "read_only_unless_trusted_requires_approval",
            approval_policy: UnlessTrusted,
            sandbox_policy: SandboxPolicy::ReadOnly,
            action: ActionKind::WriteFile {
                target: TargetPath::Workspace("ro_unless_trusted.txt"),
                content: "read-only-unless-trusted",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::ExecApproval {
                decision: ReviewDecision::Approved,
                expected_reason: None,
            },
            expectation: Expectation::FileCreated {
                target: TargetPath::Workspace("ro_unless_trusted.txt"),
                content: "read-only-unless-trusted",
            },
        },
        ScenarioSpec {
            name: "read_only_never_reports_sandbox_failure",
            approval_policy: Never,
            sandbox_policy: SandboxPolicy::ReadOnly,
            action: ActionKind::WriteFile {
                target: TargetPath::Workspace("ro_never.txt"),
                content: "read-only-never",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::FileNotCreated {
                target: TargetPath::Workspace("ro_never.txt"),
                message_contains: if cfg!(target_os = "linux") {
                    &["Permission denied"]
                } else {
                    &["failed in sandbox"]
                },
            },
        },
        ScenarioSpec {
            name: "trusted_command_never_runs_without_prompt",
            approval_policy: Never,
            sandbox_policy: SandboxPolicy::ReadOnly,
            action: ActionKind::RunCommand {
                command: &["echo", "trusted-never"],
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::CommandSuccess {
                stdout_contains: "trusted-never",
            },
        },
        ScenarioSpec {
            name: "workspace_write_on_request_allows_workspace_write",
            approval_policy: OnRequest,
            sandbox_policy: workspace_write(false),
            action: ActionKind::WriteFile {
                target: TargetPath::Workspace("ww_on_request.txt"),
                content: "workspace-on-request",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::FileCreated {
                target: TargetPath::Workspace("ww_on_request.txt"),
                content: "workspace-on-request",
            },
        },
        ScenarioSpec {
            name: "workspace_write_network_disabled_blocks_network",
            approval_policy: OnRequest,
            sandbox_policy: workspace_write(false),
            action: ActionKind::FetchUrl {
                endpoint: "/ww/network-blocked",
                response_body: "workspace-network-blocked",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::NetworkFailure { expect_tag: "ERR:" },
        },
        ScenarioSpec {
            name: "workspace_write_on_request_requires_approval_outside_workspace",
            approval_policy: OnRequest,
            sandbox_policy: workspace_write(false),
            action: ActionKind::WriteFile {
                target: TargetPath::OutsideWorkspace("ww_on_request_outside.txt"),
                content: "workspace-on-request-outside",
            },
            with_escalated_permissions: true,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::ExecApproval {
                decision: ReviewDecision::Approved,
                expected_reason: None,
            },
            expectation: Expectation::FileCreated {
                target: TargetPath::OutsideWorkspace("ww_on_request_outside.txt"),
                content: "workspace-on-request-outside",
            },
        },
        ScenarioSpec {
            name: "workspace_write_network_enabled_allows_network",
            approval_policy: OnRequest,
            sandbox_policy: workspace_write(true),
            action: ActionKind::FetchUrl {
                endpoint: "/ww/network-ok",
                response_body: "workspace-network-ok",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::NetworkSuccess {
                body_contains: "workspace-network-ok",
            },
        },
        #[cfg(not(target_os = "linux"))] // TODO (pakrym): figure out why linux behaves differently
        ScenarioSpec {
            name: "workspace_write_on_failure_escalates_outside_workspace",
            approval_policy: OnFailure,
            sandbox_policy: workspace_write(false),
            action: ActionKind::WriteFile {
                target: TargetPath::OutsideWorkspace("ww_on_failure.txt"),
                content: "workspace-on-failure",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::ExecApproval {
                decision: ReviewDecision::Approved,
                expected_reason: Some("command failed; retry without sandbox?"),
            },
            expectation: Expectation::FileCreated {
                target: TargetPath::OutsideWorkspace("ww_on_failure.txt"),
                content: "workspace-on-failure",
            },
        },
        ScenarioSpec {
            name: "workspace_write_unless_trusted_requires_approval_outside_workspace",
            approval_policy: UnlessTrusted,
            sandbox_policy: workspace_write(false),
            action: ActionKind::WriteFile {
                target: TargetPath::OutsideWorkspace("ww_unless_trusted.txt"),
                content: "workspace-unless-trusted",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::ExecApproval {
                decision: ReviewDecision::Approved,
                expected_reason: None,
            },
            expectation: Expectation::FileCreated {
                target: TargetPath::OutsideWorkspace("ww_unless_trusted.txt"),
                content: "workspace-unless-trusted",
            },
        },
        ScenarioSpec {
            name: "workspace_write_never_blocks_outside_workspace",
            approval_policy: Never,
            sandbox_policy: workspace_write(false),
            action: ActionKind::WriteFile {
                target: TargetPath::OutsideWorkspace("ww_never.txt"),
                content: "workspace-never",
            },
            with_escalated_permissions: false,
            requires_apply_patch_tool: false,
            model_override: None,
            outcome: Outcome::Auto,
            expectation: Expectation::FileNotCreated {
                target: TargetPath::OutsideWorkspace("ww_never.txt"),
                message_contains: if cfg!(target_os = "linux") {
                    &["Permission denied"]
                } else {
                    &["failed in sandbox"]
                },
            },
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_matrix_covers_all_modes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    for scenario in scenarios() {
        run_scenario(&scenario).await?;
    }

    Ok(())
}

async fn run_scenario(scenario: &ScenarioSpec) -> Result<()> {
    eprintln!("running approval scenario: {}", scenario.name);
    let server = start_mock_server().await;
    let approval_policy = scenario.approval_policy;
    let sandbox_policy = scenario.sandbox_policy.clone();
    let requires_apply_patch_tool = scenario.requires_apply_patch_tool;
    let model_override = scenario.model_override;

    let mut builder = test_codex().with_config(move |config| {
        config.approval_policy = approval_policy;
        config.sandbox_policy = sandbox_policy.clone();
        let model = model_override.unwrap_or("gpt-5");
        config.model = model.to_string();
        config.model_family =
            find_family_for_model(model).expect("model should map to a known family");
        if requires_apply_patch_tool {
            config.include_apply_patch_tool = true;
        }
    });
    let test = builder.build(&server).await?;

    let call_id = scenario.name;
    let (event, expected_command) = scenario
        .action
        .prepare(&test, &server, call_id, scenario.with_escalated_permissions)
        .await?;

    let _ = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            event,
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let results_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    submit_turn(
        &test,
        scenario.name,
        scenario.approval_policy,
        scenario.sandbox_policy.clone(),
    )
    .await?;

    match &scenario.outcome {
        Outcome::Auto => {
            wait_for_completion_without_approval(&test).await;
        }
        Outcome::ExecApproval {
            decision,
            expected_reason,
        } => {
            let command = expected_command
                .as_ref()
                .expect("exec approval requires shell command");
            let approval = expect_exec_approval(&test, command).await;
            if let Some(expected_reason) = expected_reason {
                assert_eq!(
                    approval.reason.as_deref(),
                    Some(*expected_reason),
                    "unexpected approval reason for {}",
                    scenario.name
                );
            }
            test.codex
                .submit(Op::ExecApproval {
                    id: "0".into(),
                    decision: *decision,
                })
                .await?;
            wait_for_completion(&test).await;
        }
        Outcome::PatchApproval {
            decision,
            expected_reason,
        } => {
            let approval = expect_patch_approval(&test, call_id).await;
            if let Some(expected_reason) = expected_reason {
                assert_eq!(
                    approval.reason.as_deref(),
                    Some(*expected_reason),
                    "unexpected patch approval reason for {}",
                    scenario.name
                );
            }
            test.codex
                .submit(Op::PatchApproval {
                    id: "0".into(),
                    decision: *decision,
                })
                .await?;
            wait_for_completion(&test).await;
        }
    }

    let output_item = results_mock.single_request().function_call_output(call_id);
    let result = parse_result(&output_item);
    scenario.expectation.verify(&test, &result)?;

    Ok(())
}
