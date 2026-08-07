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
/// The async hot path only enqueues events. A dedicated blocking writer owns all files.
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
            let mut last_error: Option<String> = None;

            while let Some(command) = rx.blocking_recv() {
                match command {
                    JournalCommand::Append(envelope) => {
                        if let Err(error) = append_event(&directory, &mut writers, &envelope) {
                            last_error = Some(error.to_string());
                        }
                    }
                    JournalCommand::Flush(reply) => {
                        let flush_result = flush_all(&mut writers);
                        let result = if let Some(error) = last_error.take() {
                            Err(AgentError::Storage(error))
                        } else {
                            flush_result
                        };
                        let _ = reply.send(result);
                    }
                }
            }

            let _ = flush_all(&mut writers);
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
