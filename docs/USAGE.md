# Using rustmost

A [Mattermost](https://mattermost.com) client written entirely in Rust.

The project is split into layers:

| Crate | What it is |
|---|---|
| [`rustmost-api`](crates/rustmost-api) | An async library for the Mattermost v4 REST API and WebSocket event stream. |
| [`rustmost`](crates/rustmost-cli) | A command-line client built on the library. |
| [`fake-mattermost`](crates/fake-mattermost) | An in-memory fake server used by the tests (and handy for trying things offline). |

It has been built against Mattermost server 11.7.

## Install

You need a recent stable Rust. On macOS, install it with rustup:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then, from this folder:

```sh
cargo install --path crates/rustmost-cli    # puts `rustmost` in ~/.cargo/bin
```

or just `cargo run -p rustmost -- <command>` while developing.

## The full-screen interface

```sh
cargo run -p rustmost              # or `rustmost` once installed; `rustmost tui` also works
cargo run -p rustmost -- -t myteam # start in a particular team
```

Log in first with `rustmost login` (see below); the interface uses the saved session.

| Key | Does |
|---|---|
| Enter | Send the message |
| Alt-Enter (or Ctrl-J) | New line in the message |
| Ctrl-K | Switch channel: type part of a name, Enter to open, Esc to cancel |
| Tab | Jump to the next channel with mentions, then unread messages |
| Ctrl-N / Ctrl-P (or Alt-↓ / Alt-↑) | Next / previous channel |
| ↑ / ↓, PgUp / PgDn | Scroll messages; scrolling past the top loads older history |
| Esc | Jump back to the newest messages |
| Ctrl-T or `/team` | Next team (if you're in several) |
| Ctrl-C, Ctrl-Q or `/quit` | Quit |
| Ctrl-A / Ctrl-E, Ctrl-U, Ctrl-W | Start / end of line, delete to start, delete word |

In the sidebar, bold channels have unread messages and a red number counts
mentions (every new DM counts as one). Muted channels are dimmed. Opening a
channel marks it read, as in the web app. Messages that mention you are shown
in yellow, and "… is typing" appears above the message box.

Slash commands aren't supported yet; to send text that starts with `/`,
begin it with `//`. Pasting multi-line text works. On macOS Terminal, Alt
combinations need "Use Option as Meta key" turned on in the profile's Keyboard
settings; Ctrl-J and Ctrl-N/Ctrl-P work without it.

Not in this first version: threads view and replying from the interface (use
`rustmost thread` and `send --reply`), file attachments, editing or deleting
your messages, reactions, rendering Markdown, and notifications.

## Using the CLI

```sh
rustmost --server chat.example.com login     # asks for username, password (and MFA code if needed)
rustmost whoami
rustmost teams
rustmost use my-team                         # only needed if you're in more than one team

rustmost channels                            # with unread / mention counts
rustmost channels --unread
rustmost read town-square                    # last 30 messages
rustmost read town-square -n 100 --mark-read
rustmost read @bob                           # a DM
rustmost read other-team/off-topic

rustmost send town-square "Hello from Rust"
printf "line one\nline two\n" | rustmost send town-square
rustmost send town-square --file report.pdf "see attached"

rustmost read town-square --ids              # show post ids
rustmost send town-square --reply <post-id> "a reply in the thread"
rustmost thread <post-id>
rustmost react <post-id> +1

rustmost search "from:bob deploy"
rustmost tail                                # live messages from everywhere; Ctrl-C to stop
rustmost tail town-square @bob               # just these

rustmost logout
```

Channels can be given by URL name (`town-square`), display name (`"Town Square"`),
`team/channel`, `@username` for DMs, or channel id.

The session token is saved in `~/Library/Application Support/rustmost/config.toml`
on macOS (`~/.config/rustmost/config.toml` on Linux), readable only by you.
`rustmost logout` ends the session on the server and removes the token.
You can override settings with `RUSTMOST_SERVER`, `RUSTMOST_TOKEN` (e.g. a personal
access token) and `RUSTMOST_CONFIG`.

## Using the library

```rust
use rustmost_api::{Client, CreatePost, PostsQuery, WsEvent};

let mut client = Client::new("https://chat.example.com")?;
client.login("alice", "password", None).await?;   // Some("123456") for MFA

let team = &client.my_teams().await?[0];
let town = client.channel_by_name(&team.id, "town-square").await?;
for post in client.posts(&town.id, &PostsQuery::latest(20)).await?.oldest_first() {
    println!("{}", post.message);
}
client.create_post(&CreatePost::new(&town.id, "hi")).await?;

let mut ws = client.websocket().await?;      // reconnects on its own
while let Some(event) = ws.next().await {
    if let WsEvent::Posted(p) = event {
        println!("{}: {}", p.sender_name, p.post.message);
    }
}
```

Covered so far: login/logout (username or email, password, optional MFA) and
token auth; users and statuses; teams; channels, memberships, unread counts and
marking read; DMs; posts (read with paging, create, reply, edit, delete,
search, pinned); threads (and followed threads); reactions; file upload and
download; and the WebSocket stream with typed events (posted, edited, deleted,
typing, channels viewed, status, reactions), keepalive pings, reconnect with
backoff, and resuming missed events.

## Trying it without a real server

```sh
cargo run -p fake-mattermost                      # serves on 127.0.0.1:8065
rustmost --server http://127.0.0.1:8065 login -u alice    # password: secret
```

Users `alice` and `bob` have password `secret`; `carol` also needs MFA code `123456`.

## Development

```sh
cargo test --workspace      # unit tests + end-to-end tests against the fake server
cargo clippy --workspace --all-targets
```
