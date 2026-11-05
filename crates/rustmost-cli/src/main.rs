mod config;
mod format;
mod session;

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use rustmost_api::{ChannelType, Client, CreatePost, PostsQuery, Unread, WsEvent};

use config::Config;
use format::Style;
use session::Session;

/// A Mattermost client for the terminal.
#[derive(Parser)]
#[command(name = "rustmost", version, about)]
struct Cli {
    /// Config file (default: your OS config folder, rustmost/config.toml).
    #[arg(long, global = true, env = "RUSTMOST_CONFIG")]
    config: Option<PathBuf>,

    /// Server URL, overriding the saved one.
    #[arg(long, global = true, env = "RUSTMOST_SERVER")]
    server: Option<String>,

    /// Session or personal access token, overriding the saved one.
    #[arg(long, global = true, env = "RUSTMOST_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Team to use (name or display name).
    #[arg(long, short = 't', global = true)]
    team: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Log in with your username (or email) and password.
    Login {
        /// Username or email. Prompted for if omitted.
        #[arg(long, short)]
        username: Option<String>,
    },
    /// Log out and forget the saved session.
    Logout,
    /// Show who you are logged in as, and the server version.
    Whoami,
    /// List your teams.
    Teams,
    /// Set the default team for other commands.
    Use { team: String },
    /// List your channels in a team, with unread counts.
    Channels {
        /// Only channels with unread messages or mentions.
        #[arg(long, short)]
        unread: bool,
    },
    /// Show recent messages in a channel (`town-square`, `team/channel`, `@user`).
    Read {
        channel: String,
        /// How many messages.
        #[arg(long, short = 'n', default_value_t = 30)]
        count: u32,
        /// Also mark the channel as read.
        #[arg(long, short = 'm')]
        mark_read: bool,
        /// Show post ids (to reply, react or open a thread).
        #[arg(long)]
        ids: bool,
    },
    /// Show a thread: a post and all its replies.
    Thread {
        post_id: String,
        #[arg(long)]
        ids: bool,
    },
    /// Send a message. Reads the message from stdin if none is given.
    Send {
        channel: String,
        message: Vec<String>,
        /// Reply in the thread of this post id.
        #[arg(long, short)]
        reply: Option<String>,
        /// Attach a file (repeatable).
        #[arg(long, short)]
        file: Vec<PathBuf>,
    },
    /// Follow new messages live. With no channels, follows everything.
    Tail {
        channels: Vec<String>,
        #[arg(long)]
        ids: bool,
    },
    /// Search messages (Mattermost search syntax, e.g. `from:alice deploy`).
    Search {
        terms: Vec<String>,
        #[arg(long)]
        ids: bool,
    },
    /// Add a reaction to a post (`+1`, `tada`, ...).
    React { post_id: String, emoji: String },
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("rustmost: {e:#}");
        let unauthorized = e.chain().any(|c| {
            c.downcast_ref::<rustmost_api::Error>().is_some_and(|e| {
                e.is_unauthorized() || matches!(e, rustmost_api::Error::NotAuthenticated)
            })
        });
        if unauthorized {
            eprintln!("Your session may have expired. Run `rustmost login`.");
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.config.clone().unwrap_or_else(config::default_path);
    let mut cfg = Config::load(&path)?;

    match &cli.command {
        Command::Login { username } => return login(&cli, &mut cfg, &path, username.clone()).await,
        Command::Use { team } => {
            let mut s = session(&cli, &cfg).await?;
            let t = s.find_team(team).await?;
            cfg.default_team = Some(t.name.clone());
            cfg.save(&path)?;
            println!("Default team is now {} ({}).", t.display_name, t.name);
            return Ok(());
        }
        Command::Logout => {
            if let Ok(mut c) = client(&cli, &cfg)
                && c.token().is_some()
            {
                let _ = c.logout().await;
            }
            cfg.token = None;
            cfg.save(&path)?;
            println!("Logged out.");
            return Ok(());
        }
        _ => {}
    }

    let mut s = session(&cli, &cfg).await?;
    let team_flag = cli.team.as_deref();

    match cli.command {
        Command::Login { .. } | Command::Logout | Command::Use { .. } => unreachable!(),

        Command::Whoami => {
            let me = s.client.me().await?;
            let version = s.client.ping().await?.unwrap_or_else(|| "unknown".into());
            println!(
                "{} (@{}) on {} — server {version}",
                me.display_name(),
                me.username,
                s.client.server_url()
            );
        }

        Command::Teams => {
            let default = s.default_team.clone();
            for t in s.teams().await? {
                let mark = if default.as_deref() == Some(t.name.as_str()) {
                    " (default)"
                } else {
                    ""
                };
                println!("{:<24} {}{mark}", t.name, t.display_name);
            }
        }

        Command::Channels { unread } => {
            let style = Style::detect(false);
            let team = s.team(team_flag).await?;
            let (channels, members) = tokio::try_join!(
                s.client.my_channels(&team.id),
                s.client.my_channel_members(&team.id)
            )?;
            let members: HashMap<_, _> = members
                .into_iter()
                .map(|m| (m.channel_id.clone(), m))
                .collect();
            let me = s.me.clone();
            s.load_users(channels.iter().filter_map(|c| c.dm_other_user(&me)))
                .await?;

            let mut rows = Vec::new();
            for c in &channels {
                if c.delete_at > 0 {
                    continue;
                }
                let u = members
                    .get(&c.id)
                    .map(|m| Unread::compute(c, m, false))
                    .unwrap_or_default();
                if unread && !u.is_unread() {
                    continue;
                }
                let label = s.channel_label(c).await?;
                rows.push((
                    c.channel_type.is_dm_or_group(),
                    c.last_post_at,
                    label,
                    c.display_name.clone(),
                    u,
                ));
            }
            // Channels alphabetically, then DMs by most recent activity.
            rows.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| if a.0 { b.1.cmp(&a.1) } else { a.2.cmp(&b.2) })
            });
            for (_, _, label, display, u) in rows {
                let counts = match (u.messages, u.mentions) {
                    (0, 0) => String::new(),
                    (n, 0) => format!("{n} new"),
                    (n, m) => format!("{n} new, {m} mention{}", if m == 1 { "" } else { "s" }),
                };
                let name = if u.mentions > 0 {
                    style.bold(&label)
                } else {
                    label.clone()
                };
                let pad = 28usize.saturating_sub(label.chars().count());
                let display = if display == label || display.is_empty() || label.starts_with('@') {
                    String::new()
                } else {
                    display
                };
                println!(
                    "{name}{} {:<30} {}",
                    " ".repeat(pad),
                    display,
                    style.dim(&counts)
                );
            }
        }

        Command::Read {
            channel,
            count,
            mark_read,
            ids,
        } => {
            let style = Style::detect(ids);
            let ch = s.find_channel(&channel, team_flag).await?;
            let list = s
                .client
                .posts(&ch.id, &PostsQuery::latest(count.clamp(1, 200)))
                .await?;
            let posts = list.oldest_first();
            s.load_users(posts.iter().map(|p| p.user_id.clone()))
                .await?;
            if posts.is_empty() {
                println!("{}", style.dim("(no messages)"));
            }
            for p in posts {
                let author = author(&s, p);
                println!("{}", format::post(&style, p, &author, None));
            }
            if mark_read {
                s.client.view_channel(&ch.id).await?;
            }
        }

        Command::Thread { post_id, ids } => {
            let style = Style::detect(ids);
            let list = s.client.thread(&post_id).await?;
            let posts = list.chronological();
            s.load_users(posts.iter().map(|p| p.user_id.clone()))
                .await?;
            for p in posts {
                let author = author(&s, p);
                println!("{}", format::post(&style, p, &author, None));
            }
        }

        Command::Send {
            channel,
            message,
            reply,
            file,
        } => {
            let mut text = message.join(" ");
            if text.is_empty() && file.is_empty() {
                if std::io::stdin().is_terminal() {
                    bail!("no message given (pass it as an argument or pipe it on stdin)");
                }
                std::io::stdin().read_to_string(&mut text)?;
                text = text.trim_end().to_string();
            }
            let ch = s.find_channel(&channel, team_flag).await?;
            let mut post = CreatePost::new(&ch.id, text);
            if let Some(root) = reply {
                // Replies must point at the thread's root post.
                let target = s.client.post_by_id(&root).await?;
                post = post.reply_to(if target.root_id.is_empty() {
                    target.id
                } else {
                    target.root_id
                });
            }
            for f in &file {
                let bytes = std::fs::read(f).with_context(|| format!("reading {}", f.display()))?;
                let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                post.file_ids
                    .push(s.client.upload_file(&ch.id, name, bytes).await?.id);
            }
            if post.message.is_empty() && post.file_ids.is_empty() {
                bail!("message is empty");
            }
            let created = s.client.create_post(&post).await?;
            eprintln!("Sent ({}).", created.id);
        }

        Command::Tail { channels, ids } => tail(&mut s, channels, ids, team_flag).await?,

        Command::Search { terms, ids } => {
            let style = Style::detect(ids);
            let team = s.team(team_flag).await?;
            let list = s.client.search_posts(&team.id, &terms.join(" ")).await?;
            let posts = list.oldest_first();
            s.load_users(posts.iter().map(|p| p.user_id.clone()))
                .await?;
            let mut labels: HashMap<String, String> = HashMap::new();
            for p in &posts {
                if !labels.contains_key(&p.channel_id) {
                    let c = s.client.channel(&p.channel_id).await?;
                    labels.insert(p.channel_id.clone(), s.channel_label(&c).await?);
                }
            }
            if posts.is_empty() {
                println!("{}", style.dim("(no results)"));
            }
            for p in posts {
                let author = author(&s, p);
                println!(
                    "{}",
                    format::post(
                        &style,
                        p,
                        &author,
                        Some(&format!("~{}", labels[&p.channel_id]))
                    )
                );
            }
        }

        Command::React { post_id, emoji } => {
            let me = s.me.clone();
            s.client.add_reaction(&me, &post_id, &emoji).await?;
            eprintln!("Reacted :{}:", emoji.trim_matches(':'));
        }
    }
    Ok(())
}

fn author(s: &Session, p: &rustmost_api::Post) -> String {
    p.override_username()
        .map(str::to_string)
        .unwrap_or_else(|| s.username(&p.user_id))
}

fn client(cli: &Cli, cfg: &Config) -> Result<Client> {
    let server = cli
        .server
        .clone()
        .or_else(|| cfg.server.clone())
        .ok_or_else(|| anyhow!("no server set; run `rustmost login`"))?;
    let mut c = Client::new(&server)?;
    if let Some(t) = cli.token.clone().or_else(|| cfg.token.clone()) {
        c = c.with_token(t);
    }
    Ok(c)
}

async fn session(cli: &Cli, cfg: &Config) -> Result<Session> {
    let c = client(cli, cfg)?;
    if c.token().is_none() {
        bail!("not logged in; run `rustmost login`");
    }
    // Saved user id avoids a round trip; with an overriding token, ask the server.
    let me = match (&cli.token, &cfg.user_id) {
        (None, Some(id)) => id.clone(),
        _ => c.me().await?.id,
    };
    Ok(Session::new(c, me, cfg.default_team.clone()))
}

fn prompt(label: &str) -> Result<String> {
    eprint!("{label}");
    std::io::stderr().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

async fn login(
    cli: &Cli,
    cfg: &mut Config,
    path: &std::path::Path,
    username: Option<String>,
) -> Result<()> {
    let server = match cli.server.clone().or_else(|| cfg.server.clone()) {
        Some(s) => s,
        None => prompt("Server (e.g. chat.example.com): ")?,
    };
    let mut client = Client::new(&server)?;
    let version = client
        .ping()
        .await
        .with_context(|| format!("could not reach {}", client.server_url()))?;

    let username = match username.or_else(|| cfg.username.clone()) {
        Some(u) => u,
        None => prompt("Username or email: ")?,
    };
    let password = match std::env::var("RUSTMOST_PASSWORD") {
        Ok(p) => p,
        Err(_) => rpassword::prompt_password("Password: ")?,
    };

    let (user, token) = match client.login(&username, &password, None).await {
        Ok(r) => r,
        Err(e) if e.is_mfa_error() => {
            let code = prompt("MFA code: ")?;
            client.login(&username, &password, Some(&code)).await?
        }
        Err(e) => return Err(e.into()),
    };

    let server_changed = cfg.server.as_deref() != Some(client.server_url().as_str());
    cfg.server = Some(client.server_url().to_string());
    cfg.token = Some(token);
    cfg.user_id = Some(user.id.clone());
    cfg.username = Some(user.username.clone());
    if server_changed {
        cfg.default_team = None;
    }
    cfg.save(path)?;
    println!(
        "Logged in as {} (@{}) on {} (server {}).",
        user.display_name(),
        user.username,
        client.server_url(),
        version.unwrap_or_else(|| "unknown".into())
    );
    println!("Session saved to {}", path.display());
    Ok(())
}

async fn tail(
    s: &mut Session,
    specs: Vec<String>,
    ids: bool,
    team_flag: Option<&str>,
) -> Result<()> {
    let style = Style::detect(ids);
    let mut filter: HashSet<String> = HashSet::new();
    for spec in &specs {
        filter.insert(s.find_channel(spec, team_flag).await?.id);
    }
    let mut ws = s.client.websocket().await?;
    eprintln!(
        "{}",
        style.dim(&format!(
            "Following {}. Ctrl-C to stop.",
            if specs.is_empty() {
                "all channels".to_string()
            } else {
                specs.join(", ")
            }
        ))
    );

    loop {
        let event = tokio::select! {
            e = ws.next() => e,
            _ = tokio::signal::ctrl_c() => break,
        };
        let Some(event) = event else { break };
        match event {
            WsEvent::Posted(p) => {
                if !filter.is_empty() && !filter.contains(&p.post.channel_id) {
                    continue;
                }
                s.load_users([p.post.user_id.clone()]).await.ok();
                let author = author(s, &p.post);
                let label = match p.channel_type {
                    Some(ChannelType::Direct) => "DM".to_string(),
                    _ if !p.channel_name.is_empty() => format!("~{}", p.channel_name),
                    _ => format!("~{}", p.channel_display_name),
                };
                let mentioned = p.mentions.iter().any(|m| m == &s.me);
                let line = format::post(&style, &p.post, &author, Some(&label));
                println!(
                    "{}",
                    if mentioned {
                        format!("{line} {}", style.bold("◀ mention"))
                    } else {
                        line
                    }
                );
            }
            WsEvent::PostEdited(p) if filter.is_empty() || filter.contains(&p.channel_id) => {
                s.load_users([p.user_id.clone()]).await.ok();
                let author = author(s, &p);
                println!("{}", format::post(&style, &p, &author, Some("edit")));
            }
            WsEvent::Connected { resumed: false, .. } => {}
            WsEvent::Disconnected { reason, retry_in } => {
                eprintln!(
                    "{}",
                    style.dim(&format!(
                        "Disconnected ({reason}); retrying in {}s…",
                        retry_in.as_secs()
                    ))
                );
            }
            WsEvent::Connected { resumed: true, .. } => eprintln!("{}", style.dim("Reconnected.")),
            WsEvent::MissedEvents => {
                eprintln!("{}", style.dim("(some events may have been missed)"))
            }
            WsEvent::AuthFailed(why) => {
                bail!("the server rejected the session: {why}. Run `rustmost login`.")
            }
            _ => {}
        }
    }
    Ok(())
}
