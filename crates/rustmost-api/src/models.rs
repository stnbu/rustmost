//! Data types for the Mattermost v4 API.
//!
//! Every field uses `#[serde(default)]` so the client keeps working when the
//! server adds, omits or renames optional fields between versions.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Milliseconds since the Unix epoch, as Mattermost uses everywhere.
pub type Millis = i64;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
    pub id: String,
    pub create_at: Millis,
    pub update_at: Millis,
    pub delete_at: Millis,
    pub username: String,
    pub email: String,
    pub nickname: String,
    pub first_name: String,
    pub last_name: String,
    pub position: String,
    pub roles: String,
    pub locale: String,
    pub is_bot: bool,
    pub timezone: Value,
}

impl User {
    /// "First Last" if set, otherwise the nickname, otherwise the username.
    pub fn display_name(&self) -> String {
        let full = format!("{} {}", self.first_name, self.last_name);
        let full = full.trim();
        if !full.is_empty() {
            full.to_string()
        } else if !self.nickname.is_empty() {
            self.nickname.clone()
        } else {
            self.username.clone()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Team {
    pub id: String,
    pub create_at: Millis,
    pub update_at: Millis,
    pub delete_at: Millis,
    pub display_name: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub team_type: String,
}

/// Channel type as the server encodes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChannelType {
    #[default]
    #[serde(rename = "O")]
    Open,
    #[serde(rename = "P")]
    Private,
    #[serde(rename = "D")]
    Direct,
    #[serde(rename = "G")]
    Group,
}

impl ChannelType {
    pub fn is_dm_or_group(self) -> bool {
        matches!(self, ChannelType::Direct | ChannelType::Group)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Channel {
    pub id: String,
    pub create_at: Millis,
    pub update_at: Millis,
    pub delete_at: Millis,
    pub team_id: String,
    #[serde(rename = "type")]
    pub channel_type: ChannelType,
    pub display_name: String,
    pub name: String,
    pub header: String,
    pub purpose: String,
    pub last_post_at: Millis,
    pub last_root_post_at: Millis,
    pub total_msg_count: i64,
    pub total_msg_count_root: i64,
    pub creator_id: String,
}

impl Channel {
    /// For a DM channel (`<id>__<id>`), the id of the other user.
    pub fn dm_other_user(&self, me: &str) -> Option<String> {
        if self.channel_type != ChannelType::Direct {
            return None;
        }
        let (a, b) = self.name.split_once("__")?;
        Some(if a == me { b } else { a }.to_string())
    }
}

/// The current user's membership in a channel: read state and mentions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelMember {
    pub channel_id: String,
    pub user_id: String,
    pub roles: String,
    pub last_viewed_at: Millis,
    pub msg_count: i64,
    pub msg_count_root: i64,
    pub mention_count: i64,
    pub mention_count_root: i64,
    pub urgent_mention_count: i64,
    pub last_update_at: Millis,
    pub notify_props: HashMap<String, Value>,
}

impl ChannelMember {
    pub fn is_muted(&self) -> bool {
        self.notify_props.get("mark_unread").and_then(Value::as_str) == Some("mention")
    }
}

/// Unread counts for a channel, computed from [`Channel`] and [`ChannelMember`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unread {
    pub messages: i64,
    pub mentions: i64,
}

impl Unread {
    /// `collapsed_threads`: count only root posts, as the web app does when
    /// Collapsed Reply Threads is on.
    pub fn compute(channel: &Channel, member: &ChannelMember, collapsed_threads: bool) -> Self {
        let (total, read, mentions) = if collapsed_threads {
            (
                channel.total_msg_count_root,
                member.msg_count_root,
                member.mention_count_root,
            )
        } else {
            (
                channel.total_msg_count,
                member.msg_count,
                member.mention_count,
            )
        };
        Unread {
            messages: (total - read).max(0),
            mentions,
        }
    }

    pub fn is_unread(&self) -> bool {
        self.messages > 0 || self.mentions > 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Post {
    pub id: String,
    pub create_at: Millis,
    pub update_at: Millis,
    pub edit_at: Millis,
    pub delete_at: Millis,
    pub is_pinned: bool,
    pub user_id: String,
    pub channel_id: String,
    pub root_id: String,
    pub original_id: String,
    pub message: String,
    #[serde(rename = "type")]
    pub post_type: String,
    pub props: Value,
    pub hashtags: String,
    pub file_ids: Vec<String>,
    pub pending_post_id: String,
    pub reply_count: i64,
    pub last_reply_at: Millis,
    pub metadata: Option<PostMetadata>,
}

impl Post {
    pub fn is_reply(&self) -> bool {
        !self.root_id.is_empty()
    }

    /// System messages (joins, header changes, ...) have a `system_` type.
    pub fn is_system(&self) -> bool {
        self.post_type.starts_with("system_")
    }

    pub fn is_edited(&self) -> bool {
        self.edit_at > 0
    }

    /// Username override set by bots/webhooks (`props.override_username`).
    pub fn override_username(&self) -> Option<&str> {
        self.props.get("override_username")?.as_str()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostMetadata {
    pub reactions: Vec<Reaction>,
    pub files: Vec<FileInfo>,
    pub emojis: Vec<Value>,
    pub embeds: Vec<Value>,
}

/// A page of posts. `order` lists ids newest first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostList {
    pub order: Vec<String>,
    pub posts: HashMap<String, Post>,
    pub next_post_id: String,
    pub prev_post_id: String,
    pub has_next: bool,
}

impl PostList {
    /// Posts in `order`, newest first.
    pub fn newest_first(&self) -> Vec<&Post> {
        self.order
            .iter()
            .filter_map(|id| self.posts.get(id))
            .collect()
    }

    /// Posts in `order`, oldest first (the natural reading order).
    pub fn oldest_first(&self) -> Vec<&Post> {
        let mut v = self.newest_first();
        v.reverse();
        v
    }

    /// All posts sorted by creation time, oldest first. Useful for threads,
    /// where the server may not fill `order` in reading order.
    pub fn chronological(&self) -> Vec<&Post> {
        let mut v: Vec<&Post> = self.posts.values().collect();
        v.sort_by_key(|p| (p.create_at, p.id.clone()));
        v
    }
}

/// Body for creating a post.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CreatePost {
    pub channel_id: String,
    pub message: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub root_id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub file_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub props: Option<Value>,
}

impl CreatePost {
    pub fn new(channel_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            message: message.into(),
            ..Default::default()
        }
    }

    pub fn reply_to(mut self, root_id: impl Into<String>) -> Self {
        self.root_id = root_id.into();
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reaction {
    pub user_id: String,
    pub post_id: String,
    pub emoji_name: String,
    pub create_at: Millis,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileInfo {
    pub id: String,
    pub user_id: String,
    pub post_id: String,
    pub channel_id: String,
    pub create_at: Millis,
    pub name: String,
    pub extension: String,
    pub size: i64,
    pub mime_type: String,
    pub width: i64,
    pub height: i64,
    pub has_preview_image: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FileUploadResponse {
    pub file_infos: Vec<FileInfo>,
    pub client_ids: Vec<String>,
}

/// A user's online status.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub user_id: String,
    pub status: String,
    pub manual: bool,
    pub last_activity_at: Millis,
}

/// A followed thread (Collapsed Reply Threads).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserThread {
    pub id: String,
    pub reply_count: i64,
    pub last_reply_at: Millis,
    pub last_viewed_at: Millis,
    pub participants: Vec<User>,
    pub post: Post,
    pub unread_replies: i64,
    pub unread_mentions: i64,
    pub is_urgent: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserThreads {
    pub total: i64,
    pub total_unread_threads: i64,
    pub total_unread_mentions: i64,
    pub threads: Vec<UserThread>,
}

/// Paging options for fetching posts in a channel.
#[derive(Debug, Clone, Default)]
pub struct PostsQuery {
    pub page: Option<u32>,
    /// Max 200 on the server.
    pub per_page: Option<u32>,
    /// Only posts created/modified after this time (ms).
    pub since: Option<Millis>,
    /// Posts before this post id.
    pub before: Option<String>,
    /// Posts after this post id.
    pub after: Option<String>,
}

impl PostsQuery {
    pub fn latest(per_page: u32) -> Self {
        Self {
            per_page: Some(per_page),
            ..Default::default()
        }
    }

    pub(crate) fn to_pairs(&self) -> Vec<(&'static str, String)> {
        let mut q = Vec::new();
        if let Some(p) = self.page {
            q.push(("page", p.to_string()));
        }
        if let Some(p) = self.per_page {
            q.push(("per_page", p.to_string()));
        }
        if let Some(s) = self.since {
            q.push(("since", s.to_string()));
        }
        if let Some(b) = &self.before {
            q.push(("before", b.clone()));
        }
        if let Some(a) = &self.after {
            q.push(("after", a.clone()));
        }
        q
    }
}
