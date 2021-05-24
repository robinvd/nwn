use std::{
    collections::HashMap,
    io,
    process::Stdio,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use futures::{future, Stream, StreamExt, TryStreamExt};
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::UnixListener,
};
use tower_lsp::lsp_types::TextEdit;

use crate::{
    protocol::{Frame, RawEntry, RawEntryInner, RawEntryKind},
    util::time,
};

/// Information for how to run a NwoN runtime
#[derive(Debug, Deserialize, Clone)]
pub struct Runner {
    /// The command to run
    pub command: String,
    /// The arguments to run it with
    pub args: Vec<String>,
}

impl Runner {
    /// Execute this runtime and collect the data
    pub async fn run(
        &self,
        path: &str,
        input: ropey::Rope,
    ) -> io::Result<impl Stream<Item = io::Result<RawEntry>>> {
        log::info!("run2");

        let mut dir = std::env::temp_dir();
        dir.push(&format!(
            "nwn_conn{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));

        log::info!("path {:?}", dir);
        let mut listener = UnixListener::bind(&dir)?;

        log::info!("opened");
        let mut command = tokio::process::Command::new(&self.command);
        let mut handle = command
            .env("NWN_FILE_PATH", path)
            .env("NWN_CONNECTION_FD", &dir)
            .args(&self.args)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        log::error!("spawned runtime:{}", time());

        let stdout = handle.stdout.take().unwrap();
        let stderr = handle.stderr.take().unwrap();

        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr).lines();

            while let Some(line) = reader.next().await {
                log::error!("inject msg: {:?}", line);
            }
        });
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout).lines();

            while let Some(line) = reader.next().await {
                log::warn!("inject msg: {:?}", line);
            }
        });

        tokio::spawn(async move {
            handle
                .await
                .map_err(|err| log::error!("err: {:?}", err))
                .ok();

            log::error!("runtime finished:{}", time())
        });

        let (right, _) = listener.accept().await?;
        let (read, mut write) = tokio::io::split(right);

        tokio::spawn(async move {
            for part in input.chunks() {
                let result = write.write_all(part.as_bytes()).await;
                if result.is_err() {
                    break;
                }
            }

            write.write_all(&[0]).await.ok();

            log::error!("finished contents write:{}", time());
        });

        log::info!("start loop");

        Ok(tokio::io::BufReader::new(read)
            .lines()
            .inspect(|line| log::info!("line: {:?}", line))
            .try_filter(|line| future::ready(!line.trim().is_empty()))
            .try_filter_map(|line| async move {
                log::info!("got line: {:?}", line);
                let entry = RawEntry::from_json(line.as_bytes());
                match entry {
                    Ok(entry) => Ok(Some(entry)),
                    Err(err) => {
                        log::error!("reading line json err: {:?}", err);
                        Ok(None)
                    }
                }
            }))
    }
}

#[derive(Debug, Default)]
pub struct RunData {
    pub entries: HashMap<Frame, RunEntry>,
    pub hovers: HashMap<Frame, RunEntry>,
    pub changes: Vec<TextEdit>,
}

#[derive(Debug)]
pub struct RunEntry {
    insert_type: Option<String>,
    pub out: Vec<Arc<String>>,
}

impl RunData {
    pub async fn from_raw(mut raws: impl Stream<Item = RawEntry> + Unpin, path: &str) -> Self {
        let mut new_entries = HashMap::new();
        let mut new_hovers = HashMap::new();
        let mut changes = Vec::new();
        while let Some(entry) = raws.next().await {
            // for entry in raws {
            match entry.inner {
                RawEntryInner::Simple { out, kind } => {
                    let text = Arc::new(out);
                    let kind = kind.unwrap_or(RawEntryKind::Output);

                    match kind {
                        RawEntryKind::Output => {
                            if let Some(frame) =
                                entry.frames.into_iter().find(|frame| frame.file == path)
                            {
                                let run_entry = new_entries.entry(frame).or_insert(RunEntry {
                                    out: Vec::new(),
                                    insert_type: None,
                                });
                                run_entry.out.push(text.clone())
                            }
                        }
                        RawEntryKind::Debug => {
                            if let Some(frame) = entry
                                .frames
                                .into_iter()
                                .rev()
                                .find(|frame| frame.file == path)
                            {
                                let run_entry = new_entries.entry(frame).or_insert(RunEntry {
                                    out: Vec::new(),
                                    insert_type: None,
                                });
                                run_entry.out.push(text.clone())
                            }
                        }
                        RawEntryKind::Error => {
                            for frame in entry.frames.into_iter() {
                                let run_entry = new_hovers.entry(frame).or_insert(RunEntry {
                                    out: Vec::new(),
                                    insert_type: None,
                                });
                                run_entry.out.push(text.clone())
                            }
                        }
                        RawEntryKind::Regex => {
                            if let Some(frame) =
                                entry.frames.into_iter().find(|frame| frame.file == path)
                            {
                                let run_entry = new_entries.entry(frame).or_insert(RunEntry {
                                    out: Vec::new(),
                                    insert_type: None,
                                });
                                run_entry.out.push(text.clone())
                            }
                        }
                    }
                }
                RawEntryInner::Edits {
                    changes: new_changes,
                } => changes.extend(new_changes),
            }
        }

        RunData {
            entries: new_entries,
            hovers: new_hovers,
            changes,
        }
    }
}
