//! Output progress data using the json-lines format. For more information
//! see <https://jsonlines.org/>.

use anyhow::Result;
use canon_json::CanonJsonSerialize;
use schemars::JsonSchema;
use serde::Serialize;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
#[cfg(test)]
use tokio::net::unix::pipe::Sender;

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
    fd: BufWriter<File>,
}

/// Writes progress events as JSON lines to the file descriptor given
/// with `--progress-fd`, if any.
///
/// Writes are blocking so that this can be used from both sync and
/// async code. Events are small and the lossy ones are rate limited, so
/// this only blocks if the reader stops draining the pipe.
#[derive(Clone, Debug, Default)]
pub(crate) struct ProgressWriter {
    inner: Arc<Mutex<Option<ProgressWriterInner>>>,
}

impl From<OwnedFd> for ProgressWriter {
    fn from(value: OwnedFd) -> Self {
        let inner = ProgressWriterInner {
            sent_start: false,
            last_write: None,
            fd: BufWriter::new(File::from(value)),
        };
        Self {
            inner: Arc::new(Some(inner).into()),
        }
    }
}

#[cfg(test)]
impl TryFrom<Sender> for ProgressWriter {
    type Error = anyhow::Error;

    fn try_from(value: Sender) -> Result<Self> {
        Ok(value.into_blocking_fd()?.into())
    }
}

impl TryFrom<RawProgressFd> for ProgressWriter {
    type Error = anyhow::Error;

    #[allow(unsafe_code)]
    fn try_from(fd: RawProgressFd) -> Result<Self> {
        let fd = unsafe { OwnedFd::from_raw_fd(fd.0) };
        // We do blocking writes, so make sure the caller didn't hand us a
        // non-blocking fd, which would fail with EAGAIN when the pipe is full.
        let flags = rustix::fs::fcntl_getfl(&fd)?;
        if flags.contains(rustix::fs::OFlags::NONBLOCK) {
            rustix::fs::fcntl_setfl(&fd, flags - rustix::fs::OFlags::NONBLOCK)?;
        }
        Ok(fd.into())
    }
}

impl ProgressWriter {
    /// Serialize the target value as a single line of JSON and write it.
    fn send_impl_inner<T: Serialize>(inner: &mut ProgressWriterInner, v: T) -> Result<()> {
        // canon_json is guaranteed not to output newlines here
        let buf = v.to_canon_json_vec()?;
        inner.fd.write_all(&buf)?;
        // We always end in a newline
        inner.fd.write_all(b"\n")?;
        // And flush to ensure the remote side sees updates immediately
        inner.fd.flush()?;
        Ok(())
    }

    /// Serialize the target object to JSON as a single line
    fn send_impl<T: Serialize>(&self, v: T, required: bool) -> Result<()> {
        // SAFETY: Propagating panics from the mutex here is intentional
        let mut guard = self.inner.lock().unwrap();
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
            Self::send_impl_inner(inner, &start)?;
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

        Self::send_impl_inner(inner, &v)?;
        // Update the last write time
        inner.last_write = Some(now);
        Ok(())
    }

    /// Send an event.
    pub(crate) fn send(&self, event: Event<'_>) {
        self.send_or_disable(event, true)
    }

    /// Send an event that can be dropped.
    pub(crate) fn send_lossy(&self, event: Event<'_>) {
        self.send_or_disable(event, false)
    }

    fn send_or_disable(&self, event: Event<'_>, required: bool) {
        if let Err(e) = self.send_impl(event, required) {
            eprintln!("Failed to write to jsonl: {e}");
            // Stop writing to fd but let process continue
            // SAFETY: Propagating panics from the mutex here is intentional
            let _ = self.inner.lock().unwrap().take();
        }
    }
}

#[cfg(test)]
mod test {
    use tokio::io::{AsyncBufReadExt, BufReader};

    use super::*;

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
        // ProgressWriter does blocking writes, and the sender and reader
        // below run on the same thread, so the whole output must fit in
        // the pipe buffer or this test would deadlock.
        let (send, recv) = tokio::net::unix::pipe::pipe()?;
        let testvalues_sender = testvalues.iter().cloned();
        let sender = async move {
            let w = ProgressWriter::try_from(send)?;
            for value in testvalues_sender {
                w.send(value);
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
