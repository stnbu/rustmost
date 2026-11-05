//! TUI state and everything that changes it: keys, server events, timers.
//! Rendering lives in `ui.rs` and only reads this state.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rustmost_api::{
    Channel, ChannelType, CreatePost, Post, PostsQuery, Team, Unread, WebSocketHandle, WsEvent,
};

use super::input::LineEditor;
use crate::session::Session;

const PAGE: u32 = 60;
const TYPING_SHOWN_FOR: Duration = Duration::from_secs(6);
const TYPING_SEND_EVERY: Duration = Duration::from_secs(3);
const FLASH_FOR: Duration = Duration::from_secs(6);

#[derive(Debug, Clone)]
pub struct Entry {
    pub channel: Channel,
    pub label: String,
    pub unread: Unread,
    pub muted: bool,
}

impl Entry {
    pub fn is_dm(&self) -> bool {
        self.channel.channel_type.is_dm_or_group()
    }
}

#[derive(Debug, Default)]
pub struct ChannelView {
    /// Oldest first.
    pub posts: Vec<Post>,
    pub has_more: bool,
}

impl ChannelView {
    fn upsert(&mut self, post: Post) {
        if let Some(existing) = self.posts.iter_mut().find(|p| p.id == post.id) {
            *existing = post;
            return;
        }
        let at = self
            .posts
            .partition_point(|p| p.create_at <= post.create_at);
        self.posts.insert(at, post);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Connection {
    Connecting,
    Connected,
    Reconnecting { reason: String, retry_in: Duration },
    Lost(String),
}

#[derive(Debug, Default)]
pub struct Switcher {
    pub query: LineEditor,
    pub selected: usize,
}

pub struct Flash {
    pub text: String,
    pub error: bool,
    at: Instant,
}

pub struct App {
    pub s: Session,
    pub my_username: String,
    pub teams: Vec<Team>,
    pub team: usize,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub views: HashMap<String, ChannelView>,
    pub input: LineEditor,
    /// Lines scrolled up from the bottom of the message view.
    pub scroll: usize,
    /// Set by the renderer: the largest useful `scroll` and the page height.
    pub max_scroll: usize,
    pub page: usize,
    pub switcher: Option<Switcher>,
    pub connection: Connection,
    pub flash: Option<Flash>,
    /// channel id -> user id -> last typing event
    pub typing: HashMap<String, HashMap<String, Instant>>,
    pub ws: Option<WebSocketHandle>,
    pub quit: bool,
    last_typing_sent: Option<Instant>,
    needs_view: bool,
    loading_older: bool,
}

impl App {
    pub async fn new(mut s: Session, team_flag: Option<&str>) -> Result<App> {
        let me = s.client.me().await?;
        let teams = s.teams().await?.to_vec();
        let team = match s.team(team_flag).await {
            Ok(t) => teams.iter().position(|x| x.id == t.id).unwrap_or(0),
            Err(_) => 0, // several teams and no default: start with the first
        };
        if teams.is_empty() {
            anyhow::bail!("you are not a member of any team");
        }
        let mut app = App {
            s,
            my_username: me.username,
            teams,
            team,
            entries: Vec::new(),
            selected: 0,
            views: HashMap::new(),
            input: LineEditor::default(),
            scroll: 0,
            max_scroll: 0,
            page: 20,
            switcher: None,
            connection: Connection::Connecting,
            flash: None,
            typing: HashMap::new(),
            ws: None,
            quit: false,
            last_typing_sent: None,
            needs_view: false,
            loading_older: false,
        };
        app.load_team().await?;
        let start = app
            .entries
            .iter()
            .position(|e| e.channel.name == "town-square")
            .unwrap_or(0);
        app.select(start).await;
        Ok(app)
    }

    pub fn team(&self) -> &Team {
        &self.teams[self.team]
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    pub fn current_id(&self) -> Option<String> {
        self.current().map(|e| e.channel.id.clone())
    }

    pub fn current_view(&self) -> Option<&ChannelView> {
        self.views.get(&self.current_id()?)
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.flash = Some(Flash {
            text: text.into(),
            error: false,
            at: Instant::now(),
        });
    }

    pub fn error(&mut self, e: impl std::fmt::Display) {
        self.flash = Some(Flash {
            text: e.to_string(),
            error: true,
            at: Instant::now(),
        });
    }

    /// Users typing in the current channel (not counting thread replies).
    pub fn typing_here(&self) -> Vec<String> {
        let Some(id) = self.current_id() else {
            return vec![];
        };
        let mut names: Vec<String> = self
            .typing
            .get(&id)
            .map(|m| m.keys().map(|u| self.s.username(u)).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    // ------------------------------------------------------------- loading

    /// Fetch channels and read state for the current team.
    pub async fn load_team(&mut self) -> Result<()> {
        let keep = self.current_id();
        let team_id = self.team().id.clone();
        let (channels, members) = tokio::try_join!(
            self.s.client.my_channels(&team_id),
            self.s.client.my_channel_members(&team_id)
        )?;
        let members: HashMap<_, _> = members
            .into_iter()
            .map(|m| (m.channel_id.clone(), m))
            .collect();
        let me = self.s.me.clone();
        self.s
            .load_users(channels.iter().filter_map(|c| c.dm_other_user(&me)))
            .await?;

        let mut entries = Vec::new();
        for c in channels {
            if c.delete_at > 0 {
                continue;
            }
            let label = match c.channel_type {
                ChannelType::Direct => format!(
                    "@{}",
                    self.s.username(&c.dm_other_user(&me).unwrap_or_default())
                ),
                ChannelType::Group => c.display_name.clone(),
                _ => c.display_name.clone(),
            };
            let (unread, muted) = match members.get(&c.id) {
                Some(m) => (Unread::compute(&c, m, false), m.is_muted()),
                None => (Unread::default(), false),
            };
            entries.push(Entry {
                channel: c,
                label,
                unread,
                muted,
            });
        }
        self.entries = entries;
        self.sort_entries(keep.as_deref());
        Ok(())
    }

    /// Channels alphabetically, then DMs by most recent activity.
    fn sort_entries(&mut self, keep: Option<&str>) {
        self.entries.sort_by(|a, b| {
            a.is_dm().cmp(&b.is_dm()).then_with(|| {
                if a.is_dm() {
                    b.channel.last_post_at.cmp(&a.channel.last_post_at)
                } else {
                    a.label.to_lowercase().cmp(&b.label.to_lowercase())
                }
            })
        });
        if let Some(id) = keep
            && let Some(i) = self.entries.iter().position(|e| e.channel.id == id)
        {
            self.selected = i;
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    async fn load_posts(&mut self, channel_id: &str) -> Result<()> {
        let list = self
            .s
            .client
            .posts(channel_id, &PostsQuery::latest(PAGE))
            .await?;
        let posts: Vec<Post> = list.oldest_first().into_iter().cloned().collect();
        self.s
            .load_users(posts.iter().map(|p| p.user_id.clone()))
            .await?;
        let has_more = posts.len() as u32 >= PAGE;
        self.views
            .insert(channel_id.to_string(), ChannelView { posts, has_more });
        Ok(())
    }

    /// Fetch the page of posts before the oldest one shown.
    pub async fn load_older(&mut self) {
        let Some(id) = self.current_id() else { return };
        let Some(oldest) = self.views.get(&id).and_then(|v| {
            v.has_more
                .then(|| v.posts.first().map(|p| p.id.clone()))
                .flatten()
        }) else {
            return;
        };
        if self.loading_older {
            return;
        }
        self.loading_older = true;
        let q = PostsQuery {
            before: Some(oldest),
            per_page: Some(PAGE),
            ..Default::default()
        };
        let r = self.s.client.posts(&id, &q).await;
        self.loading_older = false;
        match r {
            Ok(list) => {
                let older: Vec<Post> = list.oldest_first().into_iter().cloned().collect();
                if let Err(e) = self
                    .s
                    .load_users(older.iter().map(|p| p.user_id.clone()))
                    .await
                {
                    self.error(e);
                }
                let view = self.views.entry(id).or_default();
                view.has_more = older.len() as u32 >= PAGE;
                for p in older {
                    view.upsert(p);
                }
            }
            Err(e) => self.error(e),
        }
    }

    /// Open the channel at `index`: load history if needed and mark it read.
    pub async fn select(&mut self, index: usize) {
        if index >= self.entries.len() {
            return;
        }
        self.selected = index;
        self.scroll = 0;
        let id = self.entries[index].channel.id.clone();
        if !self.views.contains_key(&id)
            && let Err(e) = self.load_posts(&id).await
        {
            self.error(e);
            return;
        }
        self.mark_read(&id).await;
    }

    async fn mark_read(&mut self, channel_id: &str) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.channel.id == channel_id) {
            if !e.unread.is_unread() && !self.needs_view {
                return;
            }
            e.unread = Unread::default();
        }
        self.needs_view = false;
        if let Err(e) = self.s.client.view_channel(channel_id).await {
            self.error(e);
        }
    }

    async fn switch_team(&mut self, delta: isize) {
        if self.teams.len() < 2 {
            self.info("You are only in one team.");
            return;
        }
        let n = self.teams.len() as isize;
        self.team = ((self.team as isize + delta).rem_euclid(n)) as usize;
        self.selected = 0;
        if let Err(e) = self.load_team().await {
            self.error(e);
            return;
        }
        let start = self
            .entries
            .iter()
            .position(|e| e.channel.name == "town-square")
            .unwrap_or(0);
        self.select(start).await;
        let name = self.team().display_name.clone();
        self.info(format!("Team: {name}"));
    }

    // ----------------------------------------------------------- sending

    async fn send(&mut self) {
        let text = self.input.text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let message = if let Some(rest) = trimmed.strip_prefix("//") {
            // `//text` sends `/text` literally.
            format!("/{rest}")
        } else if let Some(cmd) = trimmed.strip_prefix('/') {
            let word = cmd
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            match word.as_str() {
                "quit" | "exit" | "q" => {
                    self.input.clear();
                    self.quit = true;
                }
                "team" => {
                    self.input.clear();
                    self.switch_team(1).await;
                }
                _ => self.error(format!(
                    "/{word}: slash commands aren't supported yet (start with // to send text beginning with /)"
                )),
            }
            return;
        } else {
            text.trim_end().to_string()
        };
        let Some(id) = self.current_id() else { return };
        match self
            .s
            .client
            .create_post(&CreatePost::new(&id, message))
            .await
        {
            Ok(post) => {
                self.input.clear();
                self.scroll = 0;
                self.views.entry(id).or_default().upsert(post);
            }
            Err(e) => self.error(format!("Not sent: {e}")),
        }
    }

    fn typed(&mut self) {
        let due = self
            .last_typing_sent
            .is_none_or(|t| t.elapsed() >= TYPING_SEND_EVERY);
        if due && let (Some(ws), Some(id)) = (&self.ws, self.current_id()) {
            ws.send_typing(&id, None);
            self.last_typing_sent = Some(Instant::now());
        }
    }

    // -------------------------------------------------------------- keys

    pub async fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
            self.quit = true;
            return;
        }
        if self.switcher.is_some() {
            self.on_switcher_key(key).await;
            return;
        }

        match key.code {
            KeyCode::Char('k') if ctrl => self.switcher = Some(Switcher::default()),
            KeyCode::Char('n') if ctrl => {
                self.select((self.selected + 1) % self.entries.len().max(1))
                    .await
            }
            KeyCode::Char('p') if ctrl => {
                let n = self.entries.len().max(1);
                self.select((self.selected + n - 1) % n).await
            }
            KeyCode::Char('t') if ctrl => self.switch_team(1).await,
            KeyCode::Tab => self.next_unread().await,
            KeyCode::Up if alt => {
                let n = self.entries.len().max(1);
                self.select((self.selected + n - 1) % n).await
            }
            KeyCode::Down if alt => {
                self.select((self.selected + 1) % self.entries.len().max(1))
                    .await
            }

            KeyCode::Up => self.scroll_by(1).await,
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageUp => self.scroll_by(self.page.saturating_sub(2).max(1)).await,
            KeyCode::PageDown => {
                self.scroll = self
                    .scroll
                    .saturating_sub(self.page.saturating_sub(2).max(1))
            }
            KeyCode::Esc => {
                self.scroll = 0;
                self.flash = None;
            }

            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.input.insert('\n')
            }
            KeyCode::Enter => self.send().await,
            KeyCode::Char('j') if ctrl => self.input.insert('\n'),
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('u') if ctrl => self.input.kill_to_start(),
            KeyCode::Char('w') if ctrl => self.input.delete_word(),
            KeyCode::Char('b') if alt => self.input.word_left(),
            KeyCode::Char('f') if alt => self.input.word_right(),
            KeyCode::Left if ctrl || alt => self.input.word_left(),
            KeyCode::Right if ctrl || alt => self.input.word_right(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Backspace if alt || ctrl => self.input.delete_word(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Char(c) if !ctrl => {
                self.input.insert(c);
                self.typed();
            }
            _ => {}
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        if let Some(sw) = &mut self.switcher {
            sw.query.insert_str(&text.replace('\n', " "));
        } else {
            self.input.insert_str(text);
        }
    }

    async fn scroll_by(&mut self, n: usize) {
        if self.scroll + n > self.max_scroll {
            self.scroll = self.max_scroll;
            let more = self.current_view().is_some_and(|v| v.has_more);
            if more {
                self.load_older().await;
            }
        } else {
            self.scroll += n;
        }
    }

    async fn next_unread(&mut self) {
        let n = self.entries.len();
        let order = (1..=n).map(|d| (self.selected + d) % n);
        // Mentions first, then any unread.
        let candidates: Vec<usize> = order.collect();
        let pick = candidates
            .iter()
            .find(|&&i| self.entries[i].unread.mentions > 0)
            .or_else(|| {
                candidates
                    .iter()
                    .find(|&&i| self.entries[i].unread.messages > 0 && !self.entries[i].muted)
            })
            .copied();
        match pick {
            Some(i) => self.select(i).await,
            None => self.info("No unread channels."),
        }
    }

    /// Indices into `entries` matching the switcher query, best first.
    pub fn switcher_matches(&self) -> Vec<usize> {
        let q = self
            .switcher
            .as_ref()
            .map(|s| s.query.text().to_lowercase())
            .unwrap_or_default();
        let q = q.trim().trim_start_matches(['#', '~']);
        let mut scored: Vec<(u8, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let label = e.label.to_lowercase();
                let name = e.channel.name.to_lowercase();
                let score = if q.is_empty() {
                    if e.unread.is_unread() { 0 } else { 1 }
                } else if label.starts_with(q)
                    || name.starts_with(q)
                    || label.trim_start_matches('@').starts_with(q)
                {
                    0
                } else if label.contains(q) || name.contains(q) {
                    1
                } else if is_subsequence(q, &label) {
                    2
                } else {
                    return None;
                };
                Some((score, i))
            })
            .collect();
        scored.sort_by_key(|&(score, i)| (score, i));
        scored.into_iter().map(|(_, i)| i).collect()
    }

    async fn on_switcher_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let matches = self.switcher_matches();
        let count = matches.len();
        let Some(sw) = &mut self.switcher else { return };
        match key.code {
            KeyCode::Esc => self.switcher = None,
            KeyCode::Char('k') if ctrl => self.switcher = None,
            KeyCode::Enter => {
                let pick = matches.get(sw.selected).copied();
                self.switcher = None;
                if let Some(i) = pick {
                    self.select(i).await;
                }
            }
            KeyCode::Up => sw.selected = sw.selected.saturating_sub(1),
            KeyCode::Char('p') if ctrl => sw.selected = sw.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                sw.selected = (sw.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char('n') if ctrl => {
                sw.selected = (sw.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Backspace => {
                sw.query.backspace();
                sw.selected = 0;
            }
            KeyCode::Char('u') if ctrl => {
                sw.query.clear();
                sw.selected = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                sw.query.insert(c);
                sw.selected = 0;
            }
            _ => {}
        }
    }

    // ------------------------------------------------------ server events

    pub async fn on_ws(&mut self, event: WsEvent) {
        match event {
            WsEvent::Connected { resumed, .. } => {
                let was_down = !matches!(self.connection, Connection::Connecting);
                self.connection = Connection::Connected;
                if was_down && !resumed {
                    self.resync().await;
                }
            }
            WsEvent::Disconnected { reason, retry_in } => {
                self.connection = Connection::Reconnecting { reason, retry_in };
            }
            WsEvent::MissedEvents => self.resync().await,
            WsEvent::AuthFailed(why) => {
                self.connection = Connection::Lost(format!(
                    "session rejected ({why}); quit and run `rustmost login`"
                ));
            }
            WsEvent::Posted(ev) => {
                let post = ev.post;
                let cid = post.channel_id.clone();
                if let Err(e) = self.s.load_users([post.user_id.clone()]).await {
                    self.error(e);
                }
                if let Some(t) = self.typing.get_mut(&cid) {
                    t.remove(&post.user_id);
                }
                let mine = post.user_id == self.s.me;
                let mentioned = ev.mentions.iter().any(|m| m == &self.s.me);
                let current = self.current_id();
                let team_id = self.team().id.clone();
                if let Some(view) = self.views.get_mut(&cid) {
                    view.upsert(post.clone());
                }
                match self.entries.iter_mut().find(|e| e.channel.id == cid) {
                    Some(entry) => {
                        entry.channel.last_post_at = post.create_at;
                        if current.as_deref() == Some(cid.as_str()) {
                            if !mine {
                                self.needs_view = true;
                            }
                        } else if !mine {
                            entry.unread.messages += 1;
                            if mentioned || entry.channel.channel_type == ChannelType::Direct {
                                entry.unread.mentions += 1;
                            }
                        }
                        if entry.is_dm() {
                            self.sort_entries(current.as_deref());
                        }
                    }
                    // A new DM, or a channel we were just added to.
                    None if ev.team_id.is_empty() || ev.team_id == team_id => {
                        if let Err(e) = self.load_team().await {
                            self.error(e);
                        }
                    }
                    None => {}
                }
            }
            WsEvent::PostEdited(post) => {
                if let Some(view) = self.views.get_mut(&post.channel_id)
                    && view.posts.iter().any(|p| p.id == post.id)
                {
                    view.upsert(*post);
                }
            }
            WsEvent::PostDeleted(post) => {
                if let Some(view) = self.views.get_mut(&post.channel_id) {
                    view.posts.retain(|p| p.id != post.id);
                }
            }
            WsEvent::Typing {
                channel_id,
                user_id,
                parent_id,
            } => {
                if parent_id.is_empty() && user_id != self.s.me {
                    if let Err(e) = self.s.load_users([user_id.clone()]).await {
                        self.error(e);
                    }
                    self.typing
                        .entry(channel_id)
                        .or_default()
                        .insert(user_id, Instant::now());
                }
            }
            WsEvent::ChannelsViewed { channel_ids } => {
                for e in self
                    .entries
                    .iter_mut()
                    .filter(|e| channel_ids.contains(&e.channel.id))
                {
                    e.unread = Unread::default();
                }
            }
            WsEvent::ReactionAdded(r) => {
                self.update_reactions(&r.post_id.clone(), |list| list.push(r))
            }
            WsEvent::ReactionRemoved(r) => self.update_reactions(&r.post_id.clone(), |list| {
                list.retain(|x| !(x.user_id == r.user_id && x.emoji_name == r.emoji_name))
            }),
            _ => {}
        }
    }

    fn update_reactions(
        &mut self,
        post_id: &str,
        f: impl FnOnce(&mut Vec<rustmost_api::Reaction>),
    ) {
        for view in self.views.values_mut() {
            if let Some(p) = view.posts.iter_mut().find(|p| p.id == post_id) {
                let meta = p.metadata.get_or_insert_with(Default::default);
                f(&mut meta.reactions);
                return;
            }
        }
    }

    /// After a reconnect without replay: refetch what we show.
    async fn resync(&mut self) {
        self.views.clear();
        if let Err(e) = self.load_team().await {
            self.error(e);
            return;
        }
        let i = self.selected;
        let scroll = self.scroll;
        self.select(i).await;
        self.scroll = scroll;
    }

    /// Called a few times a second.
    pub async fn tick(&mut self) {
        for users in self.typing.values_mut() {
            users.retain(|_, t| t.elapsed() < TYPING_SHOWN_FOR);
        }
        if self
            .flash
            .as_ref()
            .is_some_and(|f| f.at.elapsed() > FLASH_FOR)
        {
            self.flash = None;
        }
        if self.needs_view {
            // Only mark read when the newest message is actually on screen.
            if self.scroll == 0
                && let Some(id) = self.current_id()
            {
                self.mark_read(&id).await;
            }
        }
    }
}

fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}
