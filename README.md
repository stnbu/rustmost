# rustmost

100% written by claude (so far)

A [Mattermost](https://mattermost.com) client written entirely in Rust: a
full-screen terminal interface, a command-line tool, and the async library
they're built on. It has been built against Mattermost server 11.7.

| Crate | What it is |
|---|---|
| [`rustmost`](crates/rustmost-cli) | The client: full-screen interface plus command-line commands. |
| [`rustmost-api`](crates/rustmost-api) | An async library for the Mattermost v4 REST API and WebSocket event stream. |
| [`fake-mattermost`](crates/fake-mattermost) | An in-memory fake server used by the tests, and handy for trying things offline. |

## Getting started

You need a recent stable Rust. On macOS, install it with rustup if you don't have it:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Everything below runs straight from this folder with `cargo run`; nothing is
installed. `-p rustmost` picks the client crate, `-q` hides Cargo's build
output, and everything after `--` goes to rustmost. The first run takes a
minute or two to compile; after that it starts almost instantly. (If the build
complains about a missing C compiler, run `xcode-select --install`.)

Log in once. The server address and a session token are saved for next time:

```sh
cargo run -q -p rustmost -- --server https://chat.example.com:8065/ login
```

It asks for your username (or email) and password, and for an MFA code if your
account uses one. Then open the full-screen interface:

```sh
cargo run -q -p rustmost
```

To save typing, you can define an alias for the current terminal session:

```sh
alias rustmost='cargo run -q --manifest-path ~/git/rustmost/Cargo.toml -p rustmost --'
rustmost channels
```

If you'd rather install it, `cargo install --path crates/rustmost-cli` puts a
`rustmost` binary in `~/.cargo/bin`.

## The full-screen interface

```sh
cargo run -q -p rustmost                 # also: cargo run -q -p rustmost -- tui
cargo run -q -p rustmost -- -t myteam    # start in a particular team
```

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
in yellow, and "… is typing" appears above the message box. The bottom line
shows whether live updates are connected.

Pasting multi-line text works. To send text that starts with `/`, begin it
with `//`. On macOS Terminal, Alt combinations need "Use Option as Meta key"
turned on in the profile's Keyboard settings; Ctrl-J and Ctrl-N / Ctrl-P work
without it.

### Not in this version

The full-screen interface doesn't do these yet:

- Viewing threads or replying in them (use the `thread` and `send --reply` commands below)
- Slash commands (`/giphy`, `/away` and so on)
- Attaching or downloading files (use `send --file`)
- Editing or deleting your messages
- Adding reactions (use `react`)
- Rendering Markdown: messages are shown as plain text
- Notifications or sounds
- Showing who's online
- Single sign-on (SSO) logins: use a username and password, or a personal access token

## Command-line commands

Each command does one thing and exits, which is handy for scripts. They use the
same saved login.

```sh
cargo run -q -p rustmost -- whoami                 # who you are and the server version
cargo run -q -p rustmost -- teams
cargo run -q -p rustmost -- use my-team            # set a default team, if you're in several

cargo run -q -p rustmost -- channels               # with unread / mention counts
cargo run -q -p rustmost -- channels --unread
cargo run -q -p rustmost -- read town-square       # last 30 messages
cargo run -q -p rustmost -- read town-square -n 100 --mark-read
cargo run -q -p rustmost -- read @bob              # a DM
cargo run -q -p rustmost -- read other-team/off-topic

cargo run -q -p rustmost -- send town-square "Hello from Rust"
printf "line one\nline two\n" | cargo run -q -p rustmost -- send town-square
cargo run -q -p rustmost -- send town-square --file report.pdf "see attached"

cargo run -q -p rustmost -- read town-square --ids # show post ids
cargo run -q -p rustmost -- send town-square --reply <post-id> "a reply in the thread"
cargo run -q -p rustmost -- thread <post-id>
cargo run -q -p rustmost -- react <post-id> +1

cargo run -q -p rustmost -- search "from:bob deploy"
cargo run -q -p rustmost -- tail                   # live messages from everywhere; Ctrl-C to stop
cargo run -q -p rustmost -- tail town-square @bob  # just these

cargo run -q -p rustmost -- logout
cargo run -q -p rustmost -- --help                 # all commands and options
```

Channels can be given by URL name (`town-square`), display name (`"Town Square"`),
`team/channel`, `@username` for DMs, or channel id.

The session token is saved in `~/Library/Application Support/rustmost/config.toml`
on macOS (`~/.config/rustmost/config.toml` on Linux), readable only by you.
`logout` ends the session on the server and removes the token. You can override
settings with the environment variables `RUSTMOST_SERVER`, `RUSTMOST_TOKEN`
(for example a personal access token) and `RUSTMOST_CONFIG`.

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

In one terminal, start the fake server:

```sh
cargo run -q -p fake-mattermost                    # serves on 127.0.0.1:8065
```

In another, log in to it and open the interface:

```sh
cargo run -q -p rustmost -- --server http://127.0.0.1:8065 login -u alice   # password: secret
cargo run -q -p rustmost
```

Users `alice` and `bob` have password `secret`; `carol` also needs MFA code
`123456`. Logging in to the fake server replaces your saved real login, so log
in to your real server again afterwards.

## Development

```sh
cargo test --workspace                  # unit tests + end-to-end tests against the fake server
cargo clippy --workspace --all-targets
```
