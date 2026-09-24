use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use frances_config::EnvString;
use rmcp::{
    ClientHandler,
    model::*,
    service::{
        ClientLifecycleMode, ClientServiceExt, NotificationContext, RoleClient, RunningService,
    },
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio_util::sync::CancellationToken;

use crate::{Error, Lifecycle, ServerConfig, Transport};

pub struct Environment {
    pub worker: Option<frances_worker::Client>,
    pub cwd: PathBuf,
    pub env: Arc<HashMap<OsString, OsString>>,
}

pub struct Handler {
    changed: Arc<AtomicU64>,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("frances", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changed.fetch_add(1, Ordering::AcqRel);
    }
}

/// Owns a single provider connection; full tool metadata and peer capabilities
/// remain available to the host rather than being flattened into model text.
pub struct Connection {
    pub name: String,
    service: RunningService<RoleClient, Handler>,
    notifications: tokio::task::JoinSet<()>,
    changed: Arc<AtomicU64>,
    timeout: Duration,
    local_process: Option<tokio::task::JoinHandle<()>>,
}

fn expand(value: &EnvString, env: &Environment) -> Result<String, Error> {
    value.expand(env.env.as_ref()).map_err(|error| match error {
        frances_config::EnvStringExpandError::MissingVar { var, .. } => Error::Environment(var),
    })
}

impl Connection {
    pub async fn connect(
        name: String,
        config: &ServerConfig,
        env: &Environment,
        cancel: &CancellationToken,
    ) -> Result<Self, Error> {
        let changed = Arc::new(AtomicU64::new(0));
        let handler = Handler {
            changed: changed.clone(),
        };
        let lifecycle = match config.lifecycle {
            Lifecycle::Auto => ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::V_2025_11_25),
            },
            Lifecycle::Modern => ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
            Lifecycle::Legacy => ClientLifecycleMode::Initialize,
        };
        let connect = async {
            let mut local_process = None;
            let service = match &config.transport {
                Transport::Stdio {
                    command,
                    args,
                    env: overrides,
                    cwd,
                } => {
                    let worker = env.worker.as_ref().ok_or(Error::WorkerUnavailable)?;
                    let stream = worker
                        .open_process(frances_worker_protocol::ProcessOptions {
                            shutdown_timeout_seconds: config.shutdown_timeout_seconds,
                            command: command.clone(),
                            args: args.clone(),
                            env: overrides
                                .iter()
                                .map(|(key, value)| (key.clone(), value.raw().to_owned()))
                                .collect(),
                            cwd: cwd
                                .as_ref()
                                .map(|p| env.cwd.join(p))
                                .unwrap_or_else(|| env.cwd.clone()),
                        })
                        .await?;
                    handler.serve_with_lifecycle(stream, lifecycle).await?
                }
                Transport::LocalStdio {
                    command,
                    args,
                    env: overrides,
                    cwd,
                } => {
                    let cwd = cwd
                        .as_ref()
                        .map(|p| env.cwd.join(p))
                        .unwrap_or_else(|| env.cwd.clone());
                    let mut process_env = (*env.env).clone();
                    for (key, value) in overrides {
                        process_env
                            .insert(OsString::from(key), OsString::from(expand(value, env)?));
                    }
                    let executable = frances_core::which::which_in(
                        command,
                        process_env.get(std::ffi::OsStr::new("PATH")).cloned(),
                        &cwd,
                    )
                    .await
                    .map_err(|source| Error::Executable {
                        command: command.clone(),
                        source,
                    })?;
                    let mut process = tokio::process::Command::new(executable);
                    process
                        .args(args)
                        .env_clear()
                        .envs(&process_env)
                        .current_dir(cwd)
                        .kill_on_drop(true);
                    let mut child = process
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::inherit())
                        .spawn()?;
                    let mut stdin = child.stdin.take().expect("piped MCP stdin");
                    let mut stdout = child.stdout.take().expect("piped MCP stdout");
                    let (transport, bridge) = tokio::io::duplex(64 * 1024);
                    let grace = Duration::from_secs(config.shutdown_timeout_seconds);
                    local_process = Some(tokio::spawn(async move {
                        let (mut read, mut write) = tokio::io::split(bridge);
                        let result = tokio::select! {
                            result = tokio::io::copy(&mut read, &mut stdin) => result,
                            result = tokio::io::copy(&mut stdout, &mut write) => result,
                        };
                        if let Err(error) = result {
                            tracing::debug!(%error, "local MCP stream closed");
                        }
                        if let Err(error) = frances_core::process::shutdown(&mut child, grace).await
                        {
                            tracing::warn!(%error, "failed to stop local MCP process");
                        }
                    }));
                    handler.serve_with_lifecycle(transport, lifecycle).await?
                }
                Transport::Http { url, headers } => {
                    let mut values = reqwest::header::HeaderMap::new();
                    for (key, value) in headers {
                        let key = reqwest::header::HeaderName::from_bytes(key.as_bytes())?;
                        let mut value =
                            reqwest::header::HeaderValue::from_str(&expand(value, env)?)?;
                        value.set_sensitive(true);
                        values.insert(key, value);
                    }
                    let client = reqwest::Client::builder()
                        .default_headers(values)
                        .redirect(reqwest::redirect::Policy::none())
                        .build()?;
                    let transport = StreamableHttpClientTransport::with_client(
                        client,
                        StreamableHttpClientTransportConfig::with_uri(url.as_str()),
                    );
                    handler.serve_with_lifecycle(transport, lifecycle).await?
                }
            };
            let mut connection = Self {
                name,
                service,
                local_process,
                notifications: tokio::task::JoinSet::new(),
                changed,
                timeout: Duration::from_secs(config.request_timeout_seconds),
            };
            if connection.service.peer_info().is_some_and(|info| {
                info.protocol_version == ProtocolVersion::V_2026_07_28
                    && info
                        .capabilities
                        .tools
                        .as_ref()
                        .is_some_and(|tools| tools.list_changed == Some(true))
            }) {
                let filter = SubscriptionFilter::builder().tools_list_changed().build();
                let mut subscription = connection.service.listen(filter).await?;
                let changed = connection.changed.clone();
                connection.notifications.spawn(async move {
                    loop {
                        match subscription.next().await {
                            Ok(Some(_)) => {
                                changed.fetch_add(1, Ordering::AcqRel);
                            }
                            Ok(None) => {
                                changed.fetch_add(1, Ordering::AcqRel);
                                break;
                            }
                            Err(error) => {
                                tracing::warn!(%error, "MCP tool subscription failed");
                                changed.fetch_add(1, Ordering::AcqRel);
                                break;
                            }
                        }
                    }
                });
            }
            Ok::<_, Error>(connection)
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Error::Interrupted),
            result = tokio::time::timeout(Duration::from_secs(config.startup_timeout_seconds), connect) => result.map_err(|_| Error::Timeout)?,
        }
    }

    pub fn catalog_revision(&self) -> u64 {
        self.changed.load(Ordering::Acquire)
    }
    pub fn is_closed(&self) -> bool {
        self.service.is_closed()
    }

    pub async fn tools(&self, cancel: &CancellationToken) -> Result<Vec<Tool>, Error> {
        if self
            .service
            .peer_info()
            .is_none_or(|info| info.capabilities.tools.is_none())
        {
            return Ok(vec![]);
        }
        let mut tools = self.wait(self.service.list_all_tools(), cancel).await?;
        let mut names = std::collections::HashSet::new();
        for tool in &tools {
            let schema = serde_json::Value::Object((*tool.input_schema).clone());
            if schema.get("type").and_then(serde_json::Value::as_str) != Some("object") {
                return Err(Error::SchemaNotObject(tool.name.to_string()));
            }
            jsonschema::validator_for(&schema).map_err(|source| Error::Schema {
                tool: tool.name.to_string(),
                source: Box::new(source),
            })?;
            if !names.insert(tool.name.clone()) {
                return Err(Error::DuplicateTool(tool.name.to_string()));
            }
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tools)
    }

    pub fn instructions(&self) -> Option<String> {
        self.service
            .peer_info()
            .and_then(|info| info.instructions.clone())
    }

    pub async fn call(
        &self,
        name: String,
        arguments: serde_json::Map<String, serde_json::Value>,
        cancel: &CancellationToken,
    ) -> Result<CallToolResult, Error> {
        match self
            .complete(
                ClientRequest::CallToolRequest(Request::new(
                    CallToolRequestParams::new(name).with_arguments(arguments),
                )),
                cancel,
            )
            .await?
        {
            ServerResult::CallToolResult(result) => Ok(result),
            _ => Err(Error::UnexpectedResponse),
        }
    }

    pub async fn resources(&self, cancel: &CancellationToken) -> Result<Vec<Resource>, Error> {
        self.wait(self.service.list_all_resources(), cancel).await
    }

    pub async fn templates(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<ResourceTemplate>, Error> {
        self.wait(self.service.list_all_resource_templates(), cancel)
            .await
    }

    pub async fn read(
        &self,
        uri: String,
        cancel: &CancellationToken,
    ) -> Result<ReadResourceResult, Error> {
        match self
            .complete(
                ClientRequest::ReadResourceRequest(Request::new(ReadResourceRequestParams::new(
                    uri,
                ))),
                cancel,
            )
            .await?
        {
            ServerResult::ReadResourceResult(result) => Ok(result),
            _ => Err(Error::UnexpectedResponse),
        }
    }

    pub async fn prompts(&self, cancel: &CancellationToken) -> Result<Vec<Prompt>, Error> {
        self.wait(self.service.list_all_prompts(), cancel).await
    }

    pub async fn prompt(
        &self,
        name: String,
        arguments: BTreeMap<String, String>,
        cancel: &CancellationToken,
    ) -> Result<GetPromptResult, Error> {
        let params = GetPromptRequestParams::new(name).with_arguments(
            arguments
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect(),
        );
        match self
            .complete(
                ClientRequest::GetPromptRequest(Request::new(params)),
                cancel,
            )
            .await?
        {
            ServerResult::GetPromptResult(result) => Ok(result),
            _ => Err(Error::UnexpectedResponse),
        }
    }

    pub fn has_resources(&self) -> bool {
        self.service
            .peer_info()
            .is_some_and(|info| info.capabilities.resources.is_some())
    }

    pub fn has_prompts(&self) -> bool {
        self.service
            .peer_info()
            .is_some_and(|info| info.capabilities.prompts.is_some())
    }

    async fn complete(
        &self,
        mut request: ClientRequest,
        cancel: &CancellationToken,
    ) -> Result<ServerResult, Error> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        for _ in 0..16 {
            if cancel.is_cancelled() {
                return Err(Error::Interrupted);
            }
            let mut handle = self
                .service
                .send_cancellable_request(
                    request.clone(),
                    rmcp::service::PeerRequestOptions::default(),
                )
                .await?;
            let failure = tokio::select! {
                biased;
                () = cancel.cancelled() => Some(Error::Interrupted),
                () = tokio::time::sleep_until(deadline) => Some(Error::Timeout),
                result = &mut handle.rx => {
                    match result?? {
                        ServerResult::InputRequiredResult(input) => {
                            if input.input_requests.as_ref().is_some_and(|requests| !requests.is_empty()) { return Err(Error::UnsupportedInput); }
                            match &mut request {
                                ClientRequest::CallToolRequest(call) => call.params.request_state = input.request_state,
                                ClientRequest::ReadResourceRequest(read) => read.params.request_state = input.request_state,
                                ClientRequest::GetPromptRequest(prompt) => prompt.params.request_state = input.request_state,
                                _ => return Err(Error::UnexpectedResponse),
                            }
                            None
                        }
                        result => return Ok(result),
                    }
                }
            };
            if let Some(failure) = failure {
                if let Err(error) = handle
                    .cancel(Some("Frances stopped waiting for this request".into()))
                    .await
                {
                    tracing::debug!(%error, server = %self.name, "MCP request cancellation failed");
                }
                return Err(failure);
            }
        }
        Err(Error::ContinuationLimit)
    }

    async fn wait<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, rmcp::service::ServiceError>>,
        cancel: &CancellationToken,
    ) -> Result<T, Error> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Error::Interrupted),
            result = tokio::time::timeout(self.timeout, future) => result.map_err(|_| Error::Timeout)?.map_err(Error::from),
        }
    }

    pub async fn close(mut self) {
        if let Err(error) = self
            .service
            .close_with_timeout(Duration::from_secs(2))
            .await
        {
            tracing::warn!(%error, server = %self.name, "closing MCP connection failed");
        }
        if let Some(task) = self.local_process.take()
            && let Err(error) = task.await
        {
            tracing::warn!(%error, "local MCP cleanup task failed");
        }
    }
}
