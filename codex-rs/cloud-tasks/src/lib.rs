mod app;
mod cli;
pub mod env_detect;
mod new_task;
pub mod scrollable_diff;
mod ui;
pub mod util;
pub use cli::Cli;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;
use tracing_subscriber::EnvFilter;
use util::append_error_log;
use util::set_user_agent_suffix;

struct ApplyJob {
    task_id: codex_cloud_tasks_client::TaskId,
    diff_override: Option<String>,
}

fn level_from_status(status: codex_cloud_tasks_client::ApplyStatus) -> app::ApplyResultLevel {
    match status {
        codex_cloud_tasks_client::ApplyStatus::Success => app::ApplyResultLevel::Success,
        codex_cloud_tasks_client::ApplyStatus::Partial => app::ApplyResultLevel::Partial,
        codex_cloud_tasks_client::ApplyStatus::Error => app::ApplyResultLevel::Error,
    }
}

fn spawn_preflight(
    app: &mut app::App,
    backend: &Arc<dyn codex_cloud_tasks_client::CloudBackend>,
    tx: &UnboundedSender<app::AppEvent>,
    frame_tx: &UnboundedSender<Instant>,
    title: String,
    job: ApplyJob,
) -> bool {
    if app.apply_inflight {
        app.status = "An apply is already running; wait for it to finish first.".to_string();
        return false;
    }
    if app.apply_preflight_inflight {
        app.status = "A preflight is already running; wait for it to finish first.".to_string();
        return false;
    }

    app.apply_preflight_inflight = true;
    let _ = frame_tx.send(Instant::now() + Duration::from_millis(100));

    let backend = backend.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let ApplyJob {
            task_id,
            diff_override,
        } = job;
        let result = codex_cloud_tasks_client::CloudBackend::apply_task_preflight(
            &*backend,
            task_id.clone(),
            diff_override,
        )
        .await;

        let event = match result {
            Ok(outcome) => {
                let level = level_from_status(outcome.status);
                app::AppEvent::ApplyPreflightFinished {
                    id: task_id,
                    title,
                    message: outcome.message,
                    level,
                    skipped: outcome.skipped_paths,
                    conflicts: outcome.conflict_paths,
                }
            }
            Err(e) => app::AppEvent::ApplyPreflightFinished {
                id: task_id,
                title,
                message: format!("Preflight failed: {e}"),
                level: app::ApplyResultLevel::Error,
                skipped: Vec::new(),
                conflicts: Vec::new(),
            },
        };

        let _ = tx.send(event);
    });

    true
}

fn spawn_apply(
    app: &mut app::App,
    backend: &Arc<dyn codex_cloud_tasks_client::CloudBackend>,
    tx: &UnboundedSender<app::AppEvent>,
    frame_tx: &UnboundedSender<Instant>,
    job: ApplyJob,
) -> bool {
    if app.apply_inflight {
        app.status = "An apply is already running; wait for it to finish first.".to_string();
        return false;
    }
    if app.apply_preflight_inflight {
        app.status = "Finish the current preflight before starting another apply.".to_string();
        return false;
    }

    app.apply_inflight = true;
    let _ = frame_tx.send(Instant::now() + Duration::from_millis(100));

    let backend = backend.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let ApplyJob {
            task_id,
            diff_override,
        } = job;
        let result = codex_cloud_tasks_client::CloudBackend::apply_task(
            &*backend,
            task_id.clone(),
            diff_override,
        )
        .await;

        let event = match result {
            Ok(outcome) => app::AppEvent::ApplyFinished {
                id: task_id,
                result: Ok(outcome),
            },
            Err(e) => app::AppEvent::ApplyFinished {
                id: task_id,
                result: Err(format!("{e}")),
            },
        };

        let _ = tx.send(event);
    });

    true
}

// logging helper lives in util module

// (no standalone patch summarizer needed – UI displays raw diffs)

/// Entry point for the `codex cloud` subcommand.
pub async fn run_main(_cli: Cli, _codex_linux_sandbox_exe: Option<PathBuf>) -> anyhow::Result<()> {
    // Very minimal logging setup; mirrors other crates' pattern.
    let default_level = "error";
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .or_else(|_| EnvFilter::try_new(default_level))
                .unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .try_init();

    info!("Launching Cloud Tasks list UI");
    set_user_agent_suffix("codex_cloud_tasks_tui");

    // Default to online unless explicitly configured to use mock.
    let use_mock = matches!(
        std::env::var("CODEX_CLOUD_TASKS_MODE").ok().as_deref(),
        Some("mock") | Some("MOCK")
    );

    let backend: Arc<dyn codex_cloud_tasks_client::CloudBackend> = if use_mock {
        Arc::new(codex_cloud_tasks_client::MockClient)
    } else {
        // Build an HTTP client against the configured (or default) base URL.
        let base_url = std::env::var("CODEX_CLOUD_TASKS_BASE_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string());
        let ua = codex_core::default_client::get_codex_user_agent();
        let mut http =
            codex_cloud_tasks_client::HttpClient::new(base_url.clone())?.with_user_agent(ua);
        // Log which base URL and path style we're going to use.
        let style = if base_url.contains("/backend-api") {
            "wham"
        } else {
            "codex-api"
        };
        append_error_log(format!("startup: base_url={base_url} path_style={style}"));

        // Require ChatGPT login (SWIC). Exit with a clear message if missing.
        let _token = match codex_core::config::find_codex_home()
            .ok()
            .map(codex_login::AuthManager::new)
            .and_then(|am| am.auth())
        {
            Some(auth) => {
                // Log account context for debugging workspace selection.
                if let Some(acc) = auth.get_account_id() {
                    append_error_log(format!("auth: mode=ChatGPT account_id={acc}"));
                }
                match auth.get_token().await {
                    Ok(t) if !t.is_empty() => {
                        // Attach token and ChatGPT-Account-Id header if available
                        http = http.with_bearer_token(t.clone());
                        if let Some(acc) = auth
                            .get_account_id()
                            .or_else(|| util::extract_chatgpt_account_id(&t))
                        {
                            append_error_log(format!("auth: set ChatGPT-Account-Id header: {acc}"));
                            http = http.with_chatgpt_account_id(acc);
                        }
                        t
                    }
                    _ => {
                        eprintln!(
                            "Not signed in. Please run 'codex login' to sign in with ChatGPT, then re-run 'codex cloud'."
                        );
                        std::process::exit(1);
                    }
                }
            }
            None => {
                eprintln!(
                    "Not signed in. Please run 'codex login' to sign in with ChatGPT, then re-run 'codex cloud'."
                );
                std::process::exit(1);
            }
        };
        Arc::new(http)
    };

    // Terminal setup
    use crossterm::ExecutableCommand;
    use crossterm::event::DisableBracketedPaste;
    use crossterm::event::EnableBracketedPaste;
    use crossterm::event::KeyboardEnhancementFlags;
    use crossterm::event::PopKeyboardEnhancementFlags;
    use crossterm::event::PushKeyboardEnhancementFlags;
    use crossterm::terminal::EnterAlternateScreen;
    use crossterm::terminal::LeaveAlternateScreen;
    use crossterm::terminal::disable_raw_mode;
    use crossterm::terminal::enable_raw_mode;
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableBracketedPaste)?;
    // Enable enhanced key reporting so Shift+Enter is distinguishable from Enter.
    // Some terminals may not support these flags; ignore errors if enabling fails.
    let _ = crossterm::execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );
    let backend_ui = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend_ui)?;
    terminal.clear()?;

    // App state
    let mut app = app::App::new();
    // Initial load
    let force_internal = matches!(
        std::env::var("CODEX_CLOUD_TASKS_FORCE_INTERNAL")
            .ok()
            .as_deref(),
        Some("1") | Some("true") | Some("TRUE")
    );
    append_error_log(format!(
        "startup: wham_force_internal={} ua={}",
        force_internal,
        codex_core::default_client::get_codex_user_agent()
    ));
    // Non-blocking initial load so the in-box spinner can animate
    app.status = "Loading tasks…".to_string();
    app.refresh_inflight = true;
    // New list generation; reset background enrichment coordination
    app.list_generation = app.list_generation.saturating_add(1);
    app.in_flight.clear();
    // reset any in-flight enrichment state

    // Event stream
    use crossterm::event::Event;
    use crossterm::event::EventStream;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEventKind;
    use crossterm::event::KeyModifiers;
    use tokio_stream::StreamExt;
    let mut events = EventStream::new();

    // Channel for non-blocking background loads
    use tokio::sync::mpsc::unbounded_channel;
    let (tx, mut rx) = unbounded_channel::<app::AppEvent>();
    // Kick off the initial load in background
    {
        let backend = Arc::clone(&backend);
        let tx = tx.clone();
        tokio::spawn(async move {
            let res = app::load_tasks(&*backend, None).await;
            let _ = tx.send(app::AppEvent::TasksLoaded {
                env: None,
                result: res,
            });
        });
    }
    // Fetch environment list in parallel so the header can show friendly names quickly.
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let base_url = util::normalize_base_url(
                &std::env::var("CODEX_CLOUD_TASKS_BASE_URL")
                    .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string()),
            );
            let headers = util::build_chatgpt_headers().await;
            let res = crate::env_detect::list_environments(&base_url, &headers).await;
            let _ = tx.send(app::AppEvent::EnvironmentsLoaded(res));
        });
    }

    // Try to auto-detect a likely environment id on startup and refresh if found.
    // Do this concurrently so the initial list shows quickly; on success we refetch with filter.
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let base_url = util::normalize_base_url(
                &std::env::var("CODEX_CLOUD_TASKS_BASE_URL")
                    .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string()),
            );
            // Build headers: UA + ChatGPT auth if available
            let headers = util::build_chatgpt_headers().await;

            // Run autodetect. If it fails, we keep using "All".
            let res = crate::env_detect::autodetect_environment_id(&base_url, &headers, None).await;
            let _ = tx.send(app::AppEvent::EnvironmentAutodetected(res));
        });
    }

    // Event-driven redraws with a tiny coalescing scheduler (snappy UI, no fixed 250ms tick).
    let mut needs_redraw = true;
    use std::time::Instant;
    use tokio::time::Instant as TokioInstant;
    use tokio::time::sleep_until;
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel::<Instant>();
    let (redraw_tx, mut redraw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // Coalesce frame requests to the earliest deadline; emit a single redraw signal.
    tokio::spawn(async move {
        let mut next_deadline: Option<Instant> = None;
        loop {
            let target =
                next_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(24 * 60 * 60));
            let sleeper = sleep_until(TokioInstant::from_std(target));
            tokio::pin!(sleeper);
            tokio::select! {
                recv = frame_rx.recv() => {
                    match recv {
                        Some(at) => {
                            if next_deadline.is_none_or(|cur| at < cur) {
                                next_deadline = Some(at);
                            }
                            continue; // recompute sleep target
                        }
                        None => break,
                    }
                }
                _ = &mut sleeper => {
                    if next_deadline.take().is_some() {
                        let _ = redraw_tx.send(());
                    }
                }
            }
        }
    });
    // Kick an initial draw so the UI appears immediately.
    let _ = frame_tx.send(Instant::now());

    // Render helper to centralize immediate redraws after handling events.
    let render_if_needed = |terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
                            app: &mut app::App,
                            needs_redraw: &mut bool|
     -> anyhow::Result<()> {
        if *needs_redraw {
            terminal.draw(|f| ui::draw(f, app))?;
            *needs_redraw = false;
        }
        Ok(())
    };

    let exit_code = loop {
        tokio::select! {
            // Coalesced redraw requests: spinner animation and paste-burst micro‑flush.
            Some(()) = redraw_rx.recv() => {
                // Micro‑flush pending first key held by paste‑burst.
                if let Some(page) = app.new_task.as_mut() {
                    if page.composer.flush_paste_burst_if_due() { needs_redraw = true; }
                    if page.composer.is_in_paste_burst() {
                        let _ = frame_tx.send(Instant::now() + codex_tui::ComposerInput::recommended_flush_delay());
                    }
                }
                // Advance throbber only while loading.
                if app.refresh_inflight
                    || app.details_inflight
                    || app.env_loading
                    || app.apply_preflight_inflight
                    || app.apply_inflight
                {
                    app.throbber.calc_next();
                    needs_redraw = true;
                    let _ = frame_tx.send(Instant::now() + Duration::from_millis(100));
                }
                render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
            }
            maybe_app_event = rx.recv() => {
                if let Some(ev) = maybe_app_event {
                    match ev {
                        app::AppEvent::TasksLoaded { env, result } => {
                            // Only apply results for the current filter to avoid races.
                            if env.as_deref() != app.env_filter.as_deref() {
                                append_error_log(format!(
                                    "refresh.drop: env={} current={}",
                                    env.clone().unwrap_or_else(|| "<all>".to_string()),
                                    app.env_filter.clone().unwrap_or_else(|| "<all>".to_string())
                                ));
                                continue;
                            }
                            app.refresh_inflight = false;
                            match result {
                                Ok(tasks) => {
                                    append_error_log(format!(
                                        "refresh.apply: env={} count={}",
                                        env.clone().unwrap_or_else(|| "<all>".to_string()),
                                        tasks.len()
                                    ));
                                    app.tasks = tasks;
                                    if app.selected >= app.tasks.len() { app.selected = app.tasks.len().saturating_sub(1); }
                                    app.status = "Loaded tasks".to_string();
                                }
                                Err(e) => {
                                    append_error_log(format!("refresh load_tasks failed: {e}"));
                                    app.status = format!("Failed to load tasks: {e}");
                                }
                            }
                            needs_redraw = true;
                            let _ = frame_tx.send(Instant::now());
                        }
                        app::AppEvent::NewTaskSubmitted(result) => {
                            match result {
                                Ok(created) => {
                                    append_error_log(format!("new-task: created id={}", created.id.0));
                                    app.status = format!("Submitted as {}", created.id.0);
                                    app.new_task = None;
                                    // Refresh tasks in background for current filter
                                    app.status = format!("Submitted as {} — refreshing…", created.id.0);
                                    app.refresh_inflight = true;
                                    app.list_generation = app.list_generation.saturating_add(1);
                                    needs_redraw = true;
                                    let backend = Arc::clone(&backend);
                                    let tx = tx.clone();
                                    let env_sel = app.env_filter.clone();
                                    tokio::spawn(async move {
                                        let res = app::load_tasks(&*backend, env_sel.as_deref()).await;
                                        let _ = tx.send(app::AppEvent::TasksLoaded { env: env_sel, result: res });
                                    });
                                    let _ = frame_tx.send(Instant::now());
                                }
                                Err(msg) => {
                                    append_error_log(format!("new-task: submit failed: {msg}"));
                                    if let Some(page) = app.new_task.as_mut() { page.submitting = false; }
                                    app.status = format!("Submit failed: {msg}. See error.log for details.");
                                    needs_redraw = true;
                                    let _ = frame_tx.send(Instant::now());
                                }
                            }
                        }
                        // (removed TaskSummaryUpdated; unused in this prototype)
                        app::AppEvent::ApplyPreflightFinished { id, title, message, level, skipped, conflicts } => {
                            // Only update if modal is still open and ids match
                            if let Some(m) = app.apply_modal.as_mut()
                                && m.task_id == id
                            {
                                    m.title = title;
                                    m.result_message = Some(message);
                                    m.result_level = Some(level);
                                    m.skipped_paths = skipped;
                                    m.conflict_paths = conflicts;
                                    app.apply_preflight_inflight = false;
                                    needs_redraw = true;
                                    let _ = frame_tx.send(Instant::now());
                            }
                        }
                        app::AppEvent::EnvironmentsLoaded(result) => {
                            app.env_loading = false;
                            match result {
                                Ok(list) => {
                                    app.environments = list;
                                    app.env_error = None;
                                    app.env_last_loaded = Some(std::time::Instant::now());
                                }
                                Err(e) => {
                                    app.env_error = Some(e.to_string());
                                }
                            }
                            needs_redraw = true;
                            let _ = frame_tx.send(Instant::now());
                        }
                        app::AppEvent::EnvironmentAutodetected(result) => {
                            if let Ok(sel) = result {
                                // Only apply if user hasn't set a filter yet or it's different.
                                if app.env_filter.as_deref() != Some(sel.id.as_str()) {
                                    append_error_log(format!(
                                        "env.select: autodetected id={} label={}",
                                        sel.id,
                                        sel.label.clone().unwrap_or_else(|| "<none>".to_string())
                                    ));
                                    // Preseed environments with detected label so header can show it even before list arrives
                                    if let Some(lbl) = sel.label.clone() {
                                        let present = app.environments.iter().any(|r| r.id == sel.id);
                                        if !present {
                                            app.environments.push(app::EnvironmentRow { id: sel.id.clone(), label: Some(lbl), is_pinned: false, repo_hints: None });
                                        }
                                    }
                                    app.env_filter = Some(sel.id);
                                    app.status = "Loading tasks…".to_string();
                                    app.refresh_inflight = true;
                                    app.list_generation = app.list_generation.saturating_add(1);
                                    app.in_flight.clear();
                            // reset spinner state
                                    needs_redraw = true;
                                    {
                                        let backend = Arc::clone(&backend);
                                        let tx = tx.clone();
                                        let env_sel = app.env_filter.clone();
                                        tokio::spawn(async move {
                                            let res = app::load_tasks(&*backend, env_sel.as_deref()).await;
                                            let _ = tx.send(app::AppEvent::TasksLoaded { env: env_sel, result: res });
                                        });
                                    }
                                    // Proactively fetch environments to resolve a friendly name for the header.
                                    app.env_loading = true;
                                    {
                                        let tx = tx.clone();
                                        tokio::spawn(async move {
                                            let base_url = crate::util::normalize_base_url(
                                                &std::env::var("CODEX_CLOUD_TASKS_BASE_URL")
                                                    .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string()),
                                            );
                                            let headers = crate::util::build_chatgpt_headers().await;
                                            let res = crate::env_detect::list_environments(&base_url, &headers).await;
                                            let _ = tx.send(app::AppEvent::EnvironmentsLoaded(res));
                                        });
                                    }
                                    let _ = frame_tx.send(Instant::now());
                                }
                            }
                            // on Err, silently continue with All
                        }
                        app::AppEvent::DetailsDiffLoaded { id, title, diff } => {
                            if let Some(ov) = &app.diff_overlay
                                && ov.task_id != id {
                                    continue;
                                }
                            let diff_lines: Vec<String> = diff.lines().map(str::to_string).collect();
                            if let Some(ov) = app.diff_overlay.as_mut() {
                                ov.title = title;
                                {
                                    let base = ov.base_attempt_mut();
                                    base.diff_lines = diff_lines.clone();
                                    base.diff_raw = Some(diff.clone());
                                }
                                ov.base_can_apply = true;
                                ov.apply_selection_to_fields();
                            } else {
                                let mut overlay = app::DiffOverlay::new(id.clone(), title, None);
                                {
                                    let base = overlay.base_attempt_mut();
                                    base.diff_lines = diff_lines.clone();
                                    base.diff_raw = Some(diff.clone());
                                }
                                overlay.base_can_apply = true;
                                overlay.current_view = app::DetailView::Diff;
                                overlay.apply_selection_to_fields();
                                app.diff_overlay = Some(overlay);
                            }
                            app.details_inflight = false;
                            app.status.clear();
                            needs_redraw = true;
                        }
                        app::AppEvent::DetailsMessagesLoaded {
                            id,
                            title,
                            messages,
                            prompt,
                            turn_id,
                            sibling_turn_ids,
                            attempt_placement,
                            attempt_status,
                        } => {
                            if let Some(ov) = &app.diff_overlay
                                && ov.task_id != id {
                                    continue;
                                }
                            let conv = conversation_lines(prompt.clone(), &messages);
                            if let Some(ov) = app.diff_overlay.as_mut() {
                                ov.title = title.clone();
                                {
                                    let base = ov.base_attempt_mut();
                                    base.text_lines = conv.clone();
                                    base.prompt = prompt.clone();
                                    base.turn_id = turn_id.clone();
                                    base.status = attempt_status;
                                    base.attempt_placement = attempt_placement;
                                }
                                ov.base_turn_id = turn_id.clone();
                                ov.sibling_turn_ids = sibling_turn_ids.clone();
                                ov.attempt_total_hint = Some(sibling_turn_ids.len().saturating_add(1));
                                if !ov.base_can_apply {
                                    ov.current_view = app::DetailView::Prompt;
                                }
                                ov.apply_selection_to_fields();
                                if let (Some(turn_id), true) = (turn_id.clone(), !sibling_turn_ids.is_empty())
                                    && ov.attempts.len() == 1 {
                                        let backend = Arc::clone(&backend);
                                        let tx = tx.clone();
                                        let task_id = id.clone();
                                        tokio::spawn(async move {
                                            match codex_cloud_tasks_client::CloudBackend::list_sibling_attempts(
                                                &*backend,
                                                task_id.clone(),
                                                turn_id,
                                            )
                                            .await
                                            {
                                                Ok(attempts) => {
                                                    let _ = tx.send(app::AppEvent::AttemptsLoaded { id: task_id, attempts });
                                                }
                                                Err(e) => {
                                                    crate::util::append_error_log(format!(
                                                        "attempts.load failed for {}: {e}",
                                                        task_id.0
                                                    ));
                                                }
                                            }
                                        });
                                    }
                            } else {
                                let mut overlay = app::DiffOverlay::new(id.clone(), title, None);
                                {
                                    let base = overlay.base_attempt_mut();
                                    base.text_lines = conv.clone();
                                    base.prompt = prompt.clone();
                                    base.turn_id = turn_id.clone();
                                    base.status = attempt_status;
                                    base.attempt_placement = attempt_placement;
                                }
                                overlay.base_turn_id = turn_id.clone();
                                overlay.sibling_turn_ids = sibling_turn_ids.clone();
                                overlay.attempt_total_hint = Some(sibling_turn_ids.len().saturating_add(1));
                                overlay.current_view = app::DetailView::Prompt;
                                overlay.apply_selection_to_fields();
                                app.diff_overlay = Some(overlay);
                            }
                            app.details_inflight = false;
                            app.status.clear();
                            needs_redraw = true;
                        }
                        app::AppEvent::AttemptsLoaded { id, attempts } => {
                            if let Some(ov) = app.diff_overlay.as_mut() {
                                if ov.task_id != id {
                                    continue;
                                }
                                for attempt in attempts {
                                    if ov
                                        .attempts
                                        .iter()
                                        .any(|existing| existing.turn_id.as_deref() == Some(attempt.turn_id.as_str()))
                                    {
                                        continue;
                                    }
                                    let diff_lines = attempt
                                        .diff
                                        .as_ref()
                                        .map(|d| d.lines().map(str::to_string).collect())
                                        .unwrap_or_default();
                                    let text_lines = conversation_lines(None, &attempt.messages);
                                    ov.attempts.push(app::AttemptView {
                                        turn_id: Some(attempt.turn_id.clone()),
                                        status: attempt.status,
                                        attempt_placement: attempt.attempt_placement,
                                        diff_lines,
                                        text_lines,
                                        prompt: None,
                                        diff_raw: attempt.diff.clone(),
                                    });
                                }
                                if ov.attempts.len() > 1 {
                                    let (_, rest) = ov.attempts.split_at_mut(1);
                                    rest.sort_by(|a, b| match (a.attempt_placement, b.attempt_placement) {
                                        (Some(lhs), Some(rhs)) => lhs.cmp(&rhs),
                                        (Some(_), None) => std::cmp::Ordering::Less,
                                        (None, Some(_)) => std::cmp::Ordering::Greater,
                                        (None, None) => a.turn_id.cmp(&b.turn_id),
                                    });
                                }
                                if ov.selected_attempt >= ov.attempts.len() {
                                    ov.selected_attempt = ov.attempts.len().saturating_sub(1);
                                }
                                ov.attempt_total_hint = Some(ov.attempts.len());
                                ov.apply_selection_to_fields();
                                needs_redraw = true;
                            }
                        }
                        app::AppEvent::DetailsFailed { id, title, error } => {
                            if let Some(ov) = &app.diff_overlay
                                && ov.task_id != id {
                                    continue;
                                }
                            append_error_log(format!("details failed for {}: {error}", id.0));
                            let pretty = pretty_lines_from_error(&error);
                            if let Some(ov) = app.diff_overlay.as_mut() {
                                ov.title = title.clone();
                                {
                                    let base = ov.base_attempt_mut();
                                    base.diff_lines.clear();
                                    base.text_lines = pretty.clone();
                                    base.prompt = None;
                                }
                                ov.base_can_apply = false;
                                ov.current_view = app::DetailView::Prompt;
                                ov.apply_selection_to_fields();
                            } else {
                                let mut overlay = app::DiffOverlay::new(id.clone(), title, None);
                                {
                                    let base = overlay.base_attempt_mut();
                                    base.text_lines = pretty;
                                }
                                overlay.base_can_apply = false;
                                overlay.current_view = app::DetailView::Prompt;
                                overlay.apply_selection_to_fields();
                                app.diff_overlay = Some(overlay);
                            }
                            app.details_inflight = false;
                            needs_redraw = true;
                        }
                        app::AppEvent::ApplyFinished { id, result } => {
                            // Only update if the modal still corresponds to this id.
                            if let Some(m) = &app.apply_modal {
                                if m.task_id != id { continue; }
                            } else {
                                continue;
                            }
                            app.apply_inflight = false;
                            match result {
                                Ok(outcome) => {
                                    app.status = outcome.message.clone();
                                    if matches!(outcome.status, codex_cloud_tasks_client::ApplyStatus::Success) {
                                        app.apply_modal = None;
                                        app.diff_overlay = None;
                                        // Refresh tasks after successful apply
                                        let backend = Arc::clone(&backend);
                                        let tx = tx.clone();
                                        let env_sel = app.env_filter.clone();
                                        tokio::spawn(async move {
                                            let res = app::load_tasks(&*backend, env_sel.as_deref()).await;
                                            let _ = tx.send(app::AppEvent::TasksLoaded { env: env_sel, result: res });
                                        });
                                    }
                                }
                                Err(e) => {
                                    append_error_log(format!("apply_task failed for {}: {e}", id.0));
                                    app.status = format!("Apply failed: {e}");
                                }
                            }
                            needs_redraw = true;
                        }
                    }
                }
                // Render immediately after processing app events.
                render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
            }
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Paste(pasted))) => {
                        if app.env_modal.is_some() {
                            if let Some(m) = app.env_modal.as_mut() {
                                for ch in pasted.chars() {
                                    match ch {
                                        '\r' | '\n' => continue,
                                        '\t' => m.query.push(' '),
                                        _ => m.query.push(ch),
                                    }
                                }
                            }
                            needs_redraw = true;
                        } else if let Some(page) = app.new_task.as_mut()
                            && !page.submitting
                        {
                            if page.composer.handle_paste(pasted) {
                                needs_redraw = true;
                            }
                            let _ = frame_tx.send(Instant::now());
                        }
                    }
                    Some(Ok(Event::Key(key))) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                        // Treat Ctrl-C like pressing 'q' in the current context.
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                        {
                            if app.env_modal.is_some() {
                                // Close environment selector if open (don’t quit composer).
                                app.env_modal = None;
                                needs_redraw = true;
                            } else if app.best_of_modal.is_some() {
                                app.best_of_modal = None;
                                needs_redraw = true;
                            } else if app.apply_modal.is_some() {
                                app.apply_modal = None;
                                app.status = "Apply canceled".to_string();
                                needs_redraw = true;
                            } else if app.new_task.is_some() {
                                app.new_task = None;
                                app.status = "Canceled new task".to_string();
                                needs_redraw = true;
                            } else if app.diff_overlay.is_some() {
                                app.diff_overlay = None;
                                needs_redraw = true;
                            } else {
                                break 0;
                            }
                            // Render updated state immediately before continuing to next loop iteration.
                            render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
                            // Render after New Task branch to reflect input changes immediately.
                            render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
                            continue;
                        }
                        let is_ctrl_n = key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
                            || matches!(key.code, KeyCode::Char('\u{000E}'));
                        if is_ctrl_n {
                            if app.new_task.is_none() {
                                continue;
                            }
                            if app.best_of_modal.is_some() {
                                app.best_of_modal = None;
                                needs_redraw = true;
                            } else {
                                let selected = app.best_of_n.saturating_sub(1).min(3);
                                app.best_of_modal = Some(app::BestOfModalState { selected });
                                app.status = format!(
                                    "Select best-of attempts (current: {} attempt{})",
                                    app.best_of_n,
                                    if app.best_of_n == 1 { "" } else { "s" }
                                );
                                needs_redraw = true;
                            }
                            render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
                            continue;
                        }
                        if app.best_of_modal.is_some() {
                            match key.code {
                                KeyCode::Esc => {
                                    app.best_of_modal = None;
                                    needs_redraw = true;
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    if let Some(m) = app.best_of_modal.as_mut() {
                                        m.selected = (m.selected + 1).min(3);
                                    }
                                    needs_redraw = true;
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    if let Some(m) = app.best_of_modal.as_mut() {
                                        m.selected = m.selected.saturating_sub(1);
                                    }
                                    needs_redraw = true;
                                }
                                KeyCode::Char('1') | KeyCode::Char('2') | KeyCode::Char('3') | KeyCode::Char('4') => {
                                    if let Some(m) = app.best_of_modal.as_mut() {
                                        let val = match key.code {
                                            KeyCode::Char('1') => 0,
                                            KeyCode::Char('2') => 1,
                                            KeyCode::Char('3') => 2,
                                            KeyCode::Char('4') => 3,
                                            _ => m.selected,
                                        };
                                        m.selected = val;
                                    }
                                    needs_redraw = true;
                                }
                                KeyCode::Enter => {
                                    if let Some(state) = app.best_of_modal.take() {
                                        let new_value = state.selected + 1;
                                        app.best_of_n = new_value;
                                        if let Some(page) = app.new_task.as_mut() {
                                            page.best_of_n = new_value;
                                        }
                                        append_error_log(format!("best-of.select: attempts={new_value}"));
                                        app.status = format!(
                                            "Best-of updated to {new_value} attempt{}",
                                            if new_value == 1 { "" } else { "s" }
                                        );
                                        needs_redraw = true;
                                    }
                                }
                                _ => {}
                            }
                            render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
                            continue;
                        }
                        // New Task page: Ctrl+O opens environment switcher while composing.
                        let is_ctrl_o = key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
                            || matches!(key.code, KeyCode::Char('\u{000F}'));
                        if is_ctrl_o && app.new_task.is_some() {
                            // Close task modal/pending apply if present before opening env modal
                            app.diff_overlay = None;
                            app.env_modal = Some(app::EnvModalState { query: String::new(), selected: 0 });
                            // Cache environments until user explicitly refreshes with 'r' inside the modal.
                            let should_fetch = app.environments.is_empty();
                            if should_fetch {
                                app.env_loading = true;
                                app.env_error = None;
                                // Ensure spinner animates while loading environments.
                                let _ = frame_tx.send(Instant::now() + Duration::from_millis(100));
                            }
                            needs_redraw = true;
                            if should_fetch {
                                    let tx = tx.clone();
                                    tokio::spawn(async move {
            let base_url = crate::util::normalize_base_url(&std::env::var("CODEX_CLOUD_TASKS_BASE_URL").unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string()));
            let headers = crate::util::build_chatgpt_headers().await;
                                        let res = crate::env_detect::list_environments(&base_url, &headers).await;
                                        let _ = tx.send(app::AppEvent::EnvironmentsLoaded(res));
                                    });
                            }
                            // Render after opening env modal to show it instantly.
                            render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
                            continue;
                        }

                        // New Task page has priority when active, unless an env modal is open.
                        if let Some(page) = app.new_task.as_mut() {
                            if app.env_modal.is_some() {
                                // Defer handling to env-modal branch below.
                            } else {
                            match key.code {
                                KeyCode::Esc => {
                                    app.new_task = None;
                                    app.status = "Canceled new task".to_string();
                                    needs_redraw = true;
                                }
                                _ => {
                                    if page.submitting {
                                        // Ignore input while submitting
                                    } else if let codex_tui::ComposerAction::Submitted(text) = page.composer.input(key) {
                                            // Submit only if we have an env id
                                            if let Some(env) = page.env_id.clone() {
                                                append_error_log(format!(
                                                    "new-task: submit env={} size={}",
                                                    env,
                                                    text.chars().count()
                                                ));
                                                page.submitting = true;
                                                app.status = "Submitting new task…".to_string();
                                                let tx = tx.clone();
                                                let backend = Arc::clone(&backend);
                                                let best_of_n = page.best_of_n;
                                                tokio::spawn(async move {
                                                    let result = codex_cloud_tasks_client::CloudBackend::create_task(&*backend, &env, &text, "main", false, best_of_n).await;
                                                    let evt = match result {
                                                        Ok(ok) => app::AppEvent::NewTaskSubmitted(Ok(ok)),
                                                        Err(e) => app::AppEvent::NewTaskSubmitted(Err(format!("{e}"))),
                                                    };
                                                    let _ = tx.send(evt);
                                                });
                                            } else {
                                                app.status = "No environment selected (press 'e' to choose)".to_string();
                                            }
                                    }
                                    needs_redraw = true;
                                    // If paste‑burst is active, schedule a micro‑flush frame.
                                    if page.composer.is_in_paste_burst() {
                                        let _ = frame_tx.send(Instant::now() + codex_tui::ComposerInput::recommended_flush_delay());
                                    }
                                    // Always schedule an immediate redraw for key edits in the composer.
                                    let _ = frame_tx.send(Instant::now());
                                    // Draw now so non-char edits (e.g., Option+Delete) reflect instantly.
                                    render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
                                }
                            }
                            continue;
                            }
                        }
                        // If a diff overlay is open, handle its keys first.
                        if app.apply_modal.is_some() {
                            // Simple apply confirmation modal: y apply, p preflight, n/Esc cancel
                            match key.code {
                                KeyCode::Char('y') => {
                                    if let Some(m) = app.apply_modal.as_ref() {
                                        let title = m.title.clone();
                                        let job = ApplyJob {
                                            task_id: m.task_id.clone(),
                                            diff_override: m.diff_override.clone(),
                                        };
                                        if spawn_apply(&mut app, &backend, &tx, &frame_tx, job) {
                                            app.status = format!("Applying '{title}'...");
                                        }
                                        needs_redraw = true;
                                    }
                                }
                                KeyCode::Char('p') => {
                                    if let Some(m) = app.apply_modal.take() {
                                        let title = m.title.clone();
                                        let job = ApplyJob {
                                            task_id: m.task_id.clone(),
                                            diff_override: m.diff_override.clone(),
                                        };
                                        if spawn_preflight(&mut app, &backend, &tx, &frame_tx, title.clone(), job) {
                                            app.apply_modal = Some(app::ApplyModalState {
                                                task_id: m.task_id,
                                                title: title.clone(),
                                                result_message: None,
                                                result_level: None,
                                                skipped_paths: Vec::new(),
                                                conflict_paths: Vec::new(),
                                                diff_override: m.diff_override,
                                            });
                                            app.status = format!("Preflighting '{title}'...");
                                        } else {
                                            app.apply_modal = Some(m);
                                        }
                                        needs_redraw = true;
                                    }
                                }
                                KeyCode::Esc
                                | KeyCode::Char('n')
                                | KeyCode::Char('q')
                                | KeyCode::Char('Q') => { app.apply_modal = None; app.status = "Apply canceled".to_string(); needs_redraw = true; }
                                _ => {}
                            }
                        } else if app.diff_overlay.is_some() {
                            let mut cycle_attempt = |delta: isize| {
                                if let Some(ov) = app.diff_overlay.as_mut()
                                    && ov.attempt_count() > 1 {
                                        ov.step_attempt(delta);
                                        let total = ov.attempt_display_total();
                                        let current = ov.selected_attempt + 1;
                                        app.status = format!("Viewing attempt {current} of {total}");
                                        ov.sd.to_top();
                                        needs_redraw = true;
                                    }
                            };

                            match key.code {
                                KeyCode::Char('a') => {
                                    if app.apply_inflight || app.apply_preflight_inflight {
                                        app.status = "Finish the current apply/preflight before starting another.".to_string();
                                        needs_redraw = true;
                                        continue;
                                    }
                                    let snapshot = app.diff_overlay.as_ref().map(|ov| {
                                        (
                                            ov.task_id.clone(),
                                            ov.title.clone(),
                                            ov.current_can_apply(),
                                            ov.current_attempt().and_then(|attempt| attempt.diff_raw.clone()),
                                        )
                                    });
                                    if let Some((task_id, title, can_apply, diff_override)) = snapshot {
                                        if can_apply {
                                            let job = ApplyJob {
                                                task_id: task_id.clone(),
                                                diff_override: diff_override.clone(),
                                            };
                                            if spawn_preflight(&mut app, &backend, &tx, &frame_tx, title.clone(), job) {
                                                app.apply_modal = Some(app::ApplyModalState {
                                                    task_id,
                                                    title: title.clone(),
                                                    result_message: None,
                                                    result_level: None,
                                                    skipped_paths: Vec::new(),
                                                    conflict_paths: Vec::new(),
                                                    diff_override,
                                                });
                                                app.status = format!("Preflighting '{title}'...");
                                            }
                                        } else {
                                            app.status = "No diff available to apply.".to_string();
                                        }
                                        needs_redraw = true;
                                    }
                                }
                                KeyCode::Tab => {
                                    cycle_attempt(1);
                                }
                                KeyCode::BackTab => {
                                    cycle_attempt(-1);
                                }
                                // From task modal, 'o' should close it and open the env selector
                                KeyCode::Char('o') | KeyCode::Char('O') => {
                                    app.diff_overlay = None;
                                    app.env_modal = Some(app::EnvModalState { query: String::new(), selected: 0 });
                                    // Use cached environments unless empty
                                    if app.environments.is_empty() { app.env_loading = true; app.env_error = None; }
                                    needs_redraw = true;
                                    if app.environments.is_empty() {
                                        let tx = tx.clone();
                                        tokio::spawn(async move {
                                            let base_url = crate::util::normalize_base_url(
                                                &std::env::var("CODEX_CLOUD_TASKS_BASE_URL")
                                                    .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string()),
                                            );
                                            let headers = crate::util::build_chatgpt_headers().await;
                                            let res = crate::env_detect::list_environments(&base_url, &headers).await;
                                            let _ = tx.send(app::AppEvent::EnvironmentsLoaded(res));
                                        });
                                    }
                                }
                                KeyCode::Left => {
                                    if let Some(ov) = &mut app.diff_overlay {
                                        let has_text = ov.current_attempt().is_some_and(app::AttemptView::has_text);
                                        let has_diff = ov.current_attempt().is_some_and(app::AttemptView::has_diff) || ov.base_can_apply;
                                        if has_text && has_diff {
                                            ov.set_view(app::DetailView::Prompt);
                                            ov.sd.to_top();
                                            needs_redraw = true;
                                        }
                                    }
                                }
                                KeyCode::Right => {
                                    if let Some(ov) = &mut app.diff_overlay {
                                        let has_text = ov.current_attempt().is_some_and(app::AttemptView::has_text);
                                        let has_diff = ov.current_attempt().is_some_and(app::AttemptView::has_diff) || ov.base_can_apply;
                                        if has_text && has_diff {
                                            ov.set_view(app::DetailView::Diff);
                                            ov.sd.to_top();
                                            needs_redraw = true;
                                        }
                                    }
                                }
                                KeyCode::Char(']') | KeyCode::Char('}') => {
                                    cycle_attempt(1);
                                }
                                KeyCode::Char('[') | KeyCode::Char('{') => {
                                    cycle_attempt(-1);
                                }
                                KeyCode::Esc | KeyCode::Char('q') => {
                                    app.diff_overlay = None;
                                    needs_redraw = true;
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    if let Some(ov) = &mut app.diff_overlay { ov.sd.scroll_by(1); }
                                    needs_redraw = true;
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    if let Some(ov) = &mut app.diff_overlay { ov.sd.scroll_by(-1); }
                                    needs_redraw = true;
                                }
                                KeyCode::PageDown | KeyCode::Char(' ') => {
                                    if let Some(ov) = &mut app.diff_overlay { let step = ov.sd.state.viewport_h.saturating_sub(1) as i16; ov.sd.page_by(step); }
                                    needs_redraw = true;
                                }
                                KeyCode::PageUp => {
                                    if let Some(ov) = &mut app.diff_overlay { let step = ov.sd.state.viewport_h.saturating_sub(1) as i16; ov.sd.page_by(-step); }
                                    needs_redraw = true;
                                }
                                KeyCode::Home => { if let Some(ov) = &mut app.diff_overlay { ov.sd.to_top(); } needs_redraw = true; }
                                KeyCode::End  => { if let Some(ov) = &mut app.diff_overlay { ov.sd.to_bottom(); } needs_redraw = true; }
                                _ => {}
                            }
                        } else if app.env_modal.is_some() {
                            // Environment modal key handling
                            match key.code {
                                KeyCode::Esc => { app.env_modal = None; needs_redraw = true; }
                                KeyCode::Char('r') | KeyCode::Char('R') => {
                                    // Trigger refresh of environments
                                    app.env_loading = true; app.env_error = None; needs_redraw = true;
                                    let _ = frame_tx.send(Instant::now() + Duration::from_millis(100));
                                    let tx = tx.clone();
                                    tokio::spawn(async move {
            let base_url = crate::util::normalize_base_url(&std::env::var("CODEX_CLOUD_TASKS_BASE_URL").unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string()));
            let headers = crate::util::build_chatgpt_headers().await;
                                        let res = crate::env_detect::list_environments(&base_url, &headers).await;
                                        let _ = tx.send(app::AppEvent::EnvironmentsLoaded(res));
                                    });
                                }
                                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT) => {
                                    if let Some(m) = app.env_modal.as_mut() { m.query.push(ch); }
                                    needs_redraw = true;
                                }
                                KeyCode::Backspace => { if let Some(m) = app.env_modal.as_mut() { m.query.pop(); } needs_redraw = true; }
                                KeyCode::Down | KeyCode::Char('j') => { if let Some(m) = app.env_modal.as_mut() { m.selected = m.selected.saturating_add(1); } needs_redraw = true; }
                                KeyCode::Up | KeyCode::Char('k') => { if let Some(m) = app.env_modal.as_mut() { m.selected = m.selected.saturating_sub(1); } needs_redraw = true; }
                                KeyCode::Home => { if let Some(m) = app.env_modal.as_mut() { m.selected = 0; } needs_redraw = true; }
                                KeyCode::End => { if let Some(m) = app.env_modal.as_mut() { m.selected = app.environments.len(); } needs_redraw = true; }
                                KeyCode::PageDown | KeyCode::Char(' ') => { if let Some(m) = app.env_modal.as_mut() { let step = 10usize; m.selected = m.selected.saturating_add(step); } needs_redraw = true; }
                                KeyCode::PageUp => { if let Some(m) = app.env_modal.as_mut() { let step = 10usize; m.selected = m.selected.saturating_sub(step); } needs_redraw = true; }
                                KeyCode::Char('n') => {
                                    if app.env_filter.is_none() {
                                        app.new_task = Some(crate::new_task::NewTaskPage::new(None, app.best_of_n));
                                    } else {
                                        app.new_task = Some(crate::new_task::NewTaskPage::new(app.env_filter.clone(), app.best_of_n));
                                    }
                                    app.status = "New Task: Enter to submit; Esc to cancel".to_string();
                                    needs_redraw = true;
                                }
                                KeyCode::Enter => {
                                    // Resolve selection over filtered set
                                    if let Some(state) = app.env_modal.take() {
                                        let q = state.query.to_lowercase();
                                        let filtered: Vec<&app::EnvironmentRow> = app.environments.iter().filter(|r| {
                                            if q.is_empty() { return true; }
                                            let mut hay = String::new();
                                            if let Some(l) = &r.label { hay.push_str(&l.to_lowercase()); hay.push(' '); }
                                            hay.push_str(&r.id.to_lowercase());
                                            if let Some(h) = &r.repo_hints { hay.push(' '); hay.push_str(&h.to_lowercase()); }
                                            hay.contains(&q)
                                        }).collect();
                                        // Keep original order (already sorted) — no need to re-sort
                                        let idx = state.selected;
                                        if idx == 0 { app.env_filter = None; append_error_log("env.select: All"); }
                                        else {
                                            let env_idx = idx.saturating_sub(1);
                                            if let Some(row) = filtered.get(env_idx) {
                                                append_error_log(format!(
                                                    "env.select: id={} label={}",
                                                    row.id,
                                                    row.label.clone().unwrap_or_else(|| "<none>".to_string())
                                                ));
                                                app.env_filter = Some(row.id.clone());
                                            }
                                        }
                                        // If New Task page is open, reflect the new selection in its header immediately.
                                        if let Some(page) = app.new_task.as_mut() {
                                            page.env_id = app.env_filter.clone();
                                        }
                                        // Trigger tasks refresh with the selected filter
                                        app.status = "Loading tasks…".to_string();
                                        app.refresh_inflight = true;
                                        app.list_generation = app.list_generation.saturating_add(1);
                                        app.in_flight.clear();
                                        // reset spinner state
                                        needs_redraw = true;
                                        let backend = Arc::clone(&backend);
                                        let tx = tx.clone();
                                        let env_sel = app.env_filter.clone();
                                        tokio::spawn(async move {
                                            let res = app::load_tasks(&*backend, env_sel.as_deref()).await;
                                            let _ = tx.send(app::AppEvent::TasksLoaded { env: env_sel, result: res });
                                        });
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            // Base list view keys
                            match key.code {
                                KeyCode::Char('q') | KeyCode::Esc => {
                                    break 0;
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    app.next();
                                    needs_redraw = true;
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    app.prev();
                                    needs_redraw = true;
                                }
                                // Ensure 'r' does not refresh tasks when the env modal is open.
                                KeyCode::Char('r') | KeyCode::Char('R') => {
                                    if app.env_modal.is_some() { break 0; }
                                    append_error_log(format!(
                                        "refresh.request: env={}",
                                        app.env_filter.clone().unwrap_or_else(|| "<all>".to_string())
                                    ));
                                    app.status = "Refreshing…".to_string();
                                    app.refresh_inflight = true;
                                    app.list_generation = app.list_generation.saturating_add(1);
                                    app.in_flight.clear();
                                        // reset spinner state
                                    needs_redraw = true;
                                    // Spawn background refresh
                                    let backend = Arc::clone(&backend);
                                    let tx = tx.clone();
                                    let env_sel = app.env_filter.clone();
                                    tokio::spawn(async move {
                                        let res = app::load_tasks(&*backend, env_sel.as_deref()).await;
                                        let _ = tx.send(app::AppEvent::TasksLoaded { env: env_sel, result: res });
                                    });
                                }
                                KeyCode::Char('o') | KeyCode::Char('O') => {
                                    app.env_modal = Some(app::EnvModalState { query: String::new(), selected: 0 });
                                    // Cache environments until user explicitly refreshes with 'r' inside the modal.
                                    let should_fetch = app.environments.is_empty();
                                    if should_fetch { app.env_loading = true; app.env_error = None; }
                                    needs_redraw = true;
                                    if should_fetch {
                                    let tx = tx.clone();
                                    tokio::spawn(async move {
                                        let base_url = crate::util::normalize_base_url(&std::env::var("CODEX_CLOUD_TASKS_BASE_URL").unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string()));
                                        let headers = crate::util::build_chatgpt_headers().await;
                                        let res = crate::env_detect::list_environments(&base_url, &headers).await;
                                        let _ = tx.send(app::AppEvent::EnvironmentsLoaded(res));
                                    });
                                    }
                                }
                                KeyCode::Char('n') => {
                                    let env_opt = app.env_filter.clone();
                                    app.new_task = Some(crate::new_task::NewTaskPage::new(env_opt, app.best_of_n));
                                    app.status = "New Task: Enter to submit; Esc to cancel".to_string();
                                    needs_redraw = true;
                                }
                                KeyCode::Enter => {
                                    if let Some(task) = app.tasks.get(app.selected).cloned() {
                                        app.status = format!("Loading details for {title}…", title = task.title);
                                        app.details_inflight = true;
                                        // Open empty overlay immediately; content arrives via events
                                        let overlay = app::DiffOverlay::new(
                                            task.id.clone(),
                                            task.title.clone(),
                                            task.attempt_total,
                                        );
                                        app.diff_overlay = Some(overlay);
                                        needs_redraw = true;
                                        // Spawn background details load (diff first, then messages fallback)
                                        let id = task.id.clone();
                                        let title = task.title.clone();
                                        {
                                            let backend = Arc::clone(&backend);
                                            let tx = tx.clone();
                                            let diff_id = id.clone();
                                            let diff_title = title.clone();
                                            tokio::spawn(async move {
                                                match codex_cloud_tasks_client::CloudBackend::get_task_diff(&*backend, diff_id.clone()).await {
                                                    Ok(Some(diff)) => {
                                                        let _ = tx.send(app::AppEvent::DetailsDiffLoaded { id: diff_id, title: diff_title, diff });
                                                    }
                                                    Ok(None) => {
                                                        match codex_cloud_tasks_client::CloudBackend::get_task_text(&*backend, diff_id.clone()).await {
                                                            Ok(text) => {
                                                                let evt = app::AppEvent::DetailsMessagesLoaded {
                                                                    id: diff_id,
                                                                    title: diff_title,
                                                                    messages: text.messages,
                                                                    prompt: text.prompt,
                                                                    turn_id: text.turn_id,
                                                                    sibling_turn_ids: text.sibling_turn_ids,
                                                                    attempt_placement: text.attempt_placement,
                                                                    attempt_status: text.attempt_status,
                                                                };
                                                                let _ = tx.send(evt);
                                                            }
                                                            Err(e2) => {
                                                                let _ = tx.send(app::AppEvent::DetailsFailed { id: diff_id, title: diff_title, error: format!("{e2}") });
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        append_error_log(format!("get_task_diff failed for {}: {e}", diff_id.0));
                                                        match codex_cloud_tasks_client::CloudBackend::get_task_text(&*backend, diff_id.clone()).await {
                                                            Ok(text) => {
                                                                let evt = app::AppEvent::DetailsMessagesLoaded {
                                                                    id: diff_id,
                                                                    title: diff_title,
                                                                    messages: text.messages,
                                                                    prompt: text.prompt,
                                                                    turn_id: text.turn_id,
                                                                    sibling_turn_ids: text.sibling_turn_ids,
                                                                    attempt_placement: text.attempt_placement,
                                                                    attempt_status: text.attempt_status,
                                                                };
                                                                let _ = tx.send(evt);
                                                            }
                                                            Err(e2) => {
                                                                let _ = tx.send(app::AppEvent::DetailsFailed { id: diff_id, title: diff_title, error: format!("{e2}") });
                                                            }
                                                        }
                                                    }
                                                }
                                            });
                                        }
                                        // Also fetch conversation text even when diff exists
                                        {
                                            let backend = Arc::clone(&backend);
                                            let tx = tx.clone();
                                            let msg_id = id;
                                            let msg_title = title;
                                            tokio::spawn(async move {
                                                if let Ok(text) = codex_cloud_tasks_client::CloudBackend::get_task_text(&*backend, msg_id.clone()).await {
                                                    let evt = app::AppEvent::DetailsMessagesLoaded {
                                                        id: msg_id,
                                                        title: msg_title,
                                                        messages: text.messages,
                                                        prompt: text.prompt,
                                                        turn_id: text.turn_id,
                                                        sibling_turn_ids: text.sibling_turn_ids,
                                                        attempt_placement: text.attempt_placement,
                                                        attempt_status: text.attempt_status,
                                                    };
                                                    let _ = tx.send(evt);
                                                }
                                            });
                                        }
                                        // Animate spinner while details load.
                                        let _ = frame_tx.send(Instant::now() + Duration::from_millis(100));
                                    }
                                }
                                KeyCode::Char('a') => {
                                    if app.apply_inflight || app.apply_preflight_inflight {
                                        app.status = "Finish the current apply/preflight before starting another.".to_string();
                                        needs_redraw = true;
                                        continue;
                                    }

                                    if let Some(task) = app.tasks.get(app.selected).cloned() {
                                        match codex_cloud_tasks_client::CloudBackend::get_task_diff(&*backend, task.id.clone()).await {
                                            Ok(Some(diff)) => {
                                                let diff_override = Some(diff.clone());
                                                let task_id = task.id.clone();
                                                let title = task.title.clone();
                                                let job = ApplyJob {
                                                    task_id: task_id.clone(),
                                                    diff_override: diff_override.clone(),
                                                };
                                                if spawn_preflight(
                                                    &mut app,
                                                    &backend,
                                                    &tx,
                                                    &frame_tx,
                                                    title.clone(),
                                                    job,
                                                ) {
                                                    app.apply_modal = Some(app::ApplyModalState {
                                                        task_id,
                                                        title: title.clone(),
                                                        result_message: None,
                                                        result_level: None,
                                                        skipped_paths: Vec::new(),
                                                        conflict_paths: Vec::new(),
                                                        diff_override,
                                                    });
                                                    app.status = format!("Preflighting '{title}'...");
                                                }
                                            }
                                            Ok(None) | Err(_) => {
                                                app.status = "No diff available to apply".to_string();
                                            }
                                        }
                                        needs_redraw = true;
                                    }
                                }
                                _ => {}
                            }
                        }
                        // Render after handling a key event (when not quitting).
                        render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        needs_redraw = true;
                        // Redraw immediately on resize for snappier UX.
                        render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
                    }
                    Some(Err(_)) | None => {}
                    _ => {}
                }
                // Fallback: if any other event path requested a redraw, render now.
                render_if_needed(&mut terminal, &mut app, &mut needs_redraw)?;
            }
        }
    };

    // Restore terminal
    disable_raw_mode().ok();
    terminal.show_cursor().ok();
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    // Best-effort restore of keyboard enhancement flags before leaving alt screen.
    let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

// extract_chatgpt_account_id moved to util.rs

/// Build plain-text conversation lines: a labeled user prompt followed by assistant messages.
fn conversation_lines(prompt: Option<String>, messages: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(p) = prompt {
        out.push("user:".to_string());
        for l in p.lines() {
            out.push(l.to_string());
        }
        out.push(String::new());
    }
    if !messages.is_empty() {
        out.push("assistant:".to_string());
        for (i, m) in messages.iter().enumerate() {
            for l in m.lines() {
                out.push(l.to_string());
            }
            if i + 1 < messages.len() {
                out.push(String::new());
            }
        }
    }
    if out.is_empty() {
        out.push("<no output>".to_string());
    }
    out
}

/// Convert a verbose HTTP error with embedded JSON body into concise, user-friendly lines
/// for the details overlay. Falls back to a short raw message when parsing fails.
fn pretty_lines_from_error(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let is_no_diff = raw.contains("No output_diff in response.");
    let is_no_msgs = raw.contains("No assistant text messages in response.");
    if is_no_diff {
        lines.push("No diff available for this task.".to_string());
    } else if is_no_msgs {
        lines.push("No assistant messages found for this task.".to_string());
    } else {
        lines.push("Failed to load task details.".to_string());
    }

    // Try to parse the embedded JSON body: find the first '{' after " body=" and decode.
    if let Some(body_idx) = raw.find(" body=")
        && let Some(json_start_rel) = raw[body_idx..].find('{')
    {
        let json_start = body_idx + json_start_rel;
        let json_str = raw[json_start..].trim();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
            // Prefer assistant turn context.
            let turn = v
                .get("current_assistant_turn")
                .and_then(|x| x.as_object())
                .cloned()
                .or_else(|| {
                    v.get("current_diff_task_turn")
                        .and_then(|x| x.as_object())
                        .cloned()
                });
            if let Some(t) = turn {
                if let Some(err) = t.get("error").and_then(|e| e.as_object()) {
                    let code = err.get("code").and_then(|s| s.as_str()).unwrap_or("");
                    let msg = err.get("message").and_then(|s| s.as_str()).unwrap_or("");
                    if !code.is_empty() || !msg.is_empty() {
                        let summary = if code.is_empty() {
                            msg.to_string()
                        } else if msg.is_empty() {
                            code.to_string()
                        } else {
                            format!("{code}: {msg}")
                        };
                        lines.push(format!("Assistant error: {summary}"));
                    }
                }
                if let Some(status) = t.get("turn_status").and_then(|s| s.as_str()) {
                    lines.push(format!("Status: {status}"));
                }
                if let Some(text) = t
                    .get("latest_event")
                    .and_then(|e| e.get("text"))
                    .and_then(|s| s.as_str())
                    && !text.trim().is_empty()
                {
                    lines.push(format!("Latest event: {}", text.trim()));
                }
            }
        }
    }

    if lines.len() == 1 {
        // Parsing yielded nothing; include a trimmed, short raw message tail for context.
        let tail = if raw.len() > 320 {
            format!("{}…", &raw[..320])
        } else {
            raw.to_string()
        };
        lines.push(tail);
    } else if lines.len() >= 2 {
        // Add a hint to refresh when still in progress.
        if lines.iter().any(|l| l.contains("in_progress")) {
            lines.push("This task may still be running. Press 'r' to refresh.".to_string());
        }
        // Avoid an empty overlay
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use codex_tui::ComposerAction;
    use codex_tui::ComposerInput;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    #[test]
    fn composer_input_renders_typed_characters() {
        let mut composer = ComposerInput::new();
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        match composer.input(key) {
            ComposerAction::Submitted(_) => panic!("unexpected submission"),
            ComposerAction::None => {}
        }

        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        composer.render_ref(area, &mut buf);

        let found = buf.content().iter().any(|cell| cell.symbol() == "a");
        assert!(found, "typed character was not rendered: {buf:?}");

        composer.set_hint_items(vec![("⌃O", "env"), ("⌃C", "quit")]);
        composer.render_ref(area, &mut buf);
        let footer = buf
            .content()
            .iter()
            .skip((area.width as usize) * (area.height as usize - 1))
            .map(ratatui::buffer::Cell::symbol)
            .collect::<Vec<_>>()
            .join("");
        assert!(footer.contains("⌃O env"));
    }
}
