use std::time::Duration;

use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use url::Url;

use crate::error::{ApiError, Error, Result};
use crate::models::*;
use crate::websocket::{WebSocket, WebSocketOptions};

const USER_AGENT: &str = concat!("rustmost/", env!("CARGO_PKG_VERSION"));

/// Install the rustls crypto provider once (needed by the WebSocket TLS stack).
pub(crate) fn ensure_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Normalize what a person types as a server address into a base URL with a
/// trailing slash: `chat.example.com` -> `https://chat.example.com/`.
pub fn normalize_server_url(input: &str) -> Result<Url> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::Other("server URL is empty".into()));
    }
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let mut url = Url::parse(&with_scheme)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Other(format!(
            "unsupported URL scheme: {}",
            url.scheme()
        )));
    }
    url.set_query(None);
    url.set_fragment(None);
    if !url.path().ends_with('/') {
        let p = format!("{}/", url.path());
        url.set_path(&p);
    }
    Ok(url)
}

/// An async client for the Mattermost REST API (v4).
///
/// Cheap to clone; clones share the same connection pool.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    server: Url,
    api: Url,
    token: Option<String>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("server", &self.server.as_str())
            .field("authenticated", &self.token.is_some())
            .finish()
    }
}

impl Client {
    /// Create a client for `server` (e.g. `https://chat.example.com`).
    pub fn new(server: &str) -> Result<Self> {
        ensure_crypto_provider();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()?;
        Self::with_http_client(server, http)
    }

    /// Create a client using your own configured `reqwest::Client`.
    pub fn with_http_client(server: &str, http: reqwest::Client) -> Result<Self> {
        let server = normalize_server_url(server)?;
        let api = server.join("api/v4/")?;
        Ok(Self {
            http,
            server,
            api,
            token: None,
        })
    }

    /// Use an existing session token or personal access token.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn set_token(&mut self, token: Option<String>) {
        self.token = token;
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// The server base URL (with a trailing slash).
    pub fn server_url(&self) -> &Url {
        &self.server
    }

    // ---------------------------------------------------------------- plumbing

    fn url(&self, path: &str) -> Result<Url> {
        Ok(self.api.join(path.trim_start_matches('/'))?)
    }

    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        let mut rb = self
            .http
            .request(method, self.url(path)?)
            .header("X-Requested-With", "XMLHttpRequest");
        if let Some(t) = &self.token {
            rb = rb.bearer_auth(t);
        }
        Ok(rb)
    }

    async fn send(rb: RequestBuilder) -> Result<Response> {
        let resp = rb.send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let text = resp.text().await.unwrap_or_default();
        let mut error: ApiError = serde_json::from_str(&text).unwrap_or_else(|_| ApiError {
            message: if text.is_empty() {
                status.canonical_reason().unwrap_or("error").to_string()
            } else {
                text
            },
            ..Default::default()
        });
        if error.status_code == 0 {
            error.status_code = status.as_u16();
        }
        Err(Error::Api {
            status: status.as_u16(),
            error,
        })
    }

    async fn json<T: DeserializeOwned>(resp: Response) -> Result<T> {
        let bytes = resp.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::json(Self::send(self.request(Method::GET, path)?).await?).await
    }

    async fn get_query<T: DeserializeOwned, Q: Serialize + ?Sized>(
        &self,
        path: &str,
        q: &Q,
    ) -> Result<T> {
        Self::json(Self::send(self.request(Method::GET, path)?.query(q)).await?).await
    }

    async fn post<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        Self::json(Self::send(self.request(Method::POST, path)?.json(body)).await?).await
    }

    async fn post_no_content<B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<()> {
        Self::send(self.request(Method::POST, path)?.json(body)).await?;
        Ok(())
    }

    async fn put<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        Self::json(Self::send(self.request(Method::PUT, path)?.json(body)).await?).await
    }

    async fn delete(&self, path: &str) -> Result<()> {
        Self::send(self.request(Method::DELETE, path)?).await?;
        Ok(())
    }

    fn require_token(&self) -> Result<&str> {
        self.token.as_deref().ok_or(Error::NotAuthenticated)
    }

    // ------------------------------------------------------------------ system

    /// Check the server is reachable. Returns the server version from the
    /// `X-Version-Id` header (e.g. `11.7.3`), if the server sent one.
    pub async fn ping(&self) -> Result<Option<String>> {
        let resp = Self::send(self.request(Method::GET, "system/ping")?).await?;
        Ok(resp
            .headers()
            .get("X-Version-Id")
            .and_then(|v| v.to_str().ok())
            .map(parse_version_header))
    }

    /// The limited client config any visitor can read (site name, version,
    /// feature flags such as `CollapsedThreads`).
    pub async fn client_config(&self) -> Result<std::collections::HashMap<String, String>> {
        self.get_query("config/client", &[("format", "old")]).await
    }

    // -------------------------------------------------------------------- auth

    /// Log in with a username or email and password. `mfa_code` is the
    /// 6-digit code from your authenticator app, if MFA is enabled.
    ///
    /// On success the session token is stored in the client and returned
    /// alongside the user.
    pub async fn login(
        &mut self,
        login_id: &str,
        password: &str,
        mfa_code: Option<&str>,
    ) -> Result<(User, String)> {
        let mut body = json!({ "login_id": login_id, "password": password });
        if let Some(code) = mfa_code.filter(|c| !c.trim().is_empty()) {
            body["token"] = json!(code.trim());
        }
        // Don't send a stale token with a login request.
        let rb = self
            .http
            .post(self.url("users/login")?)
            .header("X-Requested-With", "XMLHttpRequest")
            .json(&body);
        let resp = Self::send(rb).await?;
        let token = resp
            .headers()
            .get("Token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or(Error::MissingToken)?;
        let user: User = Self::json(resp).await?;
        self.token = Some(token.clone());
        Ok((user, token))
    }

    /// End the session on the server and forget the token.
    pub async fn logout(&mut self) -> Result<()> {
        self.require_token()?;
        let r = self.post_no_content("users/logout", &json!({})).await;
        self.token = None;
        r
    }

    // ------------------------------------------------------------------- users

    pub async fn me(&self) -> Result<User> {
        self.get("users/me").await
    }

    pub async fn user(&self, user_id: &str) -> Result<User> {
        self.get(&format!("users/{user_id}")).await
    }

    pub async fn user_by_username(&self, username: &str) -> Result<User> {
        self.get(&format!(
            "users/username/{}",
            username.trim_start_matches('@')
        ))
        .await
    }

    /// Look up many users at once.
    pub async fn users_by_ids(&self, ids: &[String]) -> Result<Vec<User>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.post("users/ids", ids).await
    }

    pub async fn statuses_by_ids(&self, ids: &[String]) -> Result<Vec<Status>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.post("users/status/ids", ids).await
    }

    /// A user's preferences (used e.g. to see whether Collapsed Reply
    /// Threads is on: category `display_settings`, name `collapsed_reply_threads`).
    pub async fn my_preferences(&self) -> Result<Vec<serde_json::Value>> {
        self.get("users/me/preferences").await
    }

    // ------------------------------------------------------------------- teams

    pub async fn my_teams(&self) -> Result<Vec<Team>> {
        self.get("users/me/teams").await
    }

    pub async fn team(&self, team_id: &str) -> Result<Team> {
        self.get(&format!("teams/{team_id}")).await
    }

    pub async fn team_by_name(&self, name: &str) -> Result<Team> {
        self.get(&format!("teams/name/{name}")).await
    }

    // ---------------------------------------------------------------- channels

    /// All channels you belong to in a team, including DMs and group messages.
    pub async fn my_channels(&self, team_id: &str) -> Result<Vec<Channel>> {
        self.get(&format!("users/me/teams/{team_id}/channels"))
            .await
    }

    /// Your membership (read state, mention counts) for every channel in a team.
    pub async fn my_channel_members(&self, team_id: &str) -> Result<Vec<ChannelMember>> {
        self.get(&format!("users/me/teams/{team_id}/channels/members"))
            .await
    }

    pub async fn my_channel_member(&self, channel_id: &str) -> Result<ChannelMember> {
        self.get(&format!("channels/{channel_id}/members/me")).await
    }

    pub async fn channel(&self, channel_id: &str) -> Result<Channel> {
        self.get(&format!("channels/{channel_id}")).await
    }

    pub async fn channel_by_name(&self, team_id: &str, name: &str) -> Result<Channel> {
        self.get(&format!("teams/{team_id}/channels/name/{name}"))
            .await
    }

    /// Get or create the DM channel between you and another user.
    pub async fn direct_channel(&self, my_id: &str, other_user_id: &str) -> Result<Channel> {
        self.post("channels/direct", &[my_id, other_user_id]).await
    }

    /// Mark a channel as read (what happens when you open it in the app).
    pub async fn view_channel(&self, channel_id: &str) -> Result<()> {
        self.post_no_content(
            "channels/members/me/view",
            &json!({ "channel_id": channel_id, "prev_channel_id": "" }),
        )
        .await
    }

    // ------------------------------------------------------------------- posts

    pub async fn posts(&self, channel_id: &str, query: &PostsQuery) -> Result<PostList> {
        self.get_query(&format!("channels/{channel_id}/posts"), &query.to_pairs())
            .await
    }

    pub async fn post_by_id(&self, post_id: &str) -> Result<Post> {
        self.get(&format!("posts/{post_id}")).await
    }

    /// A root post and all its replies.
    pub async fn thread(&self, post_id: &str) -> Result<PostList> {
        self.get(&format!("posts/{post_id}/thread")).await
    }

    pub async fn create_post(&self, post: &CreatePost) -> Result<Post> {
        self.post("posts", post).await
    }

    /// Change a post's text.
    pub async fn edit_post(&self, post_id: &str, message: &str) -> Result<Post> {
        self.put(
            &format!("posts/{post_id}/patch"),
            &json!({ "message": message }),
        )
        .await
    }

    pub async fn delete_post(&self, post_id: &str) -> Result<()> {
        self.delete(&format!("posts/{post_id}")).await
    }

    /// Search posts in a team. `terms` uses Mattermost search syntax
    /// (`from:alice in:town-square "exact phrase"`).
    pub async fn search_posts(&self, team_id: &str, terms: &str) -> Result<PostList> {
        self.post(
            &format!("teams/{team_id}/posts/search"),
            &json!({ "terms": terms, "is_or_search": false }),
        )
        .await
    }

    pub async fn pinned_posts(&self, channel_id: &str) -> Result<PostList> {
        self.get(&format!("channels/{channel_id}/pinned")).await
    }

    // --------------------------------------------------------------- reactions

    pub async fn add_reaction(
        &self,
        user_id: &str,
        post_id: &str,
        emoji_name: &str,
    ) -> Result<Reaction> {
        self.post(
            "reactions",
            &json!({ "user_id": user_id, "post_id": post_id, "emoji_name": emoji_name.trim_matches(':') }),
        )
        .await
    }

    pub async fn remove_reaction(
        &self,
        user_id: &str,
        post_id: &str,
        emoji_name: &str,
    ) -> Result<()> {
        self.delete(&format!(
            "users/{user_id}/posts/{post_id}/reactions/{}",
            emoji_name.trim_matches(':')
        ))
        .await
    }

    // ----------------------------------------------------------------- threads

    /// Threads you follow in a team (Collapsed Reply Threads).
    pub async fn my_threads(&self, team_id: &str, unread_only: bool) -> Result<UserThreads> {
        self.get_query(
            &format!("users/me/teams/{team_id}/threads"),
            &[
                ("unread", unread_only.to_string()),
                ("extended", "true".to_string()),
            ],
        )
        .await
    }

    // ------------------------------------------------------------------- files

    /// Upload a file to a channel; attach the returned id with
    /// [`CreatePost::file_ids`].
    pub async fn upload_file(
        &self,
        channel_id: &str,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<FileInfo> {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new()
            .text("channel_id", channel_id.to_string())
            .part("files", part);
        let resp = Self::send(self.request(Method::POST, "files")?.multipart(form)).await?;
        let r: FileUploadResponse = Self::json(resp).await?;
        r.file_infos
            .into_iter()
            .next()
            .ok_or_else(|| Error::Other("server returned no file info".into()))
    }

    pub async fn download_file(&self, file_id: &str) -> Result<Vec<u8>> {
        let resp = Self::send(self.request(Method::GET, &format!("files/{file_id}"))?).await?;
        Ok(resp.bytes().await?.to_vec())
    }

    // --------------------------------------------------------------- websocket

    /// Open the real-time event stream. It reconnects on its own.
    pub async fn websocket(&self) -> Result<WebSocket> {
        self.websocket_with(WebSocketOptions::default()).await
    }

    pub async fn websocket_with(&self, options: WebSocketOptions) -> Result<WebSocket> {
        let token = self.require_token()?.to_string();
        let mut url = self.api.join("websocket")?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| Error::Other("cannot build websocket URL".into()))?;
        WebSocket::connect(url, token, options).await
    }
}

/// `11.7.3.12345.abc.true` -> `11.7.3`
fn parse_version_header(v: &str) -> String {
    v.split('.').take(3).collect::<Vec<_>>().join(".")
}

/// Treat 404 as `None`.
pub trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExt<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.status() == Some(StatusCode::NOT_FOUND.as_u16()) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_server_url("chat.example.com").unwrap().as_str(),
            "https://chat.example.com/"
        );
        assert_eq!(
            normalize_server_url("http://localhost:8065")
                .unwrap()
                .as_str(),
            "http://localhost:8065/"
        );
        assert_eq!(
            normalize_server_url("https://example.com/mattermost")
                .unwrap()
                .as_str(),
            "https://example.com/mattermost/"
        );
        assert!(normalize_server_url("ftp://x").is_err());
    }

    #[test]
    fn parses_version() {
        assert_eq!(parse_version_header("11.7.3.12345.abc.true"), "11.7.3");
        assert_eq!(parse_version_header("11.7.3"), "11.7.3");
    }
}
