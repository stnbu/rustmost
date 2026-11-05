//! A tiny in-memory imitation of a Mattermost server, for tests and for
//! trying rustmost without a real server.
//!
//! It speaks a subset of the v4 REST API and the WebSocket event stream,
//! with JSON shaped like the real server's. It is deliberately independent
//! of `rustmost-api`, so tests check the client against server-shaped JSON
//! rather than against its own types.
//!
//! Seed data: team `dev`, channels `town-square` and `off-topic`, users
//! `alice` / `bob` (password `secret`) and `carol` (password `secret`, MFA
//! code `123456`).

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::broadcast;

pub const VERSION: &str = "11.7.3";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// 26-char lowercase ids like the real server's.
fn new_id(n: u64) -> String {
    let mut s = format!("{n:x}");
    while s.len() < 26 {
        s.insert(0, '0');
    }
    s
}

#[derive(Clone)]
struct User {
    id: String,
    username: String,
    first: String,
    last: String,
    password: String,
    mfa: Option<String>,
}

#[derive(Clone)]
struct Channel {
    id: String,
    team_id: String,
    kind: &'static str,
    name: String,
    display_name: String,
    total: i64,
    total_root: i64,
    last_post_at: i64,
    members: Vec<String>,
}

#[derive(Clone)]
struct Post {
    id: String,
    channel_id: String,
    user_id: String,
    root_id: String,
    message: String,
    create_at: i64,
    edit_at: i64,
    file_ids: Vec<String>,
    reactions: Vec<(String, String, i64)>,
}

#[derive(Default)]
struct Data {
    next: u64,
    users: Vec<User>,
    team_id: String,
    channels: Vec<Channel>,
    posts: Vec<Post>,
    /// (user, channel) -> (msg_count, msg_count_root, mention_count, last_viewed_at)
    reads: HashMap<(String, String), (i64, i64, i64, i64)>,
    tokens: HashMap<String, String>,
    files: HashMap<String, (String, Vec<u8>)>,
}

impl Data {
    fn id(&mut self) -> String {
        self.next += 1;
        new_id(self.next)
    }
    fn user(&self, id: &str) -> Option<&User> {
        self.users.iter().find(|u| u.id == id)
    }
    fn channel_mut(&mut self, id: &str) -> Option<&mut Channel> {
        self.channels.iter_mut().find(|c| c.id == id)
    }
}

#[derive(Clone, Debug)]
enum Push {
    Event(Value),
    Kick,
}

#[derive(Clone)]
struct AppState {
    data: Arc<Mutex<Data>>,
    push: broadcast::Sender<Push>,
}

/// A running fake server. Dropping it does not stop it; it lives until
/// the runtime shuts down.
#[derive(Clone)]
pub struct FakeServer {
    pub addr: SocketAddr,
    state: AppState,
}

impl FakeServer {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Close every WebSocket connection (to test reconnecting).
    pub fn kick_websockets(&self) {
        let _ = self.state.push.send(Push::Kick);
    }

    pub fn user_id(&self, username: &str) -> String {
        let d = self.state.data.lock().unwrap();
        d.users
            .iter()
            .find(|u| u.username == username)
            .map(|u| u.id.clone())
            .unwrap_or_default()
    }

    pub fn channel_id(&self, name: &str) -> String {
        let d = self.state.data.lock().unwrap();
        d.channels
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.id.clone())
            .unwrap_or_default()
    }

    /// Post as `username` (like someone else typing in the app).
    pub fn post_as(&self, username: &str, channel_name: &str, message: &str) -> String {
        let user_id = self.user_id(username);
        let channel_id = self.channel_id(channel_name);
        create_post_inner(&self.state, &user_id, &channel_id, message, "", vec![]).0
    }

    /// Messages currently in a channel, oldest first.
    pub fn messages(&self, channel_name: &str) -> Vec<String> {
        let cid = self.channel_id(channel_name);
        let d = self.state.data.lock().unwrap();
        d.posts
            .iter()
            .filter(|p| p.channel_id == cid)
            .map(|p| p.message.clone())
            .collect()
    }

    pub fn root_of(&self, message: &str) -> String {
        let d = self.state.data.lock().unwrap();
        d.posts
            .iter()
            .find(|p| p.message == message)
            .map(|p| p.root_id.clone())
            .unwrap_or_default()
    }
}

fn seed() -> Data {
    let mut d = Data::default();
    let mk = |d: &mut Data, username: &str, first: &str, last: &str, mfa: Option<&str>| {
        let id = d.id();
        d.users.push(User {
            id: id.clone(),
            username: username.into(),
            first: first.into(),
            last: last.into(),
            password: "secret".into(),
            mfa: mfa.map(Into::into),
        });
        id
    };
    let alice = mk(&mut d, "alice", "Alice", "Anders", None);
    let bob = mk(&mut d, "bob", "Bob", "Brown", None);
    let carol = mk(&mut d, "carol", "Carol", "", Some("123456"));
    d.team_id = d.id();
    let everyone = vec![alice.clone(), bob.clone(), carol.clone()];
    for (name, display) in [("town-square", "Town Square"), ("off-topic", "Off-Topic")] {
        let id = d.id();
        d.channels.push(Channel {
            id,
            team_id: d.team_id.clone(),
            kind: "O",
            name: name.into(),
            display_name: display.into(),
            total: 0,
            total_root: 0,
            last_post_at: 0,
            members: everyone.clone(),
        });
    }
    d
}

/// Start on a random local port.
pub async fn start() -> FakeServer {
    start_on("127.0.0.1:0").await
}

pub async fn start_on(addr: &str) -> FakeServer {
    let (push, _) = broadcast::channel(256);
    let state = AppState {
        data: Arc::new(Mutex::new(seed())),
        push,
    };
    let server = FakeServer {
        addr: "0.0.0.0:0".parse().unwrap(),
        state: state.clone(),
    };

    // Some history so `read` has something to show.
    server.post_as("bob", "town-square", "Welcome to the fake server!");
    server.post_as("alice", "town-square", "Thanks, Bob.");

    let api = Router::new()
        .route("/system/ping", get(ping))
        .route("/config/client", get(client_config))
        .route("/users/login", post(login))
        .route("/users/logout", post(logout))
        .route("/users/me", get(me))
        .route("/users/ids", post(users_by_ids))
        .route("/users/username/{name}", get(user_by_name))
        .route("/users/{id}", get(user_by_id))
        .route("/users/me/teams", get(my_teams))
        .route("/users/me/teams/{tid}/channels", get(my_channels))
        .route("/users/me/teams/{tid}/channels/members", get(my_members))
        .route("/teams/name/{name}", get(team_by_name))
        .route("/teams/{tid}/channels/name/{name}", get(channel_by_name))
        .route("/teams/{tid}/posts/search", post(search))
        .route("/channels/direct", post(direct))
        .route("/channels/members/me/view", post(view))
        .route("/channels/{cid}", get(channel_by_id))
        .route("/channels/{cid}/members/me", get(my_member))
        .route("/channels/{cid}/posts", get(channel_posts))
        .route("/posts", post(create_post))
        .route("/posts/{pid}", get(post_by_id).delete(delete_post))
        .route("/posts/{pid}/patch", put(patch_post))
        .route("/posts/{pid}/thread", get(thread))
        .route("/reactions", post(add_reaction))
        .route("/files", post(upload))
        .route("/websocket", get(websocket));
    let app = Router::new().nest("/api/v4", api).with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    FakeServer { addr, state }
}

// ------------------------------------------------------------------ helpers

fn err(status: StatusCode, id: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "id": id, "message": message, "detailed_error": "", "request_id": "fake", "status_code": status.as_u16() })),
    )
        .into_response()
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "app.not_found", "Not found")
}

/// Check the bearer token and return the user id.
fn auth(st: &AppState, headers: &HeaderMap) -> Result<String, Response> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .unwrap_or_default();
    st.data
        .lock()
        .unwrap()
        .tokens
        .get(token)
        .cloned()
        .ok_or_else(|| {
            err(
                StatusCode::UNAUTHORIZED,
                "api.context.session_expired.app_error",
                "Invalid or expired session, please login again.",
            )
        })
}

macro_rules! authed {
    ($st:expr, $h:expr) => {
        match auth(&$st, &$h) {
            Ok(u) => u,
            Err(r) => return r,
        }
    };
}

fn user_json(u: &User) -> Value {
    json!({
        "id": u.id, "create_at": 1, "update_at": 1, "delete_at": 0,
        "username": u.username, "first_name": u.first, "last_name": u.last,
        "nickname": "", "email": format!("{}@example.com", u.username),
        "roles": "system_user", "locale": "en", "is_bot": false,
        "timezone": { "automaticTimezone": "", "manualTimezone": "", "useAutomaticTimezone": "true" },
        "props": {}, "notify_props": { "desktop": "mention" }
    })
}

fn team_json(d: &Data) -> Value {
    json!({ "id": d.team_id, "name": "dev", "display_name": "Dev Team", "type": "O", "create_at": 1, "update_at": 1, "delete_at": 0, "description": "" })
}

fn channel_json(c: &Channel) -> Value {
    json!({
        "id": c.id, "team_id": c.team_id, "type": c.kind, "name": c.name, "display_name": c.display_name,
        "header": "", "purpose": "", "create_at": 1, "update_at": 1, "delete_at": 0,
        "last_post_at": c.last_post_at, "last_root_post_at": c.last_post_at,
        "total_msg_count": c.total, "total_msg_count_root": c.total_root, "creator_id": "",
        "extra_update_at": 0, "props": null, "policy_id": null
    })
}

fn member_json(d: &Data, user: &str, c: &Channel) -> Value {
    let (count, root, mentions, viewed) = d
        .reads
        .get(&(user.to_string(), c.id.clone()))
        .copied()
        .unwrap_or_default();
    json!({
        "channel_id": c.id, "user_id": user, "roles": "channel_user",
        "last_viewed_at": viewed, "msg_count": count, "msg_count_root": root,
        "mention_count": mentions, "mention_count_root": mentions, "urgent_mention_count": 0,
        "notify_props": { "desktop": "default", "mark_unread": "all", "push": "default" },
        "last_update_at": viewed, "scheme_user": true, "scheme_admin": false
    })
}

fn post_json(d: &Data, p: &Post) -> Value {
    let reply_count = d
        .posts
        .iter()
        .filter(|r| r.root_id == p.id || (!p.root_id.is_empty() && r.root_id == p.root_id))
        .count();
    let files: Vec<Value> = p
        .file_ids
        .iter()
        .filter_map(|id| d.files.get(id).map(|(name, b)| json!({ "id": id, "name": name, "size": b.len(), "extension": "", "mime_type": "application/octet-stream" })))
        .collect();
    let reactions: Vec<Value> = p
        .reactions
        .iter()
        .map(|(u, e, t)| json!({ "user_id": u, "post_id": p.id, "emoji_name": e, "create_at": t }))
        .collect();
    let mut metadata = json!({});
    if !files.is_empty() {
        metadata["files"] = json!(files);
    }
    if !reactions.is_empty() {
        metadata["reactions"] = json!(reactions);
    }
    json!({
        "id": p.id, "create_at": p.create_at, "update_at": p.edit_at.max(p.create_at), "edit_at": p.edit_at,
        "delete_at": 0, "is_pinned": false, "user_id": p.user_id, "channel_id": p.channel_id,
        "root_id": p.root_id, "original_id": "", "message": p.message, "type": "",
        "props": {}, "hashtags": "", "pending_post_id": "", "reply_count": reply_count,
        "last_reply_at": 0, "participants": null, "file_ids": p.file_ids, "metadata": metadata
    })
}

fn post_list(d: &Data, posts: &[&Post]) -> Value {
    let mut sorted: Vec<&&Post> = posts.iter().collect();
    sorted.sort_by_key(|p| std::cmp::Reverse((p.create_at, p.id.clone())));
    let order: Vec<&str> = sorted.iter().map(|p| p.id.as_str()).collect();
    let map: serde_json::Map<String, Value> = posts
        .iter()
        .map(|p| (p.id.clone(), post_json(d, p)))
        .collect();
    json!({ "order": order, "posts": map, "next_post_id": "", "prev_post_id": "", "has_next": false })
}

fn create_post_inner(
    st: &AppState,
    user_id: &str,
    channel_id: &str,
    message: &str,
    root_id: &str,
    file_ids: Vec<String>,
) -> (String, Value) {
    let mut d = st.data.lock().unwrap();
    let id = d.id();
    // Strictly increasing timestamps keep ordering deterministic.
    let create_at = now().max(d.posts.last().map(|p| p.create_at + 1).unwrap_or(0));
    let post = Post {
        id: id.clone(),
        channel_id: channel_id.into(),
        user_id: user_id.into(),
        root_id: root_id.into(),
        message: message.into(),
        create_at,
        edit_at: 0,
        file_ids,
        reactions: vec![],
    };
    d.posts.push(post.clone());
    let is_root = root_id.is_empty();
    let (team_id, ch) = {
        let c = d.channel_mut(channel_id).expect("channel");
        c.total += 1;
        if is_root {
            c.total_root += 1;
        }
        c.last_post_at = create_at;
        (c.team_id.clone(), c.clone())
    };
    // The author has read their own post.
    let entry = d
        .reads
        .entry((user_id.to_string(), channel_id.to_string()))
        .or_default();
    *entry = (ch.total, ch.total_root, entry.2, create_at);
    // Mentions: @username in the text, or anyone in a DM.
    let mentioned: Vec<String> = d
        .users
        .iter()
        .filter(|u| {
            u.id != user_id
                && (message.contains(&format!("@{}", u.username))
                    || (ch.kind == "D" && ch.members.contains(&u.id)))
        })
        .map(|u| u.id.clone())
        .collect();
    for m in &mentioned {
        d.reads
            .entry((m.clone(), channel_id.to_string()))
            .or_default()
            .2 += 1;
    }
    let sender = d
        .user(user_id)
        .map(|u| u.username.clone())
        .unwrap_or_default();
    let pj = post_json(&d, &post);
    let event = json!({
        "event": "posted",
        "data": {
            "channel_display_name": if ch.kind == "D" { format!("@{sender}") } else { ch.display_name.clone() },
            "channel_name": ch.name, "channel_type": ch.kind,
            "mentions": serde_json::to_string(&mentioned).unwrap(),
            "post": pj.to_string(), "sender_name": format!("@{sender}"), "set_online": true,
            "team_id": if ch.kind == "D" { String::new() } else { team_id }
        },
        "broadcast": { "omit_users": null, "user_id": "", "channel_id": channel_id, "team_id": "" }
    });
    drop(d);
    let _ = st.push.send(Push::Event(event));
    (id, pj)
}

// ----------------------------------------------------------------- handlers

async fn ping() -> Response {
    (
        [("X-Version-Id", format!("{VERSION}.0.fake.false"))],
        Json(json!({ "status": "OK" })),
    )
        .into_response()
}

async fn client_config() -> Response {
    Json(json!({ "Version": VERSION, "SiteName": "Fake Mattermost", "CollapsedThreads": "default_off" })).into_response()
}

async fn login(State(st): State<AppState>, Json(body): Json<Value>) -> Response {
    let login_id = body["login_id"].as_str().unwrap_or_default();
    let password = body["password"].as_str().unwrap_or_default();
    let code = body["token"].as_str().unwrap_or_default();
    let mut d = st.data.lock().unwrap();
    let Some(user) = d
        .users
        .iter()
        .find(|u| u.username == login_id || format!("{}@example.com", u.username) == login_id)
        .cloned()
    else {
        return err(
            StatusCode::UNAUTHORIZED,
            "api.user.login.invalid_credentials_email_username",
            "Enter a valid email or username and/or password.",
        );
    };
    if user.password != password {
        return err(
            StatusCode::UNAUTHORIZED,
            "api.user.login.invalid_credentials_email_username",
            "Enter a valid email or username and/or password.",
        );
    }
    if let Some(mfa) = &user.mfa
        && code != mfa
    {
        return err(
            StatusCode::UNAUTHORIZED,
            "api.user.check_user_mfa.bad_code.app_error",
            "Invalid MFA token.",
        );
    }
    let token = format!("tok{}", d.id());
    d.tokens.insert(token.clone(), user.id.clone());
    ([("Token", token)], Json(user_json(&user))).into_response()
}

async fn logout(State(st): State<AppState>, h: HeaderMap) -> Response {
    let _ = authed!(st, h);
    let token = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_start_matches("Bearer ")
        .to_string();
    st.data.lock().unwrap().tokens.remove(&token);
    Json(json!({ "status": "OK" })).into_response()
}

async fn me(State(st): State<AppState>, h: HeaderMap) -> Response {
    let uid = authed!(st, h);
    let d = st.data.lock().unwrap();
    Json(user_json(d.user(&uid).unwrap())).into_response()
}

async fn user_by_id(State(st): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Response {
    let _ = authed!(st, h);
    let d = st.data.lock().unwrap();
    d.user(&id)
        .map(|u| Json(user_json(u)).into_response())
        .unwrap_or_else(not_found)
}

async fn user_by_name(
    State(st): State<AppState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let _ = authed!(st, h);
    let d = st.data.lock().unwrap();
    d.users
        .iter()
        .find(|u| u.username == name)
        .map(|u| Json(user_json(u)).into_response())
        .unwrap_or_else(not_found)
}

async fn users_by_ids(
    State(st): State<AppState>,
    h: HeaderMap,
    Json(ids): Json<Vec<String>>,
) -> Response {
    let _ = authed!(st, h);
    let d = st.data.lock().unwrap();
    Json(
        d.users
            .iter()
            .filter(|u| ids.contains(&u.id))
            .map(user_json)
            .collect::<Vec<_>>(),
    )
    .into_response()
}

async fn my_teams(State(st): State<AppState>, h: HeaderMap) -> Response {
    let _ = authed!(st, h);
    Json(vec![team_json(&st.data.lock().unwrap())]).into_response()
}

async fn team_by_name(
    State(st): State<AppState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let _ = authed!(st, h);
    let d = st.data.lock().unwrap();
    if name == "dev" {
        Json(team_json(&d)).into_response()
    } else {
        not_found()
    }
}

async fn my_channels(
    State(st): State<AppState>,
    h: HeaderMap,
    Path(_tid): Path<String>,
) -> Response {
    let uid = authed!(st, h);
    let d = st.data.lock().unwrap();
    Json(
        d.channels
            .iter()
            .filter(|c| c.members.contains(&uid))
            .map(channel_json)
            .collect::<Vec<_>>(),
    )
    .into_response()
}

async fn my_members(
    State(st): State<AppState>,
    h: HeaderMap,
    Path(_tid): Path<String>,
) -> Response {
    let uid = authed!(st, h);
    let d = st.data.lock().unwrap();
    Json(
        d.channels
            .iter()
            .filter(|c| c.members.contains(&uid))
            .map(|c| member_json(&d, &uid, c))
            .collect::<Vec<_>>(),
    )
    .into_response()
}

async fn my_member(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<String>) -> Response {
    let uid = authed!(st, h);
    let d = st.data.lock().unwrap();
    d.channels
        .iter()
        .find(|c| c.id == cid)
        .map(|c| Json(member_json(&d, &uid, c)).into_response())
        .unwrap_or_else(not_found)
}

async fn channel_by_id(
    State(st): State<AppState>,
    h: HeaderMap,
    Path(cid): Path<String>,
) -> Response {
    let _ = authed!(st, h);
    let d = st.data.lock().unwrap();
    d.channels
        .iter()
        .find(|c| c.id == cid)
        .map(|c| Json(channel_json(c)).into_response())
        .unwrap_or_else(not_found)
}

async fn channel_by_name(
    State(st): State<AppState>,
    h: HeaderMap,
    Path((tid, name)): Path<(String, String)>,
) -> Response {
    let _ = authed!(st, h);
    let d = st.data.lock().unwrap();
    d.channels
        .iter()
        .find(|c| c.team_id == tid && c.name == name)
        .map(|c| Json(channel_json(c)).into_response())
        .unwrap_or_else(not_found)
}

async fn direct(
    State(st): State<AppState>,
    h: HeaderMap,
    Json(ids): Json<Vec<String>>,
) -> Response {
    let _ = authed!(st, h);
    let mut d = st.data.lock().unwrap();
    let mut pair = ids.clone();
    pair.sort();
    if pair.len() != 2 || pair.iter().any(|id| d.user(id).is_none()) {
        return err(
            StatusCode::BAD_REQUEST,
            "api.channel.create_direct_channel.invalid_user.app_error",
            "Invalid user",
        );
    }
    let name = format!("{}__{}", pair[0], pair[1]);
    if let Some(c) = d.channels.iter().find(|c| c.name == name) {
        return Json(channel_json(c)).into_response();
    }
    let id = d.id();
    let c = Channel {
        id,
        team_id: String::new(),
        kind: "D",
        name,
        display_name: String::new(),
        total: 0,
        total_root: 0,
        last_post_at: 0,
        members: pair,
    };
    d.channels.push(c.clone());
    (StatusCode::CREATED, Json(channel_json(&c))).into_response()
}

async fn view(State(st): State<AppState>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    let uid = authed!(st, h);
    let cid = body["channel_id"].as_str().unwrap_or_default().to_string();
    let mut d = st.data.lock().unwrap();
    let Some(c) = d.channels.iter().find(|c| c.id == cid).cloned() else {
        return not_found();
    };
    d.reads.insert(
        (uid.clone(), cid.clone()),
        (c.total, c.total_root, 0, now()),
    );
    drop(d);
    let _ = st.push.send(Push::Event(json!({
        "event": "multiple_channels_viewed",
        "data": { "channel_times": { cid: now() } },
        "broadcast": { "user_id": uid, "channel_id": "", "team_id": "" }
    })));
    Json(json!({ "status": "OK", "last_viewed_at_times": {} })).into_response()
}

async fn channel_posts(
    State(st): State<AppState>,
    h: HeaderMap,
    Path(cid): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let _ = authed!(st, h);
    let d = st.data.lock().unwrap();
    let per_page: usize = q.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(60);
    let mut posts: Vec<&Post> = d.posts.iter().filter(|p| p.channel_id == cid).collect();
    posts.sort_by_key(|p| std::cmp::Reverse(p.create_at));
    posts.truncate(per_page);
    Json(post_list(&d, &posts)).into_response()
}

async fn create_post(
    State(st): State<AppState>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let uid = authed!(st, h);
    let cid = body["channel_id"].as_str().unwrap_or_default().to_string();
    let message = body["message"].as_str().unwrap_or_default().to_string();
    let root = body["root_id"].as_str().unwrap_or_default().to_string();
    let files: Vec<String> = body["file_ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    {
        let d = st.data.lock().unwrap();
        match d.channels.iter().find(|c| c.id == cid) {
            None => {
                return err(
                    StatusCode::FORBIDDEN,
                    "api.context.permissions.app_error",
                    "You do not have the appropriate permissions.",
                );
            }
            Some(c) if !c.members.contains(&uid) => {
                return err(
                    StatusCode::FORBIDDEN,
                    "api.context.permissions.app_error",
                    "You do not have the appropriate permissions.",
                );
            }
            _ => {}
        }
        if !root.is_empty() && !d.posts.iter().any(|p| p.id == root && p.root_id.is_empty()) {
            return err(
                StatusCode::BAD_REQUEST,
                "api.post.create_post.root_id.app_error",
                "Invalid RootId parameter.",
            );
        }
    }
    let (_, pj) = create_post_inner(&st, &uid, &cid, &message, &root, files);
    (StatusCode::CREATED, Json(pj)).into_response()
}

async fn post_by_id(State(st): State<AppState>, h: HeaderMap, Path(pid): Path<String>) -> Response {
    let _ = authed!(st, h);
    let d = st.data.lock().unwrap();
    d.posts
        .iter()
        .find(|p| p.id == pid)
        .map(|p| Json(post_json(&d, p)).into_response())
        .unwrap_or_else(not_found)
}

async fn delete_post(
    State(st): State<AppState>,
    h: HeaderMap,
    Path(pid): Path<String>,
) -> Response {
    let _ = authed!(st, h);
    let mut d = st.data.lock().unwrap();
    d.posts.retain(|p| p.id != pid);
    Json(json!({ "status": "OK" })).into_response()
}

async fn patch_post(
    State(st): State<AppState>,
    h: HeaderMap,
    Path(pid): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let uid = authed!(st, h);
    let mut d = st.data.lock().unwrap();
    let Some(i) = d.posts.iter().position(|p| p.id == pid) else {
        return not_found();
    };
    if d.posts[i].user_id != uid {
        return err(
            StatusCode::FORBIDDEN,
            "api.context.permissions.app_error",
            "You do not have the appropriate permissions.",
        );
    }
    if let Some(m) = body["message"].as_str() {
        d.posts[i].message = m.to_string();
        d.posts[i].edit_at = now();
    }
    let p = d.posts[i].clone();
    let pj = post_json(&d, &p);
    drop(d);
    let _ = st.push.send(Push::Event(json!({
        "event": "post_edited", "data": { "post": pj.to_string() },
        "broadcast": { "channel_id": p.channel_id, "user_id": "", "team_id": "" }
    })));
    Json(pj).into_response()
}

async fn thread(State(st): State<AppState>, h: HeaderMap, Path(pid): Path<String>) -> Response {
    let _ = authed!(st, h);
    let d = st.data.lock().unwrap();
    let Some(p) = d.posts.iter().find(|p| p.id == pid) else {
        return not_found();
    };
    let root = if p.root_id.is_empty() {
        p.id.clone()
    } else {
        p.root_id.clone()
    };
    let posts: Vec<&Post> = d
        .posts
        .iter()
        .filter(|p| p.id == root || p.root_id == root)
        .collect();
    Json(post_list(&d, &posts)).into_response()
}

async fn search(
    State(st): State<AppState>,
    h: HeaderMap,
    Path(_tid): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let _ = authed!(st, h);
    let terms = body["terms"].as_str().unwrap_or_default().to_lowercase();
    let d = st.data.lock().unwrap();
    let posts: Vec<&Post> = d
        .posts
        .iter()
        .filter(|p| {
            terms
                .split_whitespace()
                .all(|t| p.message.to_lowercase().contains(t))
        })
        .collect();
    Json(post_list(&d, &posts)).into_response()
}

async fn add_reaction(
    State(st): State<AppState>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let uid = authed!(st, h);
    let pid = body["post_id"].as_str().unwrap_or_default();
    let emoji = body["emoji_name"].as_str().unwrap_or_default().to_string();
    let mut d = st.data.lock().unwrap();
    let Some(p) = d.posts.iter_mut().find(|p| p.id == pid) else {
        return not_found();
    };
    let t = now();
    p.reactions.push((uid.clone(), emoji.clone(), t));
    let r = json!({ "user_id": uid, "post_id": pid, "emoji_name": emoji, "create_at": t });
    let cid = p.channel_id.clone();
    drop(d);
    let _ = st.push.send(Push::Event(json!({
        "event": "reaction_added", "data": { "reaction": r.to_string() },
        "broadcast": { "channel_id": cid, "user_id": "", "team_id": "" }
    })));
    Json(r).into_response()
}

async fn upload(
    State(st): State<AppState>,
    h: HeaderMap,
    mut form: axum::extract::Multipart,
) -> Response {
    let uid = authed!(st, h);
    let mut infos = Vec::new();
    while let Ok(Some(field)) = form.next_field().await {
        if field.name() != Some("files") {
            continue;
        }
        let name = field.file_name().unwrap_or("file").to_string();
        let bytes = field.bytes().await.unwrap_or_default().to_vec();
        let mut d = st.data.lock().unwrap();
        let id = d.id();
        infos.push(json!({ "id": id, "user_id": uid, "name": name, "size": bytes.len(), "extension": "", "mime_type": "application/octet-stream" }));
        d.files.insert(id, (name, bytes));
    }
    (
        StatusCode::CREATED,
        Json(json!({ "file_infos": infos, "client_ids": [] })),
    )
        .into_response()
}

async fn websocket(State(st): State<AppState>, h: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let uid = authed!(st, h);
    ws.on_upgrade(move |socket| ws_session(st, uid, socket))
}

async fn ws_session(st: AppState, user_id: String, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut pushes = st.push.subscribe();
    let connection_id = st.data.lock().unwrap().id();
    let mut seq: i64 = 0;
    let hello = json!({
        "event": "hello", "seq": seq,
        "data": { "connection_id": connection_id, "server_version": format!("{VERSION}.0.fake.false"), "server_hostname": "fake" },
        "broadcast": { "user_id": user_id, "channel_id": "", "team_id": "" }
    });
    if tx
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            push = pushes.recv() => {
                let mut event = match push {
                    Ok(Push::Event(e)) => e,
                    Ok(Push::Kick) | Err(_) => { let _ = tx.send(Message::Close(None)).await; return; }
                };
                // Events targeted at one user go only to them.
                let target = event["broadcast"]["user_id"].as_str().unwrap_or_default().to_string();
                if !target.is_empty() && target != user_id {
                    continue;
                }
                if event["broadcast"]["omit_users"][&user_id] == json!(true) {
                    continue;
                }
                seq += 1;
                event["seq"] = json!(seq);
                if tx.send(Message::Text(event.to_string().into())).await.is_err() {
                    return;
                }
            }
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { return };
                if let Message::Text(t) = msg {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    let reply = json!({ "status": "OK", "seq_reply": v["seq"] });
                    if tx.send(Message::Text(reply.to_string().into())).await.is_err() { return; }
                    if v["action"] == "user_typing" {
                        let _ = st.push.send(Push::Event(json!({
                            "event": "typing",
                            "data": { "parent_id": v["data"]["parent_id"], "user_id": user_id },
                            "broadcast": { "channel_id": v["data"]["channel_id"], "user_id": "", "team_id": "",
                                           "omit_users": { user_id.clone(): true } }
                        })));
                    }
                }
            }
        }
    }
}
