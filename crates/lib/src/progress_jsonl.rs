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
const API_VERSION: &str = "0.1.0";

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

#[cfg(test)]
mod test {
    use std::future::Future;
    use std::process::{Child, Command};
    use std::time::Duration;

    use anyhow::Context;
    use tokio::io::{AsyncBufReadExt, BufReader};

    use super::*;

    // The deadline lives outside the runtime under test: a blocking write can
    // prevent even a Tokio timeout from being polled on a current-thread runtime.
    const CHILD_DEADLINE: Duration = Duration::from_secs(15);
    const CHILD_TEST: &str = "progress_jsonl::test::test_backpressure_child";

    struct TestChild(Child);

    impl Drop for TestChild {
        fn drop(&mut self) {
            // Also reap on assertion failure or an error in the parent.
            if let Err(e) = self.0.kill() {
                eprintln!("Progress test child kill: {e}");
            }
            if let Err(e) = self.0.wait() {
                eprintln!("Progress test child wait: {e}");
            }
        }
    }

    #[test]
    fn test_backpressure() -> Result<()> {
        let child = Command::new(std::env::current_exe()?)
            .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
            .spawn()?;
        let mut child = TestChild(child);
        let deadline = Instant::now() + CHILD_DEADLINE;
        loop {
            if let Some(status) = child.0.try_wait()? {
                anyhow::ensure!(status.success(), "Progress test child failed: {status}");
                return Ok(());
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "Progress test child exceeded {CHILD_DEADLINE:?}; runtime may be blocked"
            );
            // Only parent-side process polling uses a delay; child synchronization
            // uses futures and channels, never timing assumptions.
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn required_event(steps: u64, padding: usize) -> Event<'static> {
        Event::ProgressSteps {
            task: "backpressure".into(),
            description: "x".repeat(padding).into(),
            id: "ordered".into(),
            steps_cached: 0,
            steps,
            steps_total: 3,
            subtasks: Vec::new(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "run by test_backpressure with an external deadline"]
    async fn test_backpressure_child() -> Result<()> {
        let (send, recv) = tokio::net::unix::pipe::pipe()?;
        // Keep both payloads and the number of required events bounded, even on
        // hosts with unusually large default pipe capacities or page sizes.
        let capacity = rustix::pipe::fcntl_setpipe_size(&send, 4096)?;
        anyhow::ensure!(capacity <= 65536, "Unexpected pipe capacity {capacity}");
        let start = Event::Start {
            version: API_VERSION.into(),
        };
        let overhead =
            start.to_canon_json_vec()?.len() + required_event(0, 0).to_canon_json_vec()?.len() + 2; // The two JSONL newlines.
        let padding = capacity.checked_sub(overhead).context("Pipe too small")?;
        let events = [
            required_event(0, padding),
            required_event(1, 1024),
            required_event(2, 1024),
        ];
        let writer = ProgressWriter::from(send);
        // Start plus the first required event exactly fill the real kernel pipe.
        // The reader remains open, but no task reads it until the probe succeeds.
        writer.send(events[0].clone()).await;
        assert_eq!(rustix::io::ioctl_fionread(&recv)?, capacity as u64);
        let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
        let producer_events = events.clone();
        let producer = tokio::spawn(async move {
            {
                let pending = writer.send(producer_events[1].clone());
                tokio::pin!(pending);
                let mut blocked_tx = Some(blocked_tx);
                std::future::poll_fn(|cx| {
                    let result = pending.as_mut().poll(cx);
                    if result.is_pending() {
                        if let Some(tx) = blocked_tx.take() {
                            tx.send(()).expect("probe receiver alive");
                        }
                    }
                    result
                })
                .await;
            }
            writer.send(producer_events[2].clone()).await;
            // Exercise explicit shutdown as well as dropping the last writer.
            let mut sender = writer.into_inner().await?.context("Writer disabled")?;
            sender.shutdown().await?;
            anyhow::Ok(())
        });
        let probe = tokio::spawn(async move {
            blocked_rx.await.context("send did not become pending")?;
            assert_eq!(rustix::io::ioctl_fionread(&recv)?, capacity as u64);
            anyhow::Ok(recv)
        });
        // This independent task must run while the required send is still
        // pending. Only now release the reader; completion while full is not required.
        let recv = probe.await??;
        let mut lines = BufReader::new(recv).lines();
        for expected in std::iter::once(&start).chain(events.iter()) {
            let line = lines.next_line().await?.context("Missing JSON line")?;
            assert_eq!(serde_json::from_str::<Event>(&line)?, *expected);
        }
        assert!(lines.next_line().await?.is_none());
        producer.await??;

        // Reader closure must disable the writer, close its fd, and make later
        // required and lossy sends harmless, including through a surviving clone.
        let (send, recv) = tokio::net::unix::pipe::pipe()?;
        let writer = ProgressWriter::from(send);
        let clone = writer.clone();
        drop(recv);
        writer.send(required_event(0, 0)).await;
        assert!(writer.inner.lock().await.is_none());
        clone.send(required_event(1, 0)).await;
        clone.send_lossy(required_event(2, 0)).await;
        assert!(clone.inner.lock().await.is_none());
        drop(writer);
        assert!(clone.into_inner().await?.is_none());

        // An ordinary drop must close the pipe too, even without any sends.
        let (send, recv) = tokio::net::unix::pipe::pipe()?;
        drop(ProgressWriter::from(send));
        assert!(BufReader::new(recv).lines().next_line().await?.is_none());
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
            anyhow::Ok(())
        };
        tokio::try_join!(sender, receiver)?;
        Ok(())
    }
}
