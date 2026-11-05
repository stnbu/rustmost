//! A logged-in session: the API client plus lookups the commands share
//! (teams, channels by name, usernames).

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use rustmost_api::{Channel, ChannelType, Client, OptionalExt, Team, User};

pub struct Session {
    pub client: Client,
    pub me: String,
    pub default_team: Option<String>,
    teams: Option<Vec<Team>>,
    users: HashMap<String, User>,
}

fn looks_like_id(s: &str) -> bool {
    s.len() == 26
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

impl Session {
    pub fn new(client: Client, me: String, default_team: Option<String>) -> Self {
        Session {
            client,
            me,
            default_team,
            teams: None,
            users: HashMap::new(),
        }
    }

    pub async fn teams(&mut self) -> Result<&[Team]> {
        if self.teams.is_none() {
            let mut teams = self.client.my_teams().await.context("listing teams")?;
            teams.sort_by_key(|t| t.display_name.to_lowercase());
            self.teams = Some(teams);
        }
        Ok(self.teams.as_deref().unwrap_or_default())
    }

    /// Find a team by name, display name or id.
    pub async fn find_team(&mut self, spec: &str) -> Result<Team> {
        let teams = self.teams().await?;
        let lower = spec.to_lowercase();
        teams
            .iter()
            .find(|t| t.id == spec || t.name == lower)
            .or_else(|| {
                teams
                    .iter()
                    .find(|t| t.display_name.to_lowercase() == lower)
            })
            .cloned()
            .ok_or_else(|| {
                let names: Vec<_> = teams.iter().map(|t| t.name.as_str()).collect();
                anyhow!(
                    "no team matching '{spec}'. Your teams: {}",
                    names.join(", ")
                )
            })
    }

    /// The team to use: `--team`, else the saved default, else your only team.
    pub async fn team(&mut self, flag: Option<&str>) -> Result<Team> {
        if let Some(spec) = flag
            .map(str::to_string)
            .or_else(|| self.default_team.clone())
        {
            return self.find_team(&spec).await;
        }
        let teams = self.teams().await?;
        match teams {
            [] => bail!("you are not a member of any team"),
            [only] => Ok(only.clone()),
            many => {
                let names: Vec<_> = many.iter().map(|t| t.name.as_str()).collect();
                bail!(
                    "you are in several teams ({}); pass --team or run `rustmost use <team>`",
                    names.join(", ")
                )
            }
        }
    }

    /// Resolve what someone types to a channel:
    /// `town-square`, `Town Square`, `team/channel`, `@username` (DM) or a channel id.
    pub async fn find_channel(&mut self, spec: &str, team_flag: Option<&str>) -> Result<Channel> {
        let spec = spec.trim().trim_start_matches('~');
        if let Some(username) = spec.strip_prefix('@') {
            let user = self
                .client
                .user_by_username(username)
                .await
                .optional()?
                .ok_or_else(|| anyhow!("no user named @{username}"))?;
            let me = self.me.clone();
            return Ok(self.client.direct_channel(&me, &user.id).await?);
        }
        if looks_like_id(spec)
            && let Some(c) = self.client.channel(spec).await.optional()?
        {
            return Ok(c);
        }
        let (team_spec, name) = match spec.split_once('/') {
            Some((t, c)) => (Some(t), c),
            None => (team_flag, spec),
        };
        let team = self.team(team_spec).await?;
        let channels = self.client.my_channels(&team.id).await?;
        let lower = name.to_lowercase();
        if let Some(c) = channels.iter().find(|c| c.name == lower).or_else(|| {
            channels
                .iter()
                .find(|c| c.display_name.to_lowercase() == lower)
        }) {
            return Ok(c.clone());
        }
        // A public channel you haven't joined yet.
        if let Some(c) = self
            .client
            .channel_by_name(&team.id, &lower)
            .await
            .optional()?
        {
            return Ok(c);
        }
        // Maybe they meant a username without the @.
        if let Some(user) = self.client.user_by_username(&lower).await.optional()? {
            let me = self.me.clone();
            return Ok(self.client.direct_channel(&me, &user.id).await?);
        }
        bail!("no channel '{name}' in team {}", team.name)
    }

    /// Make sure these users are cached (one request for all missing ones).
    pub async fn load_users<I: IntoIterator<Item = String>>(&mut self, ids: I) -> Result<()> {
        let mut missing: Vec<String> = ids
            .into_iter()
            .filter(|id| !id.is_empty() && !self.users.contains_key(id))
            .collect();
        missing.sort();
        missing.dedup();
        if missing.is_empty() {
            return Ok(());
        }
        for u in self.client.users_by_ids(&missing).await? {
            self.users.insert(u.id.clone(), u);
        }
        Ok(())
    }

    pub fn username(&self, id: &str) -> String {
        self.users
            .get(id)
            .map(|u| u.username.clone())
            .unwrap_or_else(|| id.chars().take(8).collect())
    }

    /// What to call a channel in lists: `town-square`, `@bob`, `@bob,@carol`.
    pub async fn channel_label(&mut self, c: &Channel) -> Result<String> {
        Ok(match c.channel_type {
            ChannelType::Direct => {
                let other = c.dm_other_user(&self.me).unwrap_or_default();
                self.load_users([other.clone()]).await?;
                format!("@{}", self.username(&other))
            }
            ChannelType::Group => c
                .display_name
                .split(", ")
                .map(|n| format!("@{n}"))
                .collect::<Vec<_>>()
                .join(","),
            _ => c.name.clone(),
        })
    }
}
