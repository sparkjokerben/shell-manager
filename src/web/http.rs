//! HTTP API, WebSocket terminal and embedded asset routes for `smw`.
//!
//! The control plane is a plain JSON API; each WebSocket owns one
//! [`Attachment`](super::bridge::Attachment), which forwards raw bytes in both
//! directions. xterm.js consumes the bytes verbatim, so this layer does not
//! inspect or rewrite terminal input.

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, Path, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use shell_manager::log;
use shell_manager::protocol::{self, SessionInfo};

use super::auth;
use super::bridge::{self, AttachError, OutputEvent};
use super::static_assets;
use super::AppState;

/// Default terminal size used for `POST /api/sessions` and for WebSocket
/// clients that do not report their own size.
pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 40;

/// Highest accepted terminal dimension.
pub const MAX_DIM: u16 = 1000;

/// Upper bound on one WebSocket message. A terminal paste is tiny compared to
/// this; the limit keeps a hostile or buggy client from allocating gigabytes
/// in the input queue.
const MAX_WS_MESSAGE: usize = 1024 * 1024;

/// Bound on one WebSocket send. A peer that stopped reading must not pin the
/// attachment (and its registry slot) forever.
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Outbound message queue. Sends happen in a dedicated task so the terminal
/// loop never blocks on a dead TCP connection.
const WS_QUEUE_CAPACITY: usize = 64;

/// Body of `POST /api/sessions`.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateSession {
    pub name: String,
    #[serde(default)]
    pub wsl: Option<bool>,
}

/// Messages a WebSocket client may send as text.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMessage {
    Resize { cols: u32, rows: u32 },
    Detach,
}

/// Build the complete router. Auth middleware is attached to the API and
/// WebSocket subtrees only, keeping assets and `/api/health` public.
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/:name/detach", post(detach_session))
        .route("/sessions/:name", delete(stop_session))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_http,
        ));

    let ws = Router::new().route("/:name", get(ws_handler)).route_layer(
        axum::middleware::from_fn_with_state(state.clone(), auth::require_ws),
    );

    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .nest("/api", api)
        .nest("/ws", ws)
        .fallback(static_fallback)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

async fn index() -> Response {
    asset_response("/")
}

async fn health(State(state): State<AppState>) -> Response {
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "ok": true,
            "instance": state.instance,
            "version": env!("CARGO_PKG_VERSION"),
            "has_token": state.token.is_some(),
        }),
    )
}

async fn list_sessions(State(state): State<AppState>) -> Response {
    match state.control.list().await {
        Ok(infos) => json_response(StatusCode::OK, sessions_json(infos)),
        Err(error) => control_error(&state, "list", &error.to_string()),
    }
}

async fn create_session(
    State(state): State<AppState>,
    payload: Result<Json<CreateSession>, JsonRejection>,
) -> Response {
    let Json(body) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid request body: {}", rejection.body_text()),
            )
        }
    };
    if let Err(message) = validate_name(&body.name) {
        return error_response(StatusCode::BAD_REQUEST, &message);
    }

    let response = match state
        .control
        .start(
            &body.name,
            body.wsl.unwrap_or(false),
            DEFAULT_COLS,
            DEFAULT_ROWS,
        )
        .await
    {
        Ok(response) => response,
        Err(error) => return control_error(&state, "start", &error.to_string()),
    };

    if response.ok {
        log(
            &state,
            &format!("start name={} wsl={}", body.name, body.wsl.unwrap_or(false)),
        );
        json_response(StatusCode::CREATED, serde_json::json!({ "ok": true }))
    } else {
        log(
            &state,
            &format!("start failed name={} error={}", body.name, response.error),
        );
        error_response(daemon_status(&response.error), &response.error)
    }
}

async fn detach_session(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if let Err(message) = validate_name(&name) {
        return error_response(StatusCode::BAD_REQUEST, &message);
    }
    match state.control.detach(&name).await {
        Ok(()) => {
            log(&state, &format!("detach name={}", name));
            json_response(StatusCode::OK, serde_json::json!({ "ok": true }))
        }
        Err(error) => control_error(&state, "detach", &error.to_string()),
    }
}

async fn stop_session(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if let Err(message) = validate_name(&name) {
        return error_response(StatusCode::BAD_REQUEST, &message);
    }
    match state.control.stop(&name).await {
        Ok(()) => {
            log(&state, &format!("stop name={}", name));
            json_response(StatusCode::OK, serde_json::json!({ "ok": true }))
        }
        Err(error) => control_error(&state, "stop", &error.to_string()),
    }
}

/// Static files: everything not matched by an API or WebSocket route.
async fn static_fallback(OriginalUri(uri): OriginalUri) -> Response {
    asset_response(uri.path())
}

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

async fn ws_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(message) = validate_name(&name) {
        return error_response(StatusCode::BAD_REQUEST, &message);
    }

    let query = query.as_deref();
    let cols = parse_dim(auth::query_param(query, "cols"), DEFAULT_COLS);
    let rows = parse_dim(auth::query_param(query, "rows"), DEFAULT_ROWS);
    let takeover = matches!(
        auth::query_param(query, "takeover"),
        Some("1") | Some("true")
    );

    log(
        &state,
        &format!(
            "ws upgrade name={} cols={} rows={} takeover={}",
            name, cols, rows, takeover
        ),
    );

    upgrade
        .max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| handle_socket(socket, state, name, cols, rows, takeover))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    name: String,
    cols: u16,
    rows: u16,
    takeover: bool,
) {
    let mut attachment =
        match bridge::attach(&state.control, &state.registry, &name, cols, rows, takeover).await {
            Ok(attachment) => attachment,
            Err(error) => {
                let message = attach_error_message(&error);
                let status = attach_status(&error);
                log(
                    &state,
                    &format!(
                        "attach failed name={} status={} error={}",
                        name,
                        status.as_u16(),
                        error
                    ),
                );
                let value = serde_json::json!({ "type": "error", "message": message });
                let _ = tokio::time::timeout(
                    WS_SEND_TIMEOUT,
                    socket.send(Message::Text(value.to_string())),
                )
                .await;
                let _ =
                    tokio::time::timeout(WS_SEND_TIMEOUT, socket.send(Message::Close(None))).await;
                return;
            }
        };

    log(
        &state,
        &format!("attach name={} mouse={}", name, attachment.mouse),
    );

    let takeover = attachment.takeover_flag();
    let mut status = attachment.status_receiver();
    let input = attachment.input_sender();

    let (mut sink, mut stream) = socket.split();
    let (send_tx, mut send_rx) = mpsc::channel::<Message>(WS_QUEUE_CAPACITY);
    let mut writer = tokio::spawn(async move {
        while let Some(message) = send_rx.recv().await {
            if tokio::time::timeout(WS_SEND_TIMEOUT, sink.send(message))
                .await
                .is_err()
            {
                break;
            }
        }
        // A close frame makes an intentional detach a normal closure (1005)
        // instead of an abnormal one.
        let _ = tokio::time::timeout(WS_SEND_TIMEOUT, sink.close()).await;
    });

    let ready = serde_json::json!({
        "type": "ready",
        "name": name,
        "mouse": attachment.mouse,
        "instance": state.instance,
    });
    if send_tx
        .send(Message::Text(ready.to_string()))
        .await
        .is_err()
    {
        drop(send_tx);
        let _ = writer.await;
        attachment.shutdown().await;
        return;
    }

    // Reattaching is repainted by the session itself (`needs_repaint` in
    // session.rs). A second shrink/restore here would interleave with that one
    // and with the browser's first resize request, making ConPTY reflow the
    // screen while the user is already typing.

    let mut takeover_tick = tokio::time::interval(Duration::from_millis(200));
    takeover_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = takeover_tick.tick() => {
                if takeover.load(Ordering::Acquire) {
                    queue_termination(&send_tx, true, String::new());
                    break;
                }
            }
            event = attachment.output_receiver().recv() => {
                match event {
                    Some(OutputEvent::Data(bytes)) => {
                        if send_tx.try_send(Message::Binary(bytes)).is_err() {
                            break;
                        }
                    }
                    Some(OutputEvent::Exit(message)) => {
                        queue_termination(&send_tx, takeover.load(Ordering::Acquire), message);
                        break;
                    }
                    None => {
                        queue_termination(
                            &send_tx,
                            takeover.load(Ordering::Acquire),
                            "session ended".to_string(),
                        );
                        break;
                    }
                }
            }
            changed = status.changed() => {
                if changed.is_err() {
                    break;
                }
                let message = status.borrow().clone();
                if let Some(message) = message {
                    queue_termination(&send_tx, takeover.load(Ordering::Acquire), message);
                    break;
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Binary(bytes))) => {
                        match input.try_send(bytes) {
                            Ok(()) => {}
                            // Slow client: drop the connection instead of
                            // blocking the pipe and stalling the session.
                            Err(TrySendError::Full(_)) => {
                                let _ = send_tx.try_send(Message::Text(
                                    serde_json::json!({
                                        "type": "error",
                                        "message": "input backlog exceeded; detached",
                                    })
                                    .to_string(),
                                ));
                                break;
                            }
                            Err(TrySendError::Closed(_)) => break,
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        match parse_client_message(&text) {
                            Some(ClientMessage::Resize { cols, rows }) => {
                                let cols = clamp_dim(cols);
                                let rows = clamp_dim(rows);
                                // Control requests may take seconds when the
                                // daemon is busy; keep draining the terminal.
                                let resize_state = state.clone();
                                let resize_name = name.clone();
                                tokio::spawn(async move {
                                    if let Err(error) = resize_state
                                        .control
                                        .resize(&resize_name, cols, rows)
                                        .await
                                    {
                                        log(
                                            &resize_state,
                                            &format!(
                                                "resize failed name={} error={}",
                                                resize_name, error
                                            ),
                                        );
                                    }
                                });
                            }
                            Some(ClientMessage::Detach) => break,
                            None => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    // Ping/pong are handled by the WebSocket layer.
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    // Stop queueing output and let the writer flush the terminal message and
    // close frame. Tear the attachment down in parallel so the registry slot is
    // released even when the peer is unreachable.
    drop(send_tx);
    drop(input);
    let flush = async {
        if tokio::time::timeout(WS_SEND_TIMEOUT, &mut writer)
            .await
            .is_err()
        {
            writer.abort();
        }
    };
    tokio::join!(flush, attachment.shutdown());
    log(&state, &format!("detach name={}", name));
}

/// Queue a final message and let the writer close the socket.
fn queue_termination(tx: &mpsc::Sender<Message>, taken_over: bool, message: String) {
    let value = if taken_over {
        serde_json::json!({
            "type": "error",
            "message": "该会话已被另一个网页终端接管",
        })
    } else {
        serde_json::json!({ "type": "exit", "message": message })
    };
    let _ = tx.try_send(Message::Text(value.to_string()));
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested without a live server)
// ---------------------------------------------------------------------------

/// Map a daemon error message to an HTTP status.
pub fn daemon_status(message: &str) -> StatusCode {
    if message.contains("not found") || message.contains("has exited") {
        StatusCode::NOT_FOUND
    } else if message.contains("already exists") {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_GATEWAY
    }
}

/// True when the data pipes could not be opened because another client owns
/// them (the message comes from [`super::bridge`]'s pipe opener).
pub fn is_pipe_timeout(message: &str) -> bool {
    message.contains("timed out connecting to data pipe")
}

/// Map an attach failure to an HTTP-style status (used for logging and tests).
pub fn attach_status(error: &AttachError) -> StatusCode {
    match error {
        AttachError::Busy => StatusCode::CONFLICT,
        AttachError::Control(message) => daemon_status(message),
        AttachError::Pipe(message) if is_pipe_timeout(message) => StatusCode::CONFLICT,
        AttachError::Pipe(_) => StatusCode::BAD_GATEWAY,
    }
}

/// User-facing text for an attach failure.
pub fn attach_error_message(error: &AttachError) -> String {
    match error {
        AttachError::Busy => "该会话已被本进程的另一个网页终端占用".to_string(),
        AttachError::Control(message) => message.clone(),
        AttachError::Pipe(message) if is_pipe_timeout(message) => {
            "连接会话数据管道超时（可能被其他客户端占用，可尝试强制接管）".to_string()
        }
        AttachError::Pipe(_) => "无法打开会话的数据管道".to_string(),
    }
}

/// Reject empty/whitespace-only names, names longer than 64 characters, names
/// containing control characters and names that cannot be addressed as a single
/// URL path segment.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("session name must not be empty".to_string());
    }
    if name.chars().count() > 64 {
        return Err("session name is too long (maximum 64 characters)".to_string());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("session name must not contain control characters".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("session name must not contain slashes".to_string());
    }
    if name == "." || name == ".." {
        return Err("session name must not be . or ..".to_string());
    }
    Ok(())
}

/// Parse a client text message; malformed or unknown messages are ignored.
pub fn parse_client_message(text: &str) -> Option<ClientMessage> {
    serde_json::from_str(text).ok()
}

/// Terminal dimension from the query string, clamped to `1..=MAX_DIM`.
pub fn parse_dim(value: Option<&str>, default: u16) -> u16 {
    match value.and_then(|value| value.parse::<u32>().ok()) {
        Some(value) if value > 0 => clamp_dim(value),
        _ => default,
    }
}

/// Clamp a reported dimension into the accepted range.
pub fn clamp_dim(value: u32) -> u16 {
    value.clamp(1, MAX_DIM as u32) as u16
}

/// Session listing for the browser: never exposes pipe names or session IDs.
pub fn sessions_json(infos: Vec<SessionInfo>) -> serde_json::Value {
    let sessions: Vec<serde_json::Value> = infos
        .into_iter()
        .map(|info| {
            serde_json::json!({
                "name": info.name,
                "attached": info.attached,
            })
        })
        .collect();
    serde_json::json!({ "sessions": sessions })
}

/// Embedded asset response with MIME type and cache policy.
fn asset_response(path: &str) -> Response {
    match static_assets::lookup(path) {
        Some((mime, bytes)) => {
            let cache = if mime.starts_with("text/html") {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            };
            auth::with_security_headers(Response::builder())
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CACHE_CONTROL, cache)
                .body(Body::from(bytes))
                .expect("static response is well formed")
        }
        None => error_response(StatusCode::NOT_FOUND, "not found"),
    }
}

fn control_error(state: &AppState, action: &str, message: &str) -> Response {
    let status = daemon_status(message);
    log(
        state,
        &format!(
            "{} failed status={} error={}",
            action,
            status.as_u16(),
            message
        ),
    );
    error_response(status, message)
}

fn error_response(status: StatusCode, message: &str) -> Response {
    json_response(status, serde_json::json!({ "error": message }))
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    let body = serde_json::to_vec(&value).expect("JSON values serialize");
    auth::with_security_headers(Response::builder())
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body))
        .expect("JSON response is well formed")
}

fn log(state: &AppState, message: &str) {
    log::write_to(
        &protocol::smw_log_file(),
        &format!("instance={} {}", state.instance, message),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, session_id: &str, attached: bool) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            session_id: session_id.to_string(),
            attached,
        }
    }

    #[test]
    fn daemon_errors_map_to_expected_statuses() {
        assert_eq!(
            daemon_status("session 'a' not found"),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            daemon_status("session 'a' has exited"),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            daemon_status("session 'a' already exists"),
            StatusCode::CONFLICT
        );
        assert_eq!(
            daemon_status("failed to connect to the daemon"),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn attach_errors_map_to_expected_statuses() {
        assert_eq!(attach_status(&AttachError::Busy), StatusCode::CONFLICT);
        assert_eq!(
            attach_status(&AttachError::Control("session 'a' not found".into())),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            attach_status(&AttachError::Pipe(
                "timed out connecting to data pipe \\\\.\\pipe\\x".into()
            )),
            StatusCode::CONFLICT
        );
        assert_eq!(
            attach_status(&AttachError::Pipe("access denied".into())),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            attach_status(&AttachError::Control("daemon crashed".into())),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn attach_messages_explain_busy_and_pipe_timeouts() {
        assert!(attach_error_message(&AttachError::Busy).contains("另一个网页终端"));
        let pipe = attach_error_message(&AttachError::Pipe(
            "timed out connecting to data pipe \\\\.\\pipe\\shell-manager\\data\\uuid\\out".into(),
        ));
        assert!(pipe.contains("超时"));
        assert!(pipe.contains("强制接管"));
        // Internal pipe names and session ids must not reach the browser.
        assert!(!pipe.contains("pipe"));
        assert!(!pipe.contains("uuid"));
        assert_eq!(
            attach_error_message(&AttachError::Control("boom".into())),
            "boom"
        );
        assert_eq!(
            attach_error_message(&AttachError::Pipe("access denied".into())),
            "无法打开会话的数据管道"
        );
    }

    #[test]
    fn session_names_are_validated() {
        assert!(validate_name("build").is_ok());
        assert!(validate_name("work-1").is_ok());

        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("\t\n").is_err());
        assert!(validate_name(&"a".repeat(65)).is_err());
        assert!(validate_name(&"a".repeat(64)).is_ok());
        assert!(validate_name("bad\0name").is_err());
        assert!(validate_name("bad\nname").is_err());
        assert!(validate_name("bad\u{7}name").is_err());
        assert!(validate_name("bad/name").is_err());
        assert!(validate_name(r"bad\name").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
    }

    #[test]
    fn client_messages_are_parsed() {
        assert_eq!(
            parse_client_message(r#"{"type":"resize","cols":100,"rows":40}"#),
            Some(ClientMessage::Resize {
                cols: 100,
                rows: 40
            })
        );
        assert_eq!(
            parse_client_message(r#"{"type":"detach"}"#),
            Some(ClientMessage::Detach)
        );
        assert_eq!(parse_client_message(r#"{"type":"unknown"}"#), None);
        assert_eq!(parse_client_message("not json"), None);
        // A resize without dimensions is rejected rather than guessed.
        assert_eq!(parse_client_message(r#"{"type":"resize"}"#), None);
    }

    #[test]
    fn dimensions_fall_back_and_clamp() {
        assert_eq!(parse_dim(None, DEFAULT_COLS), DEFAULT_COLS);
        assert_eq!(parse_dim(Some("0"), DEFAULT_COLS), DEFAULT_COLS);
        assert_eq!(parse_dim(Some("nope"), DEFAULT_COLS), DEFAULT_COLS);
        assert_eq!(parse_dim(Some("80"), DEFAULT_COLS), 80);
        assert_eq!(parse_dim(Some("5000"), DEFAULT_COLS), MAX_DIM);
        assert_eq!(clamp_dim(0), 1);
        assert_eq!(clamp_dim(1), 1);
        assert_eq!(clamp_dim(1000), 1000);
        assert_eq!(clamp_dim(u32::MAX), 1000);
    }

    #[test]
    fn session_listing_omits_internal_ids() {
        let value = sessions_json(vec![
            info("alpha", r"\\.\pipe\alpha-123", true),
            info("beta", r"\\.\pipe\beta-456", false),
        ]);
        let text = value.to_string();
        assert!(!text.contains("session_id"));
        assert!(!text.contains("pipe"));
        assert!(!text.contains("123"));
        assert_eq!(
            value,
            serde_json::json!({
                "sessions": [
                    { "name": "alpha", "attached": true },
                    { "name": "beta", "attached": false },
                ]
            })
        );
    }
}
