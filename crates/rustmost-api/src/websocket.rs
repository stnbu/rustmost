//! The real-time event stream (`/api/v4/websocket`).
//!
//! [`WebSocket`] runs a background task that keeps the connection alive,
//! reconnects with backoff, and asks the server to replay events missed
//! while disconnected. You read typed [`WsEvent`]s with [`WebSocket::next`].

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use crate::client::ensure_crypto_provider;
use crate::error::{Error, Result};
use crate::models::{ChannelType, Post, Reaction};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone)]
pub struct WebSocketOptions {
    /// How often to send a ping to keep the connection alive.
    pub ping_interval: Duration,
    /// Reconnect if nothing at all arrives for this long.
    pub stale_after: Duration,
    /// Upper bound on the delay between reconnect attempts.
    pub max_backoff: Duration,
    /// Events buffered before the reader applies backpressure.
    pub buffer: usize,
}

impl Default for WebSocketOptions {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(30),
            stale_after: Duration::from_secs(90),
            max_backoff: Duration::from_secs(60),
            buffer: 1024,
        }
    }
}

/// Fields every server event carries about who it was sent to.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Broadcast {
    pub user_id: String,
    pub channel_id: String,
    pub team_id: String,
    pub omit_users: Option<Map<String, Value>>,
}

/// An event exactly as the server sent it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawEvent {
    pub event: String,
    pub data: Map<String, Value>,
    pub broadcast: Broadcast,
    pub seq: i64,
}

impl RawEvent {
    fn str(&self, key: &str) -> String {
        self.data
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// Several fields (e.g. `post`) are JSON documents encoded as strings.
    fn embedded<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Option<T> {
        match self.data.get(key)? {
            Value::String(s) => serde_json::from_str(s).ok(),
            v => serde_json::from_value(v.clone()).ok(),
        }
    }
}

/// A new message.
#[derive(Debug, Clone)]
pub struct PostedEvent {
    pub post: Post,
    pub channel_display_name: String,
    pub channel_name: String,
    pub channel_type: Option<ChannelType>,
    /// Usually `@username`.
    pub sender_name: String,
    pub team_id: String,
    /// User ids mentioned by this post (only included for recipients who were mentioned).
    pub mentions: Vec<String>,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WsEvent {
    /// The connection is up. `resumed` is true when the server replayed
    /// everything missed since the last connection; when false after a
    /// reconnect, refetch whatever state you display.
    Connected {
        connection_id: String,
        server_version: String,
        resumed: bool,
    },
    /// The connection dropped; the client will retry after `retry_in`.
    Disconnected {
        reason: String,
        retry_in: Duration,
    },
    /// Events were lost (sequence gap). Refetch state you display.
    MissedEvents,
    /// The server rejected the session (e.g. token expired). The stream ends.
    AuthFailed(String),

    Posted(Box<PostedEvent>),
    PostEdited(Box<Post>),
    PostDeleted(Box<Post>),
    Typing {
        channel_id: String,
        user_id: String,
        parent_id: String,
    },
    /// Channels were marked read (possibly from another device).
    ChannelsViewed {
        channel_ids: Vec<String>,
    },
    StatusChange {
        user_id: String,
        status: String,
    },
    ReactionAdded(Reaction),
    ReactionRemoved(Reaction),
    /// Any event this library does not decode yet.
    Other(RawEvent),
}

impl WsEvent {
    fn from_raw(raw: RawEvent) -> WsEvent {
        match raw.event.as_str() {
            "posted" => match raw.embedded::<Post>("post") {
                Some(post) => WsEvent::Posted(Box::new(PostedEvent {
                    channel_display_name: raw.str("channel_display_name"),
                    channel_name: raw.str("channel_name"),
                    channel_type: raw
                        .data
                        .get("channel_type")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                    sender_name: raw.str("sender_name"),
                    team_id: raw.str("team_id"),
                    mentions: raw.embedded("mentions").unwrap_or_default(),
                    post,
                })),
                None => WsEvent::Other(raw),
            },
            "post_edited" => match raw.embedded::<Post>("post") {
                Some(p) => WsEvent::PostEdited(Box::new(p)),
                None => WsEvent::Other(raw),
            },
            "post_deleted" => match raw.embedded::<Post>("post") {
                Some(p) => WsEvent::PostDeleted(Box::new(p)),
                None => WsEvent::Other(raw),
            },
            "typing" => WsEvent::Typing {
                channel_id: raw.broadcast.channel_id.clone(),
                user_id: raw.str("user_id"),
                parent_id: raw.str("parent_id"),
            },
            "channel_viewed" => WsEvent::ChannelsViewed {
                channel_ids: vec![raw.str("channel_id")],
            },
            "multiple_channels_viewed" => {
                let ids = raw
                    .data
                    .get("channel_times")
                    .and_then(Value::as_object)
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                WsEvent::ChannelsViewed { channel_ids: ids }
            }
            "status_change" => WsEvent::StatusChange {
                user_id: raw.str("user_id"),
                status: raw.str("status"),
            },
            "reaction_added" => match raw.embedded::<Reaction>("reaction") {
                Some(r) => WsEvent::ReactionAdded(r),
                None => WsEvent::Other(raw),
            },
            "reaction_removed" => match raw.embedded::<Reaction>("reaction") {
                Some(r) => WsEvent::ReactionRemoved(r),
                None => WsEvent::Other(raw),
            },
            _ => WsEvent::Other(raw),
        }
    }
}

enum Command {
    Typing {
        channel_id: String,
        parent_id: String,
    },
    Raw {
        action: String,
        data: Value,
    },
}

/// A cheap, cloneable handle for sending actions over the socket.
#[derive(Clone)]
pub struct WebSocketHandle {
    tx: mpsc::UnboundedSender<Command>,
}

impl WebSocketHandle {
    /// Tell others you are typing in a channel (or a thread, via `parent_id`).
    pub fn send_typing(&self, channel_id: &str, parent_id: Option<&str>) {
        let _ = self.tx.send(Command::Typing {
            channel_id: channel_id.to_string(),
            parent_id: parent_id.unwrap_or_default().to_string(),
        });
    }

    /// Send any websocket action, e.g. `get_statuses`.
    pub fn send_action(&self, action: &str, data: Value) {
        let _ = self.tx.send(Command::Raw {
            action: action.to_string(),
            data,
        });
    }
}

pub struct WebSocket {
    events: mpsc::Receiver<WsEvent>,
    handle: WebSocketHandle,
    task: JoinHandle<()>,
}

impl Drop for WebSocket {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl WebSocket {
    /// Connect to `url` (a `ws://` or `wss://` URL ending in `/api/v4/websocket`).
    /// The first connection happens before this returns, so a bad token or
    /// unreachable server is reported here.
    pub async fn connect(url: Url, token: String, options: WebSocketOptions) -> Result<WebSocket> {
        ensure_crypto_provider();
        let first = dial(&url, &token, None).await?;
        let (ev_tx, ev_rx) = mpsc::channel(options.buffer);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(url, token, options, first, ev_tx, cmd_rx));
        Ok(WebSocket {
            events: ev_rx,
            handle: WebSocketHandle { tx: cmd_tx },
            task,
        })
    }

    /// The next event, or `None` once the stream has ended for good.
    pub async fn next(&mut self) -> Option<WsEvent> {
        self.events.recv().await
    }

    pub fn handle(&self) -> WebSocketHandle {
        self.handle.clone()
    }
}

struct Resume {
    connection_id: String,
    next_seq: i64,
}

async fn dial(url: &Url, token: &str, resume: Option<&Resume>) -> Result<Ws> {
    let mut url = url.clone();
    if let Some(r) = resume {
        url.query_pairs_mut()
            .append_pair("connection_id", &r.connection_id)
            .append_pair("sequence_number", &r.next_seq.to_string());
    }
    let mut req = url.as_str().into_client_request()?;
    let auth = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|e| Error::Other(e.to_string()))?;
    req.headers_mut().insert("Authorization", auth);
    let (ws, _resp) = tokio_tungstenite::connect_async(req).await?;
    Ok(ws)
}

fn is_auth_error(e: &Error) -> bool {
    matches!(e, Error::WebSocket(tungstenite::Error::Http(resp)) if resp.status() == 401 || resp.status() == 403)
}

enum Exit {
    /// The application dropped the event receiver.
    Shutdown,
    Dropped(String),
}

async fn run(
    url: Url,
    token: String,
    options: WebSocketOptions,
    first: Ws,
    events: mpsc::Sender<WsEvent>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let mut conn = Some(first);
    let mut resume: Option<Resume> = None;
    let mut attempt: u32 = 0;

    loop {
        let ws = match conn.take() {
            Some(ws) => ws,
            None => match dial(&url, &token, resume.as_ref()).await {
                Ok(ws) => ws,
                Err(e) if is_auth_error(&e) => {
                    let _ = events.send(WsEvent::AuthFailed(e.to_string())).await;
                    return;
                }
                Err(e) => {
                    let delay = backoff(attempt, options.max_backoff);
                    attempt = attempt.saturating_add(1);
                    if events
                        .send(WsEvent::Disconnected {
                            reason: e.to_string(),
                            retry_in: delay,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(delay).await;
                    continue;
                }
            },
        };

        match session(
            ws,
            &options,
            &mut resume,
            &mut attempt,
            &events,
            &mut commands,
        )
        .await
        {
            Exit::Shutdown => return,
            Exit::Dropped(reason) => {
                let delay = backoff(attempt, options.max_backoff);
                attempt = attempt.saturating_add(1);
                if events
                    .send(WsEvent::Disconnected {
                        reason,
                        retry_in: delay,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(delay).await;
            }
        }
    }
}

fn backoff(attempt: u32, max: Duration) -> Duration {
    let secs = 1u64 << attempt.min(6); // 1, 2, 4 ... 64
    Duration::from_secs(secs).min(max)
}

async fn session(
    mut ws: Ws,
    options: &WebSocketOptions,
    resume: &mut Option<Resume>,
    attempt: &mut u32,
    events: &mpsc::Sender<WsEvent>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> Exit {
    let mut ping = tokio::time::interval(options.ping_interval);
    ping.tick().await;
    let mut client_seq: i64 = 1;
    let previous_connection = resume.as_ref().map(|r| r.connection_id.clone());
    let mut got_hello = false;
    let mut commands_open = true;

    loop {
        tokio::select! {
            msg = tokio::time::timeout(options.stale_after, ws.next()) => {
                let msg = match msg {
                    Err(_) => return Exit::Dropped("no data from server; connection looks stale".into()),
                    Ok(None) => return Exit::Dropped("connection closed".into()),
                    Ok(Some(Err(e))) => return Exit::Dropped(e.to_string()),
                    Ok(Some(Ok(m))) => m,
                };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(frame) => {
                        let why = frame.map(|f| f.reason.to_string()).filter(|r| !r.is_empty());
                        return Exit::Dropped(why.unwrap_or_else(|| "server closed the connection".into()));
                    }
                    _ => continue, // ping/pong/binary
                };
                let value: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => { tracing::warn!("bad websocket message: {e}"); continue; }
                };
                // Replies to our own actions: {"status":"OK","seq_reply":N}
                if value.get("event").is_none() {
                    continue;
                }
                let raw: RawEvent = match serde_json::from_value(value) {
                    Ok(r) => r,
                    Err(e) => { tracing::warn!("bad websocket event: {e}"); continue; }
                };

                let mut out = Vec::new();
                if raw.event == "hello" {
                    let connection_id = raw.str("connection_id");
                    let resumed = previous_connection.as_deref() == Some(connection_id.as_str()) && !connection_id.is_empty();
                    if !resumed {
                        *resume = Some(Resume { connection_id: connection_id.clone(), next_seq: raw.seq + 1 });
                    }
                    got_hello = true;
                    *attempt = 0;
                    out.push(WsEvent::Connected { connection_id, server_version: raw.str("server_version"), resumed });
                } else {
                    if let Some(r) = resume.as_mut() {
                        if raw.seq < r.next_seq {
                            continue; // duplicate from a replay
                        }
                        if raw.seq > r.next_seq && got_hello {
                            tracing::warn!("websocket sequence gap: expected {}, got {}", r.next_seq, raw.seq);
                            out.push(WsEvent::MissedEvents);
                        }
                        r.next_seq = raw.seq + 1;
                    }
                    out.push(WsEvent::from_raw(raw));
                }
                for ev in out {
                    if events.send(ev).await.is_err() {
                        let _ = ws.close(None).await;
                        return Exit::Shutdown;
                    }
                }
            }
            _ = ping.tick() => {
                if let Err(e) = ws.send(Message::Ping(Vec::new().into())).await {
                    return Exit::Dropped(e.to_string());
                }
            }
            cmd = commands.recv(), if commands_open => {
                let (action, data) = match cmd {
                    // Every handle is gone; keep receiving events anyway.
                    None => { commands_open = false; continue; }
                    Some(Command::Typing { channel_id, parent_id }) =>
                        ("user_typing".to_string(), json!({ "channel_id": channel_id, "parent_id": parent_id })),
                    Some(Command::Raw { action, data }) => (action, data),
                };
                let msg = json!({ "action": action, "seq": client_seq, "data": data });
                client_seq += 1;
                if let Err(e) = ws.send(Message::Text(msg.to_string().into())).await {
                    return Exit::Dropped(e.to_string());
                }
            }
        }
    }
}
