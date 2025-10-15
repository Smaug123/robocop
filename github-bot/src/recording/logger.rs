use super::types::RecordedEvent;
use anyhow::Result;
use std::path::PathBuf;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::{error, info};

pub struct RecordingLogger {
    sender: mpsc::UnboundedSender<RecordedEvent>,
}

impl RecordingLogger {
    pub fn new(log_file_path: PathBuf) -> Result<Self> {
        let (sender, mut receiver) = mpsc::unbounded_channel();

        // Spawn background task to write events to file
        tokio::spawn(async move {
            if let Err(e) = Self::writer_task(log_file_path, &mut receiver).await {
                error!("Recording logger failed: {}", e);
            }
        });

        Ok(Self { sender })
    }

    pub fn record(&self, event: RecordedEvent) {
        if self.sender.send(event).is_err() {
            error!("Failed to send event to recording logger: receiver dropped");
        }
    }

    /// Get a clone of the logger for use in middleware
    pub fn clone_for_middleware(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }

    async fn writer_task(
        log_file_path: PathBuf,
        receiver: &mut mpsc::UnboundedReceiver<RecordedEvent>,
    ) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = log_file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file_path)
            .await?;

        info!("Recording events to: {:?}", log_file_path);

        while let Some(event) = receiver.recv().await {
            match serde_json::to_string(&event) {
                Ok(json_line) => {
                    if let Err(e) = file.write_all(format!("{}\n", json_line).as_bytes()).await {
                        error!("Failed to write event to log: {}", e);
                        continue;
                    }
                    if let Err(e) = file.flush().await {
                        error!("Failed to flush log file: {}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to serialize event: {}", e);
                }
            }
        }

        info!("Recording writer task shutting down");

        Ok(())
    }
}
