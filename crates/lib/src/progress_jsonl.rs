//! Output progress data using the json-lines format. For more information
//! see <https://jsonlines.org/>.

use anyhow::Result;
use canon_json::CanonJsonSerialize;
use schemars::JsonSchema;
use serde::Serialize;
use std::borrow::Cow;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::unix::pipe::Sender;
use tokio::sync::Mutex;

// Maximum number of times per second that an event will be written.
const REFRESH_HZ: u16 = 5;

/// Semantic version of the protocol.
const API_VERSION: &str = "0.2.0";

/// An incremental update to e.g. a container image layer download.
/// The first time a given "subtask" name is seen, a new progress bar should be created.
/// If bytes == bytes_total, then the subtask is considered complete.
#[derive(
    Debug, serde::Serialize, serde::Deserialize, Default, Clone, JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "camelCase")]
pub struct SubTaskBytes<'t> {
    /// A machine readable type for the task (used for i18n).
    /// (e.g., "ostree_chunk", "ostree_derived")
    #[serde(borrow)]
    pub subtask: Cow<'t, str>,
    /// A human readable description of the task if i18n is not available.
    /// (e.g., "OSTree Chunk:", "Derived Layer:")
    #[serde(borrow)]
    pub description: Cow<'t, str>,
    /// A human and machine readable identifier for the task
    /// (e.g., ostree chunk/layer hash).
    #[serde(borrow)]
    pub id: Cow<'t, str>,
    /// The number of bytes fetched by a previous run (e.g., zstd_chunked).
    pub bytes_cached: u64,
    /// Updated byte level progress
    pub bytes: u64,
    /// Total number of bytes
    pub bytes_total: u64,
}

/// Marks the beginning and end of a dictrete step
#[derive(
    Debug, serde::Serialize, serde::Deserialize, Default, Clone, JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "camelCase")]
pub struct SubTaskStep<'t> {
    /// A machine readable type for the task (used for i18n).
    /// (e.g., "ostree_chunk", "ostree_derived")
    #[serde(borrow)]
    pub subtask: Cow<'t, str>,
    /// A human readable description of the task if i18n is not available.
    /// (e.g., "OSTree Chunk:", "Derived Layer:")
    #[serde(borrow)]
    pub description: Cow<'t, str>,
    /// A human and machine readable identifier for the task
    /// (e.g., ostree chunk/layer hash).
    #[serde(borrow)]
    pub id: Cow<'t, str>,
    /// Starts as false when beginning to execute and turns true when completed.
    pub completed: bool,
}

/// The severity of a `Message` event.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageLevel {
    /// Regular status output, e.g. which image was queued for the next boot.
    Info,
    /// Something the user should pay attention to, which did not cause
    /// the operation to fail.
    Warning,
}

/// An event emitted as JSON.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "PascalCase",
    rename_all_fields = "camelCase"
)]
pub enum Event<'t> {
    Start {
        /// The semantic version of the progress protocol.
        #[serde(borrow)]
        version: Cow<'t, str>,
    },
    /// An incremental update to a container image layer download
    ProgressBytes {
        /// A machine readable type (e.g., pulling) for the task (used for i18n
        /// and UI customization).
        #[serde(borrow)]
        task: Cow<'t, str>,
        /// A human readable description of the task if i18n is not available.
        #[serde(borrow)]
        description: Cow<'t, str>,
        /// A human and machine readable unique identifier for the task
        /// (e.g., the image name). For tasks that only happen once,
        /// it can be set to the same value as task.
        #[serde(borrow)]
        id: Cow<'t, str>,
        /// The number of bytes fetched by a previous run.
        bytes_cached: u64,
        /// The number of bytes already fetched.
        bytes: u64,
        /// Total number of bytes. If zero, then this should be considered "unspecified".
        bytes_total: u64,
        /// The number of steps fetched by a previous run.
        steps_cached: u64,
        /// The initial position of progress.
        steps: u64,
        /// The total number of steps (e.g. container image layers, RPMs)
        steps_total: u64,
        /// The currently running subtasks.
        subtasks: Vec<SubTaskBytes<'t>>,
    },
    /// An incremental update with discrete steps
    ProgressSteps {
        /// A machine readable type (e.g., pulling) for the task (used for i18n
        /// and UI customization).
        #[serde(borrow)]
        task: Cow<'t, str>,
        /// A human readable description of the task if i18n is not available.
        #[serde(borrow)]
        description: Cow<'t, str>,
        /// A human and machine readable unique identifier for the task
        /// (e.g., the image name). For tasks that only happen once,
        /// it can be set to the same value as task.
        #[serde(borrow)]
        id: Cow<'t, str>,
        /// The number of steps fetched by a previous run.
        steps_cached: u64,
        /// The initial position of progress.
        steps: u64,
        /// The total number of steps (e.g. container image layers, RPMs)
        steps_total: u64,
        /// The currently running subtasks.
        subtasks: Vec<SubTaskStep<'t>>,
    },
    /// A human readable status message, with the same text that is
    /// printed to the terminal. The text is not a stable interface.
    Message {
        /// The severity of the message.
        level: MessageLevel,
        /// The message text, without the newline added by the terminal helper.
        #[serde(borrow)]
        text: Cow<'t, str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawProgressFd(RawFd);

impl FromStr for RawProgressFd {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let fd = s.parse::<u32>()?;
        // Sanity check
        if matches!(fd, 0..=2) {
            anyhow::bail!("Cannot use fd {fd} for progress JSON")
        }
        Ok(Self(fd.try_into()?))
    }
}

#[derive(Debug)]
struct ProgressWriterInner {
    /// true if we sent the initial Start message
    sent_start: bool,
    last_write: Option<std::time::Instant>,
    fd: BufWriter<Sender>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProgressWriter {
    inner: Arc<Mutex<Option<ProgressWriterInner>>>,
}

impl TryFrom<OwnedFd> for ProgressWriter {
    type Error = anyhow::Error;

    fn try_from(value: OwnedFd) -> Result<Self> {
        let value = Sender::from_owned_fd(value)?;
        Ok(Self::from(value))
    }
}

impl From<Sender> for ProgressWriter {
    fn from(value: Sender) -> Self {
        let inner = ProgressWriterInner {
            sent_start: false,
            last_write: None,
            fd: BufWriter::new(value),
        };
        Self {
            inner: Arc::new(Some(inner).into()),
        }
    }
}

impl TryFrom<RawProgressFd> for ProgressWriter {
    type Error = anyhow::Error;

    #[allow(unsafe_code)]
    fn try_from(fd: RawProgressFd) -> Result<Self> {
        unsafe { OwnedFd::from_raw_fd(fd.0) }.try_into()
    }
}

impl ProgressWriter {
    /// Print status to stdout and send a required Message event, if enabled.
    #[allow(dead_code)] // Foundation for subsequent caller conversions.
    pub(crate) async fn info(&self, text: impl AsRef<str>) {
        self.message(MessageLevel::Info, text.as_ref(), |text| {
            write_terminal(&mut std::io::stdout().lock(), text)
        })
        .await;
    }

    /// Print a warning to stderr and send a required Message event, if enabled.
    #[allow(dead_code)] // Foundation for subsequent caller conversions.
    pub(crate) async fn warning(&self, text: impl AsRef<str>) {
        self.message(MessageLevel::Warning, text.as_ref(), |text| {
            write_terminal(&mut std::io::stderr().lock(), text)
        })
        .await;
    }

    // Injectable terminal sink; keep potentially blocking terminal I/O off the
    // async executor. Like println!/eprintln!, terminal write failures panic.
    async fn message<F>(&self, level: MessageLevel, text: &str, terminal: F)
    where
        F: FnOnce(&str) -> std::io::Result<()> + Send + 'static,
    {
        let text = text.to_owned();
        let (text, result) = tokio::task::spawn_blocking(move || {
            let result = terminal(&text);
            (text, result)
        })
        .await
        .expect("terminal writer task panicked");
        result.unwrap_or_else(|e| {
            let stream = match level {
                MessageLevel::Info => "stdout",
                MessageLevel::Warning => "stderr",
            };
            panic!("failed printing to {stream}: {e}");
        });
        self.send(Event::Message {
            level,
            text: text.into(),
        })
        .await;
    }

    /// Serialize the target value as a single line of JSON and write it.
    async fn send_impl_inner<T: Serialize>(inner: &mut ProgressWriterInner, v: T) -> Result<()> {
        // canon_json is guaranteed not to output newlines here
        let buf = v.to_canon_json_vec()?;
        inner.fd.write_all(&buf).await?;
        // We always end in a newline
        inner.fd.write_all(b"\n").await?;
        // And flush to ensure the remote side sees updates immediately
        inner.fd.flush().await?;
        Ok(())
    }

    /// Serialize the target object to JSON as a single line
    pub(crate) async fn send_impl<T: Serialize>(&self, v: T, required: bool) -> Result<()> {
        let mut guard = self.inner.lock().await;
        // Check if we have an inner value; if not, nothing to do.
        let Some(inner) = guard.as_mut() else {
            return Ok(());
        };

        // If this is our first message, emit the Start message
        if !inner.sent_start {
            inner.sent_start = true;
            let start = Event::Start {
                version: API_VERSION.into(),
            };
            Self::send_impl_inner(inner, &start).await?;
        }

        // For messages that can be dropped, if we already sent an update within this cycle, discard this one.
        // TODO: Also consider querying the pipe buffer and also dropping if we can't do this write.
        let now = Instant::now();
        if !required {
            const REFRESH_MS: u32 = 1000 / REFRESH_HZ as u32;
            if let Some(elapsed) = inner.last_write.map(|w| now.duration_since(w)) {
                if elapsed.as_millis() < REFRESH_MS.into() {
                    return Ok(());
                }
            }
        }

        Self::send_impl_inner(inner, &v).await?;
        // Update the last write time
        inner.last_write = Some(now);
        Ok(())
    }

    /// Send an event.
    pub(crate) async fn send(&self, event: Event<'_>) {
        if let Err(e) = self.send_impl(event, true).await {
            eprintln!("Failed to write to jsonl: {e}");
            // Stop writing to fd but let process continue
            // SAFETY: Propagating panics from the mutex here is intentional
            let _ = self.inner.lock().await.take();
        }
    }

    /// Send an event that can be dropped.
    pub(crate) async fn send_lossy(&self, event: Event<'_>) {
        if let Err(e) = self.send_impl(event, false).await {
            eprintln!("Failed to write to jsonl: {e}");
            // Stop writing to fd but let process continue
            // SAFETY: Propagating panics from the mutex here is intentional
            let _ = self.inner.lock().await.take();
        }
    }

    /// Flush remaining data and return the underlying file.
    #[allow(dead_code)]
    pub(crate) async fn into_inner(self) -> Result<Option<Sender>> {
        // SAFETY: Propagating panics from the mutex here is intentional
        let mut mutex = self.inner.lock().await;
        if let Some(inner) = mutex.take() {
            Ok(Some(inner.fd.into_inner()))
        } else {
            Ok(None)
        }
    }
}

fn write_terminal(writer: &mut impl std::io::Write, text: &str) -> std::io::Result<()> {
    writeln!(writer, "{text}")
}

#[cfg(test)]
mod test {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    use tokio::io::{AsyncBufReadExt, BufReader};

    use super::*;

    const TEXTS: &[&str] = &["", "hello", "a\nb", "trailing\n\n", "日本語 🦀"];

    #[test]
    fn test_schema() -> Result<()> {
        let documented: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/src/progress-v0.schema.json"))?;
        assert_eq!(
            serde_json::to_value(schemars::schema_for!(Event))?,
            documented
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_messages() -> Result<()> {
        let (send, recv) = tokio::net::unix::pipe::pipe()?;
        let sender = async move {
            let writer = ProgressWriter::from(send);
            for text in TEXTS {
                for level in [MessageLevel::Info, MessageLevel::Warning] {
                    let expected = format!("{text}\n").into_bytes();
                    writer
                        .message(level, text, move |text| {
                            let mut bytes = Vec::new();
                            write_terminal(&mut bytes, text)?;
                            assert_eq!(bytes, expected);
                            Ok(())
                        })
                        .await;
                }
            }
        };
        let receiver = async move {
            let mut lines = BufReader::new(recv).lines();
            assert_eq!(
                lines.next_line().await?.as_deref(),
                Some(r#"{"type":"Start","version":"0.2.0"}"#)
            );
            for text in TEXTS {
                for level in [MessageLevel::Info, MessageLevel::Warning] {
                    let line = lines.next_line().await?.unwrap();
                    assert_eq!(
                        serde_json::from_str::<Event>(&line)?,
                        Event::Message {
                            level,
                            text: (*text).into()
                        }
                    );
                }
            }
            assert!(lines.next_line().await?.is_none());
            anyhow::Ok(())
        };
        let (_, result) = tokio::join!(sender, receiver);
        result
    }

    #[test]
    fn test_terminal_write_errors() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for text in TEXTS {
            assert_eq!(
                write_terminal(&mut Broken, text).unwrap_err().kind(),
                std::io::ErrorKind::BrokenPipe
            );
        }
    }

    // Run with libtest capture disabled so these exercise the real standard
    // streams, including the macros' panic/exit behavior on closed outputs.
    #[test]
    fn terminal_child() {
        let Ok(mode) = std::env::var("BOOTC_TERMINAL_TEST") else {
            return;
        };
        let text = std::env::var("BOOTC_TERMINAL_TEXT").unwrap();
        if std::env::var_os("BOOTC_TERMINAL_CLOSED").is_some() {
            // Let the parent close the selected output after libtest's banner.
            std::io::stdout().flush().unwrap();
            std::io::stdin().read_exact(&mut [0]).unwrap();
        }
        match mode.as_str() {
            "println" => println!("{text}"),
            "eprintln" => eprintln!("{text}"),
            "info" | "warning" => {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let writer = ProgressWriter::default();
                        if mode == "info" {
                            writer.info(&text).await;
                        } else {
                            writer.warning(&text).await;
                        }
                    });
            }
            _ => panic!("unknown terminal test mode"),
        }
        std::io::stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
        // Omit libtest's timing-dependent summary.
        std::process::exit(0);
    }

    #[test]
    fn test_terminal_subprocesses() -> Result<()> {
        let run = |mode: &str, text: &str, closed: bool| -> Result<std::process::Output> {
            let mut command = Command::new(std::env::current_exe()?);
            command
                .args([
                    "--exact",
                    "progress_jsonl::test::terminal_child",
                    "--nocapture",
                    "--quiet",
                ])
                .env("BOOTC_TERMINAL_TEST", mode)
                .env("BOOTC_TERMINAL_TEXT", text)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if closed {
                command.env("BOOTC_TERMINAL_CLOSED", "1");
            }
            let mut child = command.spawn()?;
            let handshake = if closed {
                let mut stdout = child.stdout.take().unwrap();
                let mut stdin = child.stdin.take().unwrap();
                if matches!(mode, "eprintln" | "warning") {
                    drop(child.stderr.take());
                }
                Some(std::thread::spawn(move || -> std::io::Result<()> {
                    // Read on a separate thread so the deadline also bounds
                    // failures before the child reaches its handshake.
                    let mut banner = Vec::new();
                    while !banner.ends_with(b"running 1 test\n") {
                        let mut byte = [0];
                        stdout.read_exact(&mut byte)?;
                        banner.push(byte[0]);
                    }
                    drop(stdout);
                    stdin.write_all(b"x")
                }))
            } else {
                None
            };
            let deadline = Instant::now() + std::time::Duration::from_secs(15);
            while child.try_wait()?.is_none() {
                if Instant::now() >= deadline {
                    child.kill()?;
                    child.wait()?;
                    if let Some(handshake) = handshake {
                        handshake.join().unwrap()?;
                    }
                    anyhow::bail!("terminal child timed out: {mode}");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if let Some(handshake) = handshake {
                handshake.join().unwrap()?;
            }
            Ok(child.wait_with_output()?)
        };
        for (original, helper) in [("println", "info"), ("eprintln", "warning")] {
            for text in TEXTS {
                let expected = run(original, text, false)?;
                let actual = run(helper, text, false)?;
                assert!(expected.status.success());
                assert_eq!(actual.status, expected.status);
                assert_eq!(actual.stdout, expected.stdout);
                assert_eq!(actual.stderr, expected.stderr);
                let expected = run(original, text, true)?;
                let actual = run(helper, text, true)?;
                assert!(!expected.status.success());
                assert_eq!(actual.status, expected.status);
                if helper == "info" {
                    assert!(
                        String::from_utf8_lossy(&actual.stderr)
                            .contains("failed printing to stdout")
                    );
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_jsonl() -> Result<()> {
        let testvalues = [
            Event::ProgressSteps {
                task: "sometask".into(),
                description: "somedesc".into(),
                id: "someid".into(),
                steps_cached: 0,
                steps: 0,
                steps_total: 3,
                subtasks: Vec::new(),
            },
            Event::ProgressBytes {
                task: "sometask".into(),
                description: "somedesc".into(),
                id: "someid".into(),
                bytes_cached: 0,
                bytes: 11,
                bytes_total: 42,
                steps_cached: 0,
                steps: 0,
                steps_total: 3,
                subtasks: Vec::new(),
            },
        ];
        let (send, recv) = tokio::net::unix::pipe::pipe()?;
        let testvalues_sender = testvalues.iter().cloned();
        let sender = async move {
            let w = ProgressWriter::try_from(send)?;
            for value in testvalues_sender {
                w.send(value).await;
            }
            anyhow::Ok(())
        };
        let testvalues = &testvalues;
        let receiver = async move {
            let tf = BufReader::new(recv);
            let mut expected = testvalues.iter();
            let mut lines = tf.lines();
            let mut got_first = false;
            while let Some(line) = lines.next_line().await? {
                let found: Event = serde_json::from_str(&line)?;
                let expected_value = if !got_first {
                    got_first = true;
                    &Event::Start {
                        version: API_VERSION.into(),
                    }
                } else {
                    expected.next().unwrap()
                };
                assert_eq!(&found, expected_value);
            }
            assert!(got_first);
            assert!(expected.next().is_none());
            anyhow::Ok(())
        };
        tokio::try_join!(sender, receiver)?;
        Ok(())
    }
}
