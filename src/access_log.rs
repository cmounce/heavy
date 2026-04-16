use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// Handle to the background access-log writer task.
#[derive(Clone)]
pub struct AccessLog {
    tx: mpsc::UnboundedSender<LogCmd>,
}

/// An action that can be sent to the access-log task.
enum LogCmd {
    /// Record a new log entry
    Append(String),

    /// Close and reopen the log file for rotation purposes
    Reopen,
}

impl AccessLog {
    /// Spawn a dedicated access-log writer task.
    ///
    /// Opens the log file in append mode. Returns a handle which can be used to send logs to that
    /// file. Logs are serialized in the order they are received by the writer task.
    pub async fn open(path: &str) -> Self {
        // Helper: open file for append and create a BufWriter for it.
        let get_writer = async |path: &str| {
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
                .and_then(|file| Ok(tokio::io::BufWriter::new(file)))
        };

        // Make sure we can open the access log file before spawning the task
        let writer = get_writer(path)
            .await
            .unwrap_or_else(|e| panic!("couldn't open log file {path}: {e}"));

        let path = path.to_owned(); // needs to outlive initial call to open()
        let (tx, mut rx) = mpsc::unbounded_channel::<LogCmd>();
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(msg) = rx.recv().await {
                match msg {
                    LogCmd::Append(line) => {
                        if let Err(e) = async {
                            writer.write_all(line.as_bytes()).await?;
                            writer.write_all(b"\n").await?;
                            writer.flush().await
                        }
                        .await
                        {
                            eprintln!("heavy: log write failed: {e}");
                        }
                    }
                    LogCmd::Reopen => {
                        let _ = writer.flush().await;
                        match get_writer(&path).await {
                            Ok(new_writer) => {
                                eprintln!("heavy: reopened log file {path}");
                                writer = new_writer;
                            }
                            Err(e) => eprintln!("heavy: couldn't reopen log file {path}: {e}"),
                        }
                    }
                }
            }
        });

        AccessLog { tx }
    }

    /// Write a line to the access log.
    pub fn append(&self, line: String) {
        let _ = self.tx.send(LogCmd::Append(line));
    }

    /// Close and reopen the log file.
    ///
    /// This is for logrotate compatibility and should be called on SIGHUP.
    pub fn reopen(&self) {
        let _ = self.tx.send(LogCmd::Reopen);
    }
}
