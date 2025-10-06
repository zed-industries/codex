use codex_core::protocol::AgentMessageEvent;
use codex_core::protocol::AgentReasoningEvent;
use codex_core::protocol::ErrorEvent;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ExecCommandBeginEvent;
use codex_core::protocol::ExecCommandEndEvent;
use codex_core::protocol::FileChange;
use codex_core::protocol::McpInvocation;
use codex_core::protocol::McpToolCallBeginEvent;
use codex_core::protocol::McpToolCallEndEvent;
use codex_core::protocol::PatchApplyBeginEvent;
use codex_core::protocol::PatchApplyEndEvent;
use codex_core::protocol::SessionConfiguredEvent;
use codex_core::protocol::WebSearchEndEvent;
use codex_exec::event_processor_with_jsonl_output::EventProcessorWithJsonOutput;
use codex_exec::exec_events::AgentMessageItem;
use codex_exec::exec_events::CommandExecutionItem;
use codex_exec::exec_events::CommandExecutionStatus;
use codex_exec::exec_events::ItemCompletedEvent;
use codex_exec::exec_events::ItemStartedEvent;
use codex_exec::exec_events::ItemUpdatedEvent;
use codex_exec::exec_events::McpToolCallItem;
use codex_exec::exec_events::McpToolCallStatus;
use codex_exec::exec_events::PatchApplyStatus;
use codex_exec::exec_events::PatchChangeKind;
use codex_exec::exec_events::ReasoningItem;
use codex_exec::exec_events::ThreadErrorEvent;
use codex_exec::exec_events::ThreadEvent;
use codex_exec::exec_events::ThreadItem;
use codex_exec::exec_events::ThreadItemDetails;
use codex_exec::exec_events::ThreadStartedEvent;
use codex_exec::exec_events::TodoItem as ExecTodoItem;
use codex_exec::exec_events::TodoListItem as ExecTodoListItem;
use codex_exec::exec_events::TurnCompletedEvent;
use codex_exec::exec_events::TurnFailedEvent;
use codex_exec::exec_events::TurnStartedEvent;
use codex_exec::exec_events::Usage;
use codex_exec::exec_events::WebSearchItem;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use mcp_types::CallToolResult;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::time::Duration;

fn event(id: &str, msg: EventMsg) -> Event {
    Event {
        id: id.to_string(),
        msg,
    }
}

#[test]
fn session_configured_produces_thread_started_event() {
    let mut ep = EventProcessorWithJsonOutput::new(None);
    let session_id =
        codex_protocol::ConversationId::from_string("67e55044-10b1-426f-9247-bb680e5fe0c8")
            .unwrap();
    let rollout_path = PathBuf::from("/tmp/rollout.json");
    let ev = event(
        "e1",
        EventMsg::SessionConfigured(SessionConfiguredEvent {
            session_id,
            model: "codex-mini-latest".to_string(),
            reasoning_effort: None,
            history_log_id: 0,
            history_entry_count: 0,
            initial_messages: None,
            rollout_path,
        }),
    );
    let out = ep.collect_thread_events(&ev);
    assert_eq!(
        out,
        vec![ThreadEvent::ThreadStarted(ThreadStartedEvent {
            thread_id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
        })]
    );
}

#[test]
fn task_started_produces_turn_started_event() {
    let mut ep = EventProcessorWithJsonOutput::new(None);
    let out = ep.collect_thread_events(&event(
        "t1",
        EventMsg::TaskStarted(codex_core::protocol::TaskStartedEvent {
            model_context_window: Some(32_000),
        }),
    ));

    assert_eq!(out, vec![ThreadEvent::TurnStarted(TurnStartedEvent {})]);
}

#[test]
fn web_search_end_emits_item_completed() {
    let mut ep = EventProcessorWithJsonOutput::new(None);
    let query = "rust async await".to_string();
    let out = ep.collect_thread_events(&event(
        "w1",
        EventMsg::WebSearchEnd(WebSearchEndEvent {
            call_id: "call-123".to_string(),
            query: query.clone(),
        }),
    ));

    assert_eq!(
        out,
        vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::WebSearch(WebSearchItem { query }),
            },
        })]
    );
}

#[test]
fn plan_update_emits_todo_list_started_updated_and_completed() {
    let mut ep = EventProcessorWithJsonOutput::new(None);

    // First plan update => item.started (todo_list)
    let first = event(
        "p1",
        EventMsg::PlanUpdate(UpdatePlanArgs {
            explanation: None,
            plan: vec![
                PlanItemArg {
                    step: "step one".to_string(),
                    status: StepStatus::Pending,
                },
                PlanItemArg {
                    step: "step two".to_string(),
                    status: StepStatus::InProgress,
                },
            ],
        }),
    );
    let out_first = ep.collect_thread_events(&first);
    assert_eq!(
        out_first,
        vec![ThreadEvent::ItemStarted(ItemStartedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::TodoList(ExecTodoListItem {
                    items: vec![
                        ExecTodoItem {
                            text: "step one".to_string(),
                            completed: false
                        },
                        ExecTodoItem {
                            text: "step two".to_string(),
                            completed: false
                        },
                    ],
                }),
            },
        })]
    );

    // Second plan update in same turn => item.updated (same id)
    let second = event(
        "p2",
        EventMsg::PlanUpdate(UpdatePlanArgs {
            explanation: None,
            plan: vec![
                PlanItemArg {
                    step: "step one".to_string(),
                    status: StepStatus::Completed,
                },
                PlanItemArg {
                    step: "step two".to_string(),
                    status: StepStatus::InProgress,
                },
            ],
        }),
    );
    let out_second = ep.collect_thread_events(&second);
    assert_eq!(
        out_second,
        vec![ThreadEvent::ItemUpdated(ItemUpdatedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::TodoList(ExecTodoListItem {
                    items: vec![
                        ExecTodoItem {
                            text: "step one".to_string(),
                            completed: true
                        },
                        ExecTodoItem {
                            text: "step two".to_string(),
                            completed: false
                        },
                    ],
                }),
            },
        })]
    );

    // Task completes => item.completed (same id, latest state)
    let complete = event(
        "p3",
        EventMsg::TaskComplete(codex_core::protocol::TaskCompleteEvent {
            last_agent_message: None,
        }),
    );
    let out_complete = ep.collect_thread_events(&complete);
    assert_eq!(
        out_complete,
        vec![
            ThreadEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::TodoList(ExecTodoListItem {
                        items: vec![
                            ExecTodoItem {
                                text: "step one".to_string(),
                                completed: true
                            },
                            ExecTodoItem {
                                text: "step two".to_string(),
                                completed: false
                            },
                        ],
                    }),
                },
            }),
            ThreadEvent::TurnCompleted(TurnCompletedEvent {
                usage: Usage::default(),
            }),
        ]
    );
}

#[test]
fn mcp_tool_call_begin_and_end_emit_item_events() {
    let mut ep = EventProcessorWithJsonOutput::new(None);
    let invocation = McpInvocation {
        server: "server_a".to_string(),
        tool: "tool_x".to_string(),
        arguments: None,
    };

    let begin = event(
        "m1",
        EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
            call_id: "call-1".to_string(),
            invocation: invocation.clone(),
        }),
    );
    let begin_events = ep.collect_thread_events(&begin);
    assert_eq!(
        begin_events,
        vec![ThreadEvent::ItemStarted(ItemStartedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::McpToolCall(McpToolCallItem {
                    server: "server_a".to_string(),
                    tool: "tool_x".to_string(),
                    status: McpToolCallStatus::InProgress,
                }),
            },
        })]
    );

    let end = event(
        "m2",
        EventMsg::McpToolCallEnd(McpToolCallEndEvent {
            call_id: "call-1".to_string(),
            invocation,
            duration: Duration::from_secs(1),
            result: Ok(CallToolResult {
                content: Vec::new(),
                is_error: None,
                structured_content: None,
            }),
        }),
    );
    let end_events = ep.collect_thread_events(&end);
    assert_eq!(
        end_events,
        vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::McpToolCall(McpToolCallItem {
                    server: "server_a".to_string(),
                    tool: "tool_x".to_string(),
                    status: McpToolCallStatus::Completed,
                }),
            },
        })]
    );
}

#[test]
fn mcp_tool_call_failure_sets_failed_status() {
    let mut ep = EventProcessorWithJsonOutput::new(None);
    let invocation = McpInvocation {
        server: "server_b".to_string(),
        tool: "tool_y".to_string(),
        arguments: None,
    };

    let begin = event(
        "m3",
        EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
            call_id: "call-2".to_string(),
            invocation: invocation.clone(),
        }),
    );
    ep.collect_thread_events(&begin);

    let end = event(
        "m4",
        EventMsg::McpToolCallEnd(McpToolCallEndEvent {
            call_id: "call-2".to_string(),
            invocation,
            duration: Duration::from_millis(5),
            result: Err("tool exploded".to_string()),
        }),
    );
    let events = ep.collect_thread_events(&end);
    assert_eq!(
        events,
        vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::McpToolCall(McpToolCallItem {
                    server: "server_b".to_string(),
                    tool: "tool_y".to_string(),
                    status: McpToolCallStatus::Failed,
                }),
            },
        })]
    );
}

#[test]
fn plan_update_after_complete_starts_new_todo_list_with_new_id() {
    let mut ep = EventProcessorWithJsonOutput::new(None);

    // First turn: start + complete
    let start = event(
        "t1",
        EventMsg::PlanUpdate(UpdatePlanArgs {
            explanation: None,
            plan: vec![PlanItemArg {
                step: "only".to_string(),
                status: StepStatus::Pending,
            }],
        }),
    );
    let _ = ep.collect_thread_events(&start);
    let complete = event(
        "t2",
        EventMsg::TaskComplete(codex_core::protocol::TaskCompleteEvent {
            last_agent_message: None,
        }),
    );
    let _ = ep.collect_thread_events(&complete);

    // Second turn: a new todo list should have a new id
    let start_again = event(
        "t3",
        EventMsg::PlanUpdate(UpdatePlanArgs {
            explanation: None,
            plan: vec![PlanItemArg {
                step: "again".to_string(),
                status: StepStatus::Pending,
            }],
        }),
    );
    let out = ep.collect_thread_events(&start_again);

    match &out[0] {
        ThreadEvent::ItemStarted(ItemStartedEvent { item }) => {
            assert_eq!(&item.id, "item_1");
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn agent_reasoning_produces_item_completed_reasoning() {
    let mut ep = EventProcessorWithJsonOutput::new(None);
    let ev = event(
        "e1",
        EventMsg::AgentReasoning(AgentReasoningEvent {
            text: "thinking...".to_string(),
        }),
    );
    let out = ep.collect_thread_events(&ev);
    assert_eq!(
        out,
        vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::Reasoning(ReasoningItem {
                    text: "thinking...".to_string(),
                }),
            },
        })]
    );
}

#[test]
fn agent_message_produces_item_completed_agent_message() {
    let mut ep = EventProcessorWithJsonOutput::new(None);
    let ev = event(
        "e1",
        EventMsg::AgentMessage(AgentMessageEvent {
            message: "hello".to_string(),
        }),
    );
    let out = ep.collect_thread_events(&ev);
    assert_eq!(
        out,
        vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::AgentMessage(AgentMessageItem {
                    text: "hello".to_string(),
                }),
            },
        })]
    );
}

#[test]
fn error_event_produces_error() {
    let mut ep = EventProcessorWithJsonOutput::new(None);
    let out = ep.collect_thread_events(&event(
        "e1",
        EventMsg::Error(codex_core::protocol::ErrorEvent {
            message: "boom".to_string(),
        }),
    ));
    assert_eq!(
        out,
        vec![ThreadEvent::Error(ThreadErrorEvent {
            message: "boom".to_string(),
        })]
    );
}

#[test]
fn stream_error_event_produces_error() {
    let mut ep = EventProcessorWithJsonOutput::new(None);
    let out = ep.collect_thread_events(&event(
        "e1",
        EventMsg::StreamError(codex_core::protocol::StreamErrorEvent {
            message: "retrying".to_string(),
        }),
    ));
    assert_eq!(
        out,
        vec![ThreadEvent::Error(ThreadErrorEvent {
            message: "retrying".to_string(),
        })]
    );
}

#[test]
fn error_followed_by_task_complete_produces_turn_failed() {
    let mut ep = EventProcessorWithJsonOutput::new(None);

    let error_event = event(
        "e1",
        EventMsg::Error(ErrorEvent {
            message: "boom".to_string(),
        }),
    );
    assert_eq!(
        ep.collect_thread_events(&error_event),
        vec![ThreadEvent::Error(ThreadErrorEvent {
            message: "boom".to_string(),
        })]
    );

    let complete_event = event(
        "e2",
        EventMsg::TaskComplete(codex_core::protocol::TaskCompleteEvent {
            last_agent_message: None,
        }),
    );
    assert_eq!(
        ep.collect_thread_events(&complete_event),
        vec![ThreadEvent::TurnFailed(TurnFailedEvent {
            error: ThreadErrorEvent {
                message: "boom".to_string(),
            },
        })]
    );
}

#[test]
fn exec_command_end_success_produces_completed_command_item() {
    let mut ep = EventProcessorWithJsonOutput::new(None);

    // Begin -> no output
    let begin = event(
        "c1",
        EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
            call_id: "1".to_string(),
            command: vec!["bash".to_string(), "-lc".to_string(), "echo hi".to_string()],
            cwd: std::env::current_dir().unwrap(),
            parsed_cmd: Vec::new(),
        }),
    );
    let out_begin = ep.collect_thread_events(&begin);
    assert_eq!(
        out_begin,
        vec![ThreadEvent::ItemStarted(ItemStartedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::CommandExecution(CommandExecutionItem {
                    command: "bash -lc 'echo hi'".to_string(),
                    aggregated_output: String::new(),
                    exit_code: None,
                    status: CommandExecutionStatus::InProgress,
                }),
            },
        })]
    );

    // End (success) -> item.completed (item_0)
    let end_ok = event(
        "c2",
        EventMsg::ExecCommandEnd(ExecCommandEndEvent {
            call_id: "1".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: "hi\n".to_string(),
            exit_code: 0,
            duration: Duration::from_millis(5),
            formatted_output: String::new(),
        }),
    );
    let out_ok = ep.collect_thread_events(&end_ok);
    assert_eq!(
        out_ok,
        vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::CommandExecution(CommandExecutionItem {
                    command: "bash -lc 'echo hi'".to_string(),
                    aggregated_output: "hi\n".to_string(),
                    exit_code: Some(0),
                    status: CommandExecutionStatus::Completed,
                }),
            },
        })]
    );
}

#[test]
fn exec_command_end_failure_produces_failed_command_item() {
    let mut ep = EventProcessorWithJsonOutput::new(None);

    // Begin -> no output
    let begin = event(
        "c1",
        EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
            call_id: "2".to_string(),
            command: vec!["sh".to_string(), "-c".to_string(), "exit 1".to_string()],
            cwd: std::env::current_dir().unwrap(),
            parsed_cmd: Vec::new(),
        }),
    );
    assert_eq!(
        ep.collect_thread_events(&begin),
        vec![ThreadEvent::ItemStarted(ItemStartedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::CommandExecution(CommandExecutionItem {
                    command: "sh -c 'exit 1'".to_string(),
                    aggregated_output: String::new(),
                    exit_code: None,
                    status: CommandExecutionStatus::InProgress,
                }),
            },
        })]
    );

    // End (failure) -> item.completed (item_0)
    let end_fail = event(
        "c2",
        EventMsg::ExecCommandEnd(ExecCommandEndEvent {
            call_id: "2".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 1,
            duration: Duration::from_millis(2),
            formatted_output: String::new(),
        }),
    );
    let out_fail = ep.collect_thread_events(&end_fail);
    assert_eq!(
        out_fail,
        vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "item_0".to_string(),
                details: ThreadItemDetails::CommandExecution(CommandExecutionItem {
                    command: "sh -c 'exit 1'".to_string(),
                    aggregated_output: String::new(),
                    exit_code: Some(1),
                    status: CommandExecutionStatus::Failed,
                }),
            },
        })]
    );
}

#[test]
fn exec_command_end_without_begin_is_ignored() {
    let mut ep = EventProcessorWithJsonOutput::new(None);

    // End event arrives without a prior Begin; should produce no thread events.
    let end_only = event(
        "c1",
        EventMsg::ExecCommandEnd(ExecCommandEndEvent {
            call_id: "no-begin".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: Duration::from_millis(1),
            formatted_output: String::new(),
        }),
    );
    let out = ep.collect_thread_events(&end_only);
    assert!(out.is_empty());
}

#[test]
fn patch_apply_success_produces_item_completed_patchapply() {
    let mut ep = EventProcessorWithJsonOutput::new(None);

    // Prepare a patch with multiple kinds of changes
    let mut changes = std::collections::HashMap::new();
    changes.insert(
        PathBuf::from("a/added.txt"),
        FileChange::Add {
            content: "+hello".to_string(),
        },
    );
    changes.insert(
        PathBuf::from("b/deleted.txt"),
        FileChange::Delete {
            content: "-goodbye".to_string(),
        },
    );
    changes.insert(
        PathBuf::from("c/modified.txt"),
        FileChange::Update {
            unified_diff: "--- c/modified.txt\n+++ c/modified.txt\n@@\n-old\n+new\n".to_string(),
            move_path: Some(PathBuf::from("c/renamed.txt")),
            old_content: "-old\n".to_string(),
            new_content: "+new\n".to_string(),
        },
    );

    // Begin -> no output
    let begin = event(
        "p1",
        EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
            call_id: "call-1".to_string(),
            auto_approved: true,
            changes: changes.clone(),
        }),
    );
    let out_begin = ep.collect_thread_events(&begin);
    assert!(out_begin.is_empty());

    // End (success) -> item.completed (item_0)
    let end = event(
        "p2",
        EventMsg::PatchApplyEnd(PatchApplyEndEvent {
            call_id: "call-1".to_string(),
            stdout: "applied 3 changes".to_string(),
            stderr: String::new(),
            success: true,
        }),
    );
    let out_end = ep.collect_thread_events(&end);
    assert_eq!(out_end.len(), 1);

    // Validate structure without relying on HashMap iteration order
    match &out_end[0] {
        ThreadEvent::ItemCompleted(ItemCompletedEvent { item }) => {
            assert_eq!(&item.id, "item_0");
            match &item.details {
                ThreadItemDetails::FileChange(file_update) => {
                    assert_eq!(file_update.status, PatchApplyStatus::Completed);

                    let mut actual: Vec<(String, PatchChangeKind)> = file_update
                        .changes
                        .iter()
                        .map(|c| (c.path.clone(), c.kind.clone()))
                        .collect();
                    actual.sort_by(|a, b| a.0.cmp(&b.0));

                    let mut expected = vec![
                        ("a/added.txt".to_string(), PatchChangeKind::Add),
                        ("b/deleted.txt".to_string(), PatchChangeKind::Delete),
                        ("c/modified.txt".to_string(), PatchChangeKind::Update),
                    ];
                    expected.sort_by(|a, b| a.0.cmp(&b.0));

                    assert_eq!(actual, expected);
                }
                other => panic!("unexpected details: {other:?}"),
            }
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn patch_apply_failure_produces_item_completed_patchapply_failed() {
    let mut ep = EventProcessorWithJsonOutput::new(None);

    let mut changes = std::collections::HashMap::new();
    changes.insert(
        PathBuf::from("file.txt"),
        FileChange::Update {
            unified_diff: "--- file.txt\n+++ file.txt\n@@\n-old\n+new\n".to_string(),
            move_path: None,
            old_content: "-old\n".to_string(),
            new_content: "+new\n".to_string(),
        },
    );

    // Begin -> no output
    let begin = event(
        "p1",
        EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
            call_id: "call-2".to_string(),
            auto_approved: false,
            changes: changes.clone(),
        }),
    );
    assert!(ep.collect_thread_events(&begin).is_empty());

    // End (failure) -> item.completed (item_0) with Failed status
    let end = event(
        "p2",
        EventMsg::PatchApplyEnd(PatchApplyEndEvent {
            call_id: "call-2".to_string(),
            stdout: String::new(),
            stderr: "failed to apply".to_string(),
            success: false,
        }),
    );
    let out_end = ep.collect_thread_events(&end);
    assert_eq!(out_end.len(), 1);

    match &out_end[0] {
        ThreadEvent::ItemCompleted(ItemCompletedEvent { item }) => {
            assert_eq!(&item.id, "item_0");
            match &item.details {
                ThreadItemDetails::FileChange(file_update) => {
                    assert_eq!(file_update.status, PatchApplyStatus::Failed);
                    assert_eq!(file_update.changes.len(), 1);
                    assert_eq!(file_update.changes[0].path, "file.txt".to_string());
                    assert_eq!(file_update.changes[0].kind, PatchChangeKind::Update);
                }
                other => panic!("unexpected details: {other:?}"),
            }
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn task_complete_produces_turn_completed_with_usage() {
    let mut ep = EventProcessorWithJsonOutput::new(None);

    // First, feed a TokenCount event with known totals.
    let usage = codex_core::protocol::TokenUsage {
        input_tokens: 1200,
        cached_input_tokens: 200,
        output_tokens: 345,
        reasoning_output_tokens: 0,
        total_tokens: 0,
    };
    let info = codex_core::protocol::TokenUsageInfo {
        total_token_usage: usage.clone(),
        last_token_usage: usage,
        model_context_window: None,
    };
    let token_count_event = event(
        "e1",
        EventMsg::TokenCount(codex_core::protocol::TokenCountEvent {
            info: Some(info),
            rate_limits: None,
        }),
    );
    assert!(ep.collect_thread_events(&token_count_event).is_empty());

    // Then TaskComplete should produce turn.completed with the captured usage.
    let complete_event = event(
        "e2",
        EventMsg::TaskComplete(codex_core::protocol::TaskCompleteEvent {
            last_agent_message: Some("done".to_string()),
        }),
    );
    let out = ep.collect_thread_events(&complete_event);
    assert_eq!(
        out,
        vec![ThreadEvent::TurnCompleted(TurnCompletedEvent {
            usage: Usage {
                input_tokens: 1200,
                cached_input_tokens: 200,
                output_tokens: 345,
            },
        })]
    );
}
