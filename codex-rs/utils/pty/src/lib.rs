use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::Result;
use portable_pty::native_pty_system;
use portable_pty::CommandBuilder;
use portable_pty::PtySize;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

#[derive(Debug)]
pub struct ExecCommandSession {
    writer_tx: mpsc::Sender<Vec<u8>>,
    output_tx: broadcast::Sender<Vec<u8>>,
    killer: StdMutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    reader_handle: StdMutex<Option<JoinHandle<()>>>,
    writer_handle: StdMutex<Option<JoinHandle<()>>>,
    wait_handle: StdMutex<Option<JoinHandle<()>>>,
    exit_status: Arc<AtomicBool>,
    exit_code: Arc<StdMutex<Option<i32>>>,
}

impl ExecCommandSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        writer_tx: mpsc::Sender<Vec<u8>>,
        output_tx: broadcast::Sender<Vec<u8>>,
        killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
        reader_handle: JoinHandle<()>,
        writer_handle: JoinHandle<()>,
        wait_handle: JoinHandle<()>,
        exit_status: Arc<AtomicBool>,
        exit_code: Arc<StdMutex<Option<i32>>>,
    ) -> (Self, broadcast::Receiver<Vec<u8>>) {
        let initial_output_rx = output_tx.subscribe();
        (
            Self {
                writer_tx,
                output_tx,
                killer: StdMutex::new(Some(killer)),
                reader_handle: StdMutex::new(Some(reader_handle)),
                writer_handle: StdMutex::new(Some(writer_handle)),
                wait_handle: StdMutex::new(Some(wait_handle)),
                exit_status,
                exit_code,
            },
            initial_output_rx,
        )
    }

    pub fn writer_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.writer_tx.clone()
    }

    pub fn output_receiver(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    pub fn has_exited(&self) -> bool {
        self.exit_status.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code.lock().ok().and_then(|guard| *guard)
    }
}

impl Drop for ExecCommandSession {
    fn drop(&mut self) {
        if let Ok(mut killer_opt) = self.killer.lock() {
            if let Some(mut killer) = killer_opt.take() {
                let _ = killer.kill();
            }
        }

        if let Ok(mut h) = self.reader_handle.lock() {
            if let Some(handle) = h.take() {
                handle.abort();
            }
        }
        if let Ok(mut h) = self.writer_handle.lock() {
            if let Some(handle) = h.take() {
                handle.abort();
            }
        }
        if let Ok(mut h) = self.wait_handle.lock() {
            if let Some(handle) = h.take() {
                handle.abort();
            }
        }
    }
}

#[derive(Debug)]
pub struct SpawnedPty {
    pub session: ExecCommandSession,
    pub output_rx: broadcast::Receiver<Vec<u8>>,
    pub exit_rx: oneshot::Receiver<i32>,
}

pub async fn spawn_pty_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Result<SpawnedPty> {
    if program.is_empty() {
        anyhow::bail!("missing program for PTY spawn");
    }

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut command_builder = CommandBuilder::new(program);
    command_builder.cwd(cwd);
    command_builder.env_clear();
    for arg in args {
        command_builder.arg(arg);
    }
    for (key, value) in env {
        command_builder.env(key, value);
    }

    let mut child = pair.slave.spawn_command(command_builder)?;
    let killer = child.clone_killer();

    let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(128);
    let (output_tx, _) = broadcast::channel::<Vec<u8>>(256);

    let mut reader = pair.master.try_clone_reader()?;
    let output_tx_clone = output_tx.clone();
    let reader_handle: JoinHandle<()> = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8_192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = output_tx_clone.send(buf[..n].to_vec());
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            }
        }
    });

    let writer = pair.master.take_writer()?;
    let writer = Arc::new(TokioMutex::new(writer));
    let writer_handle: JoinHandle<()> = tokio::spawn({
        let writer = Arc::clone(&writer);
        async move {
            while let Some(bytes) = writer_rx.recv().await {
                let mut guard = writer.lock().await;
                use std::io::Write;
                let _ = guard.write_all(&bytes);
                let _ = guard.flush();
            }
        }
    });

    let (exit_tx, exit_rx) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_handle: JoinHandle<()> = tokio::task::spawn_blocking(move || {
        let code = match child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => -1,
        };
        wait_exit_status.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = wait_exit_code.lock() {
            *guard = Some(code);
        }
        let _ = exit_tx.send(code);
    });

    let (session, output_rx) = ExecCommandSession::new(
        writer_tx,
        output_tx,
        killer,
        reader_handle,
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
    );

    Ok(SpawnedPty {
        session,
        output_rx,
        exit_rx,
    })
}
