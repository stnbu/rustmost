//! An async client for the [Mattermost](https://mattermost.com) v4 API,
//! written in Rust.
//!
//! ```no_run
//! # async fn demo() -> rustmost_api::Result<()> {
//! use rustmost_api::{Client, CreatePost, PostsQuery, WsEvent};
//!
//! let mut client = Client::new("https://chat.example.com")?;
//! let (me, _token) = client.login("alice", "hunter2", None).await?;
//!
//! let team = &client.my_teams().await?[0];
//! let town = client.channel_by_name(&team.id, "town-square").await?;
//! for post in client.posts(&town.id, &PostsQuery::latest(20)).await?.oldest_first() {
//!     println!("{}: {}", post.user_id, post.message);
//! }
//! client.create_post(&CreatePost::new(&town.id, "hello from Rust")).await?;
//!
//! let mut ws = client.websocket().await?;
//! while let Some(event) = ws.next().await {
//!     if let WsEvent::Posted(p) = event {
//!         println!("{} in {}: {}", p.sender_name, p.channel_display_name, p.post.message);
//!     }
//! }
//! # let _ = me; Ok(()) }
//! ```

mod client;
mod error;
pub mod models;
pub mod websocket;

pub use client::{Client, OptionalExt, normalize_server_url};
pub use error::{ApiError, Error, Result};
pub use models::*;
pub use websocket::{PostedEvent, RawEvent, WebSocket, WebSocketHandle, WebSocketOptions, WsEvent};
