#![allow(clippy::module_inception)]

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::task::JoinHandle;
use tokio::time::Duration;

use crate::exec::ExecToolCallOutput;
use crate::exec::SandboxType;
use crate::exec::StreamOutput;
use crate::exec::is_likely_sandbox_denied;
use crate::truncate::truncate_middle;
use codex_utils_pty::ExecCommandSession;
use codex_utils_pty::SpawnedPty;

use super::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use super::UnifiedExecError;

#[derive(Debug, Default)]
pub(crate) struct OutputBufferState {
    chunks: VecDeque<Vec<u8>>,
    pub(crate) total_bytes: usize,
}

impl OutputBufferState {
    pub(super) fn push_chunk(&mut self, chunk: Vec<u8>) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len());
        self.chunks.push_back(chunk);

        let mut excess = self
            .total_bytes
            .saturating_sub(UNIFIED_EXEC_OUTPUT_MAX_BYTES);

        while excess > 0 {
            match self.chunks.front_mut() {
                Some(front) if excess >= front.len() => {
                    excess -= front.len();
                    self.total_bytes = self.total_bytes.saturating_sub(front.len());
                    self.chunks.pop_front();
                }
                Some(front) => {
                    front.drain(..excess);
                    self.total_bytes = self.total_bytes.saturating_sub(excess);
                    break;
                }
                None => break,
            }
        }
    }

    pub(super) fn drain(&mut self) -> Vec<Vec<u8>> {
        let drained: Vec<Vec<u8>> = self.chunks.drain(..).collect();
        self.total_bytes = 0;
        drained
    }

    pub(super) fn snapshot(&self) -> Vec<Vec<u8>> {
        self.chunks.iter().cloned().collect()
    }
}

pub(crate) type OutputBuffer = Arc<Mutex<OutputBufferState>>;
pub(crate) type OutputHandles = (OutputBuffer, Arc<Notify>);

#[derive(Debug)]
pub(crate) struct UnifiedExecSession {
    session: ExecCommandSession,
    output_buffer: OutputBuffer,
    output_notify: Arc<Notify>,
    output_task: JoinHandle<()>,
    sandbox_type: SandboxType,
}

impl UnifiedExecSession {
    pub(super) fn new(
        session: ExecCommandSession,
        initial_output_rx: tokio::sync::broadcast::Receiver<Vec<u8>>,
        sandbox_type: SandboxType,
    ) -> Self {
        let output_buffer = Arc::new(Mutex::new(OutputBufferState::default()));
        let output_notify = Arc::new(Notify::new());
        let mut receiver = initial_output_rx;
        let buffer_clone = Arc::clone(&output_buffer);
        let notify_clone = Arc::clone(&output_notify);
        let output_task = tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(chunk) => {
                        let mut guard = buffer_clone.lock().await;
                        guard.push_chunk(chunk);
                        drop(guard);
                        notify_clone.notify_waiters();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Self {
            session,
            output_buffer,
            output_notify,
            output_task,
            sandbox_type,
        }
    }

    pub(super) fn writer_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.session.writer_sender()
    }

    pub(super) fn output_handles(&self) -> OutputHandles {
        (
            Arc::clone(&self.output_buffer),
            Arc::clone(&self.output_notify),
        )
    }

    pub(super) fn has_exited(&self) -> bool {
        self.session.has_exited()
    }

    pub(super) fn exit_code(&self) -> Option<i32> {
        self.session.exit_code()
    }

    async fn snapshot_output(&self) -> Vec<Vec<u8>> {
        let guard = self.output_buffer.lock().await;
        guard.snapshot()
    }

    fn sandbox_type(&self) -> SandboxType {
        self.sandbox_type
    }

    pub(super) async fn check_for_sandbox_denial(&self) -> Result<(), UnifiedExecError> {
        if self.sandbox_type() == SandboxType::None || !self.has_exited() {
            return Ok(());
        }

        let _ =
            tokio::time::timeout(Duration::from_millis(20), self.output_notify.notified()).await;

        let collected_chunks = self.snapshot_output().await;
        let mut aggregated: Vec<u8> = Vec::new();
        for chunk in collected_chunks {
            aggregated.extend_from_slice(&chunk);
        }
        let aggregated_text = String::from_utf8_lossy(&aggregated).to_string();
        let exit_code = self.exit_code().unwrap_or(-1);

        let exec_output = ExecToolCallOutput {
            exit_code,
            stdout: StreamOutput::new(aggregated_text.clone()),
            stderr: StreamOutput::new(String::new()),
            aggregated_output: StreamOutput::new(aggregated_text.clone()),
            duration: Duration::ZERO,
            timed_out: false,
        };

        if is_likely_sandbox_denied(self.sandbox_type(), &exec_output) {
            let (snippet, _) = truncate_middle(&aggregated_text, UNIFIED_EXEC_OUTPUT_MAX_BYTES);
            let message = if snippet.is_empty() {
                format!("exit code {exit_code}")
            } else {
                snippet
            };
            return Err(UnifiedExecError::sandbox_denied(message, exec_output));
        }

        Ok(())
    }

    pub(super) async fn from_spawned(
        spawned: SpawnedPty,
        sandbox_type: SandboxType,
    ) -> Result<Self, UnifiedExecError> {
        let SpawnedPty {
            session,
            output_rx,
            mut exit_rx,
        } = spawned;
        let managed = Self::new(session, output_rx, sandbox_type);

        let exit_ready = match exit_rx.try_recv() {
            Ok(_) | Err(TryRecvError::Closed) => true,
            Err(TryRecvError::Empty) => false,
        };

        if exit_ready {
            managed.check_for_sandbox_denial().await?;
            return Ok(managed);
        }

        tokio::pin!(exit_rx);
        if tokio::time::timeout(Duration::from_millis(50), &mut exit_rx)
            .await
            .is_ok()
        {
            managed.check_for_sandbox_denial().await?;
        }

        Ok(managed)
    }
}

impl Drop for UnifiedExecSession {
    fn drop(&mut self) {
        self.output_task.abort();
    }
}
