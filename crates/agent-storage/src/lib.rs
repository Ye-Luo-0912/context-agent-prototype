use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use agent_contracts::{AgentError, AgentResult, EventJournal, RunId, RuntimeEventEnvelope};
use tokio::sync::{mpsc, oneshot};

const JOURNAL_BUFFER: usize = 4_096;

enum JournalCommand {
    Append(Box<RuntimeEventEnvelope>),
    Flush(oneshot::Sender<AgentResult<()>>),
}

/// Append-only JSONL trace storage.
///
/// The async hot path only enqueues events. A dedicated blocking writer owns
/// all files. `flush` is the *durability barrier*: because the channel is
/// FIFO, a successful `flush` guarantees every event appended before it has
/// left the process (the writer drained and flushed each `BufWriter` to the
/// OS), which is the turn-commit durability contract — events buffered in
/// userspace or in the pipe are not durable until the barrier passes.
/// Writer errors are sticky: the first failed write poisons the writer, and
/// every later barrier reports that error instead of pretending the trace
/// is intact.
pub struct FileEventJournal {
    tx: mpsc::Sender<JournalCommand>,
}

impl FileEventJournal {
    pub async fn open(directory: impl AsRef<Path>) -> AgentResult<Self> {
        let directory = directory.as_ref().to_path_buf();
        tokio::task::spawn_blocking({
            let directory = directory.clone();
            move || {
                fs::create_dir_all(&directory)
                    .map_err(|e| AgentError::Storage(format!("create trace directory: {e}")))
            }
        })
        .await
        .map_err(|e| AgentError::Storage(format!("trace init task: {e}")))??;

        let (tx, mut rx) = mpsc::channel::<JournalCommand>(JOURNAL_BUFFER);
        tokio::task::spawn_blocking(move || {
            let mut writers: HashMap<RunId, BufWriter<File>> = HashMap::new();
            // Sticky failure: once any write fails, every later barrier
            // reports it and the writer stops touching files — a trace with
            // a gap must never be mistaken for a complete one, and a broken
            // `BufWriter` is not safe to reuse. Appends after the failure
            // are still drained from the channel (the sequence stays
            // consistent) but dropped.
            let mut failed: Option<String> = None;

            while let Some(command) = rx.blocking_recv() {
                match command {
                    JournalCommand::Append(envelope) => {
                        if failed.is_none()
                            && let Err(error) = append_event(&directory, &mut writers, &envelope)
                        {
                            failed = Some(error.to_string());
                        }
                    }
                    JournalCommand::Flush(reply) => {
                        let result = match &failed {
                            Some(error) => Err(AgentError::Storage(error.clone())),
                            None => match flush_all(&mut writers) {
                                Ok(()) => Ok(()),
                                Err(error) => {
                                    failed = Some(error.to_string());
                                    Err(error)
                                }
                            },
                        };
                        let _ = reply.send(result);
                    }
                }
            }

            if failed.is_none() {
                let _ = flush_all(&mut writers);
            }
        });

        Ok(Self { tx })
    }
}

#[async_trait::async_trait]
impl EventJournal for FileEventJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        self.tx
            .send(JournalCommand::Append(Box::new(envelope.clone())))
            .await
            .map_err(|_| AgentError::Storage("event journal writer stopped".into()))
    }

    async fn flush(&self) -> AgentResult<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(JournalCommand::Flush(tx))
            .await
            .map_err(|_| AgentError::Storage("event journal writer stopped".into()))?;
        rx.await
            .map_err(|_| AgentError::Storage("event journal flush failed".into()))?
    }
}

fn append_event(
    directory: &Path,
    writers: &mut HashMap<RunId, BufWriter<File>>,
    envelope: &RuntimeEventEnvelope,
) -> AgentResult<()> {
    match writers.entry(envelope.run_id) {
        std::collections::hash_map::Entry::Occupied(_) => {}
        std::collections::hash_map::Entry::Vacant(entry) => {
            let path = trace_path(directory, envelope.run_id);
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| AgentError::Storage(format!("open trace {}: {e}", path.display())))?;
            entry.insert(BufWriter::with_capacity(64 * 1024, file));
        }
    }

    let writer = writers
        .get_mut(&envelope.run_id)
        .ok_or_else(|| AgentError::Storage("trace writer disappeared".into()))?;
    serde_json::to_writer(&mut *writer, envelope)
        .map_err(|e| AgentError::Storage(format!("serialize trace event: {e}")))?;
    writer
        .write_all(b"\n")
        .map_err(|e| AgentError::Storage(format!("append trace event: {e}")))?;
    Ok(())
}

fn flush_all(writers: &mut HashMap<RunId, BufWriter<File>>) -> AgentResult<()> {
    for writer in writers.values_mut() {
        writer
            .flush()
            .map_err(|e| AgentError::Storage(format!("flush trace: {e}")))?;
    }
    Ok(())
}

fn trace_path(directory: &Path, run_id: RunId) -> PathBuf {
    directory.join(format!("{run_id}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{RunId, RuntimeEvent};

    fn envelope(run_id: RunId) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id,
            seq: 1,
            timestamp_ms: 0,
            event: RuntimeEvent::RunStarted,
        }
    }

    fn trace_lines(dir: &Path, run_id: RunId) -> usize {
        std::fs::read_to_string(trace_path(dir, run_id))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    /// `flush` is the durability barrier: because the command channel is
    /// FIFO, a successful flush guarantees every append sent before it has
    /// left the process (drained and flushed out of the `BufWriter`s), and
    /// the trace file reflects them.
    #[tokio::test]
    async fn flush_is_a_durability_barrier_over_prior_appends() {
        let dir = std::env::temp_dir().join(format!("journal-barrier-{}", RunId::new()));
        let journal = FileEventJournal::open(&dir).await.unwrap();
        let run = RunId::new();

        for _ in 0..8 {
            journal.append(&envelope(run)).await.unwrap();
        }
        // Nothing durable until the barrier: the file may not exist yet.
        assert!(trace_lines(&dir, run) <= 8);

        journal.flush().await.expect("barrier must succeed");
        assert_eq!(
            trace_lines(&dir, run),
            8,
            "the barrier must have written every prior append"
        );

        // Appends after the barrier are covered by the next one (FIFO).
        journal.append(&envelope(run)).await.unwrap();
        journal.flush().await.unwrap();
        assert_eq!(trace_lines(&dir, run), 9);

        drop(journal);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writer errors are sticky: once a write fails, every later barrier
    /// reports that failure (never cleared, never mistaken for a complete
    /// trace). A directory squatting on a trace path makes the writer's
    /// open fail on every platform (is-a-directory on unix,
    /// access-denied on windows).
    #[tokio::test]
    async fn writer_errors_are_sticky_at_the_next_barrier() {
        let dir = std::env::temp_dir().join(format!("journal-sticky-{}", RunId::new()));
        fs::create_dir_all(&dir).unwrap();
        let journal = FileEventJournal::open(&dir).await.unwrap();

        let good_run = RunId::new();
        journal.append(&envelope(good_run)).await.unwrap();

        // The next trace path is a directory: the writer cannot open it.
        let bad_run = RunId::new();
        fs::create_dir(trace_path(&dir, bad_run)).unwrap();
        journal.append(&envelope(bad_run)).await.unwrap();

        let first = journal
            .flush()
            .await
            .expect_err("the barrier must surface the write failure");
        assert!(
            first.to_string().contains("open trace"),
            "the failure must name the trace open, got: {first}"
        );
        let second = journal
            .flush()
            .await
            .expect_err("the error is sticky, not cleared");
        assert_eq!(
            second.to_string(),
            first.to_string(),
            "a later barrier must report the same sticky failure"
        );

        drop(journal);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Crash-immediately-after-commit shape: the barrier (flush) is the
    /// crash point. Events past the last barrier live only in the writer's
    /// userspace `BufWriter` and are invisible on disk — a process killed
    /// between barriers loses exactly that tail, never the flushed prefix.
    #[tokio::test]
    async fn events_are_not_durable_until_the_barrier() {
        let dir = std::env::temp_dir().join(format!("journal-crash-{}", RunId::new()));
        fs::create_dir_all(&dir).unwrap();
        let run = RunId::new();

        let journal = FileEventJournal::open(&dir).await.unwrap();
        journal.append(&envelope(run)).await.unwrap();
        journal.append(&envelope(run)).await.unwrap();
        // Barrier 1: two events are durable and visible on disk.
        journal.flush().await.unwrap();
        assert_eq!(trace_lines(&dir, run), 2);

        // Two more events ride the writer's buffer only: until the next
        // barrier they are not on disk, so a crash loses them while the
        // flushed prefix survives.
        journal.append(&envelope(run)).await.unwrap();
        journal.append(&envelope(run)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            trace_lines(&dir, run),
            2,
            "the buffered tail must be invisible on disk until the barrier"
        );

        // The next barrier makes them durable.
        journal.flush().await.unwrap();
        assert_eq!(trace_lines(&dir, run), 4);

        drop(journal);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
