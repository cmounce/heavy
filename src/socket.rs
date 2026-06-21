use std::fs::symlink_metadata;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::ml::recorder::Recorder;

pub async fn run_debug_socket(path: String, recorder: Arc<Recorder>) {
    // Remove old socket file if left over from previous run
    let path = PathBuf::from(path);
    if let Ok(metadata) = symlink_metadata(&path) {
        if metadata.file_type().is_socket() {
            std::fs::remove_file(&path).unwrap_or_else(|e| {
                panic!("couldn't remove stale debug socket {}: {e}", path.display())
            });
        }
    }

    // Bind to new socket
    let socket = UnixListener::bind(&path)
        .unwrap_or_else(|e| panic!("couldn't bind debug socket {}: {e}", path.display()));
    eprintln!("debug socket listening on {}", path.display());

    // Respond to queries
    loop {
        let stream = match socket.accept().await {
            Ok((stream, _addr)) => stream,
            Err(e) => {
                eprintln!("debug socket accept failed: {e}");
                continue;
            }
        };
        let recorder = recorder.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_command(stream, recorder).await {
                eprintln!("debug socket client failed: {e}");
            }
        });
    }
}

async fn handle_command<S>(stream: S, recorder: Arc<Recorder>) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut stream = BufReader::new(stream);
    loop {
        let mut line = String::new();
        let num_read = stream.read_line(&mut line).await?;
        if num_read == 0 {
            break;
        }

        let command = line.trim();
        let mut args = command.split_whitespace();
        if let Some(verb) = args.next() {
            let reply = match verb {
                "hello" | "hi" => "Hello, world!\n".into(),
                // Sample N requests as ML feature vectors. Blocks client until sample is complete.
                "sample" => match args.next().and_then(|n| n.parse::<usize>().ok()) {
                    Some(size) => match recorder.start_sample(size).await {
                        Ok(sample) => {
                            let mut out = format!("collected {} samples:\n", sample.len());
                            for features in &sample {
                                out.push_str(&format!("  {features:?}\n"));
                            }
                            out
                        }
                        Err(_) => "error: sample cancelled\n".into(),
                    },
                    None => "usage: sample <n>\n".into(),
                },
                _ => format!("error: unknown command: {command}\n"),
            };
            stream.write_all(&reply.as_bytes()).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test]
    async fn handles_line_commands() {
        let (mut client, server) = tokio::io::duplex(1024);
        let task = tokio::spawn(handle_command(server, Arc::new(Recorder::new())));

        client.write_all(b"hello\n foo bar \n").await.unwrap();
        client.shutdown().await.unwrap();

        let mut output = String::new();
        client.read_to_string(&mut output).await.unwrap();
        assert_snapshot!(output, @"
        Hello, world!
        error: unknown command: foo bar
        ");

        task.await.unwrap().unwrap();
    }
}
