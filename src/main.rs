use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::io::{AsRawFd, FromRawFd},
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    thread::JoinHandle,
};
use std::{error::Error, time::Duration};
use std::{fs::File, io::ErrorKind};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use ropey::Rope;
use serde::Deserialize;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug, Deserialize, Eq, PartialEq, Hash)]
struct Frame {
    file: String,
    line: u64,
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    frames: Vec<Frame>,
    out: String,
}

impl RawEntry {
    fn from_json(reader: impl Read) -> Result<Self, Box<dyn std::error::Error>> {
        let entry = serde_json::de::from_reader(reader);
        match entry {
            Ok(entry) => Ok(entry),
            Err(err) => {
                log::error!("loading err {:?}", err);
                Err(Box::new(err))
            }
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct Runner {
    command: String,
    args: Vec<String>,
}

impl Runner {
    unsafe fn set_nonblocking(fd: &impl AsRawFd) {
        let fd = fd.as_raw_fd();
        let mut flags = libc::fcntl(fd, libc::F_GETFL);
        flags |= libc::O_NONBLOCK;
        libc::fcntl(fd, libc::F_SETFL, flags);
    }

    fn run(&self, path: &str, input: ropey::Rope) -> io::Result<Vec<RawEntry>> {
        // TODO? stream results with channel?
        // TODO just use 2 threads to read stdin/stdout
        // TODO use unix named pipe, instead of stderr for data

        let mut master: libc::c_int = 0;
        let mut slave: libc::c_int = 0;
        let winsize = libc::winsize {
            ws_col: 80,
            ws_row: 24,
            ws_xpixel: 480,
            ws_ypixel: 192,
        };
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &winsize,
            )
        };

        if result == -1 {
            return Err(io::Error::last_os_error());
        }

        let mut command = Command::new(&self.command);

        command
            .env("NWN_FILE_PATH", path)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(unsafe { Stdio::from_raw_fd(slave) });

        let mut child = command.spawn()?;
        // close the stdout/stderr writing fds, so OEF can be returned.
        drop(command);

        let mut stdin = child.stdin.take().unwrap();

        std::thread::spawn(move || {
            for chunk in input.chunks() {
                match stdin.write_all(chunk.as_bytes()) {
                    Ok(()) => {}
                    Err(e) => {
                        log::error!("stdin write err: {}", e);
                        return;
                    }
                }
            }
        });

        let stderr = child.stderr.take().unwrap();
        let mut stdout = unsafe { os_pipe::PipeReader::from_raw_fd(master) };
        unsafe {
            Self::set_nonblocking(&stderr);
            Self::set_nonblocking(&stdout);
        }
        let mut stderr = BufReader::new(stderr);

        let mut buffer = [0; 1024];
        let mut string_buffer = String::new();
        let mut entries = Vec::new();
        let (mut stdout_done, mut stderr_done) = (false, false);
        let mut would_block = false;

        while !(stdout_done && stderr_done) {
            if !stderr_done {
                string_buffer.clear();
                match stderr.read_line(&mut string_buffer) {
                    Ok(0) => {
                        stderr_done = true;
                    }
                    Ok(_) => {
                        log::warn!("read line: {:?}", string_buffer);
                        if string_buffer.trim().is_empty() {
                            continue;
                        }
                        let entry = RawEntry::from_json(string_buffer.as_bytes());
                        match entry {
                            Ok(entry) => entries.push(entry),
                            Err(err) => {
                                log::error!("reading line json err: {:?}", err);
                            }
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => would_block = true,
                    Err(e) => return Err(e),
                }
            }

            if !stdout_done {
                log::info!("start read stdout");
                match stdout.read(&mut buffer) {
                    Err(e) if e.kind() == ErrorKind::WouldBlock => would_block = true,
                    Ok(0) | Err(_) => {
                        stdout_done = true;
                    }
                    Ok(_) => {}
                }
                log::info!("finish read stdout");
            }

            if would_block {
                std::thread::sleep(Duration::from_millis(100))
            }
        }

        log::info!("finished reading");

        child.wait()?;

        Ok(entries)
    }
}

#[derive(Debug, Default)]
struct RunData {
    entries: HashMap<Frame, Vec<Arc<String>>>,
}

#[derive(Debug)]
struct Backend(Arc<BackendData>);

#[derive(Debug, Deserialize)]
struct LanguageConfig {
    extension: String,
    runner: Runner,
    line_comment: String,
    #[serde(default)]
    on_change: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
struct Config {
    #[serde(deserialize_with = "Config::map_from_list")]
    languages: HashMap<String, Arc<LanguageConfig>>,
}

impl Config {
    fn map_from_list<'de, D>(d: D) -> Result<HashMap<String, Arc<LanguageConfig>>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let vec = <Vec<Arc<LanguageConfig>>>::deserialize(d)?;

        Ok(vec
            .into_iter()
            .map(|config| (config.extension.clone(), config))
            .collect())
    }

    pub fn from_default_config() -> Result<Self, Box<dyn Error + Send + Sync>> {
        let path = "./.nwn.json";
        let file = File::open(path).map_err(|e| e.to_string())?;
        let config = serde_json::from_reader(file).map_err(|e| e.to_string())?;
        Ok(config)
    }
}

#[derive(Debug)]
struct BackendData {
    client: Client,
    data: ArcSwap<RunData>,
    runner: Option<Runner>,
    config: ArcSwap<Config>,
    buffers: DashMap<Url, ropey::Rope>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self(Arc::new(BackendData {
            client,
            data: ArcSwap::default(),
            runner: None,
            config: ArcSwap::new(Arc::new(Config::default())),
            buffers: DashMap::new(),
        }))
    }

    pub async fn load_config(&self) {
        let config = match Config::from_default_config() {
            Err(err) => {
                log::error!("error loading config: {}", err);
                self.0
                    .client
                    .log_message(
                        MessageType::Warning,
                        format!("could not find a a config: {}", err),
                    )
                    .await;

                Config::default()
            }
            Ok(config) => config,
        };

        log::info!("found config: {:?}", config);

        self.0.config.store(Arc::new(config));
    }

    pub fn language_config_for_uri(&self, uri: &Url) -> Option<Arc<LanguageConfig>> {
        let path = Path::new(uri.path());
        let config = path
            .extension()
            .map(|ext| ext.to_string_lossy())
            .and_then(|ext| {
                let config = self.0.config.load();
                config.languages.get(ext.as_ref()).cloned()
            });

        log::info!("found {:?} for {:?}", config, uri);

        config
    }

    pub async fn handle_save(&self, uri: &Url, contents: Option<Rope>) {
        log::info!("handle save");
        let config = self.language_config_for_uri(uri);
        if let Some(config) = config {
            if let Err(e) = self.reload_data(config, uri).join() {
                log::error!("did open reload err {:?}", e);
                return;
            }

            // update comments
            if let Some(contents) = contents {
                self.update_comments(uri, &contents).await
            } else {
                log::warn!("no text in handle_save");
            }
        }
    }

    pub fn reload_data(&self, config: Arc<LanguageConfig>, uri: &Url) -> JoinHandle<()> {
        let data = self.0.clone();
        let path = uri.path().to_owned();
        let buffer = self
            .0
            .buffers
            .get(uri)
            .map(|buffer| ropey::Rope::clone(&*buffer))
            .unwrap_or_default();
        std::thread::spawn(move || {
            let raw_data = match config.runner.run(&path, buffer) {
                Ok(data) => data,
                Err(err) => {
                    log::warn!("could not reload data, {}", err);
                    return;
                }
            };

            let mut new_data = HashMap::new();
            for entry in raw_data {
                let text = Arc::new(entry.out);
                for frame in entry.frames {
                    let list = new_data.entry(frame).or_insert_with(Vec::default);
                    list.push(text.clone());
                }
            }

            log::info!("new: {:?}", new_data);

            data.data.store(Arc::new(RunData { entries: new_data }));
        })
    }

    async fn update_comments(&self, uri: &Url, contents: &Rope) {
        /// find lines with comment output, result is an exclusive range
        fn find_prev_output(buffer: &Rope, line: usize) -> Option<(usize, usize)> {
            let mut curr = line + 1;
            while curr < buffer.len_lines()
                && buffer
                    .line(curr)
                    .chars()
                    .skip_while(|c| c.is_whitespace())
                    .zip("//>".chars())
                    .all(|(x, y)| x == y)
            {
                curr += 1;
            }

            if line + 1 == curr {
                None
            } else {
                Some((line + 1, curr))
            }
        }

        let config = self.language_config_for_uri(uri);
        if let Some(config) = config {
            let data = self.0.data.load();
            let changes = data
                .entries
                .iter()
                .filter(|(frame, _)| frame.file == uri.path())
                .map(|(frame, msgs)| {
                    let range = find_prev_output(contents, frame.line as usize - 1)
                        .map(|(start_line, end_line)| {
                            Range::new(
                                Position::new(start_line as u64, 0),
                                Position::new(end_line as u64, 0),
                            )
                        })
                        .unwrap_or_else(|| {
                            Range::new(Position::new(frame.line, 0), Position::new(frame.line, 0))
                        });

                    let text = msgs
                        .iter()
                        .map(|msg| format!("{}> {:?}\n", config.line_comment, msg))
                        .collect();
                    TextEdit::new(range, text)
                })
                .collect();

            log::info!("edits: {:?}", changes);

            let mut workspace_changes = HashMap::new();
            workspace_changes.insert(uri.clone(), changes);
            let response = self
                .0
                .client
                .apply_edit(WorkspaceEdit::new(workspace_changes))
                .await;

            log::info!("edit response: {:?}", response);
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> RpcResult<InitializeResult> {
        log::info!("initialize");
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        change: Some(TextDocumentSyncKind::Full),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            server_info: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        log::info!("initialized");
        self.0
            .client
            .log_message(MessageType::Info, "server initialized!")
            .await;

        self.load_config().await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let buffer: Rope = params.text_document.text.as_str().into();
        self.0
            .buffers
            .insert(params.text_document.uri.clone(), buffer.clone());

        self.handle_save(&params.text_document.uri, Some(buffer))
            .await
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.0.buffers.remove(&params.text_document.uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let buffer: Rope = params.content_changes[0].text.as_str().into();
        self.0
            .buffers
            .insert(params.text_document.uri.clone(), buffer.clone());
        log::info!(
            "new buffer: {:?}",
            self.0
                .buffers
                .get(&params.text_document.uri)
                .map(|rope| rope.len_bytes())
        );

        let config = self.language_config_for_uri(&params.text_document.uri);
        if config.map(|config| config.on_change).unwrap_or_default() {
            self.handle_save(&params.text_document.uri, Some(buffer))
                .await
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let buffer = self
            .0
            .buffers
            .get(&params.text_document.uri)
            .as_deref()
            .cloned();
        self.handle_save(&params.text_document.uri, buffer).await
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        log::info!("params {:?}", &params);
        let path = params
            .text_document_position_params
            .text_document
            .uri
            .path();
        log::warn!("path: {:?}", path);

        let frame = Frame {
            file: path.to_owned(),
            line: params.text_document_position_params.position.line + 1,
        };

        Ok(self.0.data.load().entries.get(&frame).map(|data| {
            let mut msg = String::new();
            for s in data {
                msg.push_str(s)
            }
            log::info!("hover msg {:?}", msg);
            Hover {
                contents: HoverContents::Scalar(MarkedString::String(msg)),
                range: None,
            }
        }))
    }
}

#[tokio::main]
async fn main() {
    simple_logging::log_to_file("test.log", log::LevelFilter::Info).unwrap();
    log::info!("start");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, messages) = LspService::new(Backend::new);

    Server::new(stdin, stdout)
        .interleave(messages)
        .serve(service)
        .await;
}
