//! The TUI driven against the fake server, rendered into an off-screen buffer.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rustmost_api::{Client, CreatePost, WebSocket, WsEvent};

use super::app::App;
use super::ui;
use crate::session::Session;

async fn app_for(server: &fake_mattermost::FakeServer, user: &str) -> App {
    let mut client = Client::new(&server.url()).unwrap();
    let (me, _) = client.login(user, "secret", None).await.unwrap();
    App::new(Session::new(client, me.id, None), None)
        .await
        .unwrap()
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

async fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.on_key(key(KeyCode::Char(c))).await;
    }
}

fn render(app: &mut App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| ui::draw(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Feed websocket events to the app until `done` holds.
async fn pump(app: &mut App, ws: &mut WebSocket, done: impl Fn(&App) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done(app) {
            let ev = ws.next().await.expect("websocket ended");
            app.on_ws(ev).await;
        }
    })
    .await
    .expect("timed out waiting for the app to update");
}

async fn connected_ws(app: &mut App) -> WebSocket {
    let mut ws = app.s.client.websocket().await.unwrap();
    app.ws = Some(ws.handle());
    pump(app, &mut ws, |a| {
        a.connection == super::app::Connection::Connected
    })
    .await;
    ws
}

fn entry<'a>(app: &'a App, label: &str) -> &'a super::app::Entry {
    app.entries
        .iter()
        .find(|e| e.label == label)
        .unwrap_or_else(|| panic!("no entry {label}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn first_screen() {
    let server = fake_mattermost::start().await;
    let mut app = app_for(&server, "alice").await;
    let screen = render(&mut app, 90, 16);
    println!("{screen}");
    assert!(screen.contains("Dev Team"));
    assert!(screen.contains("# Off-Topic"));
    assert!(screen.contains("# Town Square"));
    assert!(
        screen.contains("bob Welcome to the fake server!"),
        "{screen}"
    );
    // Same-minute replies from different people both show names.
    assert!(screen.contains("alice Thanks, Bob."), "{screen}");
    assert!(screen.contains("Message #Town Square"));
    assert!(screen.contains("^K switch"));
}

#[tokio::test(flavor = "multi_thread")]
async fn live_unread_tab_and_send() {
    let server = fake_mattermost::start().await;
    let mut app = app_for(&server, "alice").await;
    let mut ws = connected_ws(&mut app).await;
    assert_eq!(app.current().unwrap().label, "Town Square");

    server.post_as("bob", "off-topic", "psst @alice");
    pump(&mut app, &mut ws, |a| {
        entry(a, "Off-Topic").unread.mentions == 1
    })
    .await;
    let screen = render(&mut app, 90, 16);
    println!("{screen}");
    assert!(screen.contains("Off-Topic"));
    assert_eq!(entry(&app, "Off-Topic").unread.messages, 1);

    // A message in the open channel shows up and doesn't count as unread.
    server.post_as("bob", "town-square", "live message");
    pump(&mut app, &mut ws, |a| {
        a.current_view()
            .unwrap()
            .posts
            .iter()
            .any(|p| p.message == "live message")
    })
    .await;
    assert!(!entry(&app, "Town Square").unread.is_unread());
    app.tick().await; // marks it read on the server

    // Tab jumps to the channel with the mention and marks it read.
    app.on_key(key(KeyCode::Tab)).await;
    assert_eq!(app.current().unwrap().label, "Off-Topic");
    assert!(!entry(&app, "Off-Topic").unread.is_unread());
    let m = app
        .s
        .client
        .my_channel_member(&server.channel_id("off-topic"))
        .await
        .unwrap();
    assert_eq!(m.mention_count, 0, "marked read on the server");
    assert!(render(&mut app, 90, 16).contains("bob psst @alice"));

    // Typing and Enter sends to the open channel.
    type_text(&mut app, "hello bob").await;
    assert!(render(&mut app, 90, 16).contains("hello bob"));
    app.on_key(key(KeyCode::Enter)).await;
    assert!(app.input.is_empty());
    assert_eq!(server.messages("off-topic").last().unwrap(), "hello bob");
    let screen = render(&mut app, 90, 16);
    assert!(screen.contains("alice hello bob"), "{screen}");

    // Multi-line with Alt-Enter.
    type_text(&mut app, "line one").await;
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
        .await;
    type_text(&mut app, "line two").await;
    app.on_key(key(KeyCode::Enter)).await;
    assert_eq!(
        server.messages("off-topic").last().unwrap(),
        "line one\nline two"
    );

    // Unknown slash commands are not sent; // escapes.
    type_text(&mut app, "/giphy cats").await;
    app.on_key(key(KeyCode::Enter)).await;
    assert!(app.flash.as_ref().unwrap().error);
    assert_eq!(app.input.text(), "/giphy cats", "kept so you can fix it");
    app.on_key(ctrl('u')).await;
    type_text(&mut app, "//shrug").await;
    app.on_key(key(KeyCode::Enter)).await;
    assert_eq!(server.messages("off-topic").last().unwrap(), "/shrug");
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_edits_reactions_and_new_dm() {
    let server = fake_mattermost::start().await;
    let mut app = app_for(&server, "alice").await;
    let mut ws = connected_ws(&mut app).await;

    let mut bob = Client::new(&server.url()).unwrap();
    let (bob_user, _) = bob.login("bob", "secret", None).await.unwrap();
    let town = server.channel_id("town-square");

    let bob_ws = bob.websocket().await.unwrap();
    bob_ws.handle().send_typing(&town, None);
    pump(&mut app, &mut ws, |a| !a.typing_here().is_empty()).await;
    assert!(render(&mut app, 90, 16).contains("bob is typing…"));

    let post = bob
        .create_post(&CreatePost::new(&town, "tpyo"))
        .await
        .unwrap();
    pump(&mut app, &mut ws, |a| {
        a.current_view()
            .unwrap()
            .posts
            .iter()
            .any(|p| p.id == post.id)
    })
    .await;
    assert!(app.typing_here().is_empty(), "posting clears typing");
    bob.edit_post(&post.id, "typo fixed").await.unwrap();
    pump(&mut app, &mut ws, |a| {
        a.current_view()
            .unwrap()
            .posts
            .iter()
            .any(|p| p.message == "typo fixed")
    })
    .await;
    bob.add_reaction(&bob_user.id, &post.id, "tada")
        .await
        .unwrap();
    pump(&mut app, &mut ws, |a| {
        a.current_view()
            .unwrap()
            .posts
            .iter()
            .any(|p| p.metadata.as_ref().is_some_and(|m| !m.reactions.is_empty()))
    })
    .await;
    let screen = render(&mut app, 90, 16);
    println!("{screen}");
    assert!(screen.contains("typo fixed (edited)"), "{screen}");
    assert!(screen.contains(":tada:"), "{screen}");

    // A brand-new DM appears in the sidebar with a badge.
    let dm = bob.direct_channel(&bob_user.id, &app.s.me).await.unwrap();
    bob.create_post(&CreatePost::new(&dm.id, "got a minute?"))
        .await
        .unwrap();
    pump(&mut app, &mut ws, |a| {
        a.entries.iter().any(|e| e.label == "@bob")
    })
    .await;
    assert_eq!(entry(&app, "@bob").unread.mentions, 1);
    let screen = render(&mut app, 90, 16);
    println!("{screen}");
    assert!(screen.contains("@ bob"), "{screen}");
    assert_eq!(
        app.current().unwrap().label,
        "Town Square",
        "selection kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn switcher_and_channel_keys() {
    let server = fake_mattermost::start().await;
    let mut app = app_for(&server, "alice").await;

    app.on_key(ctrl('k')).await;
    type_text(&mut app, "off").await;
    let screen = render(&mut app, 90, 20);
    println!("{screen}");
    assert!(screen.contains("Switch channel") && screen.contains("> off"));
    assert_eq!(app.switcher_matches().len(), 1);
    app.on_key(key(KeyCode::Enter)).await;
    assert!(app.switcher.is_none());
    assert_eq!(app.current().unwrap().label, "Off-Topic");

    // Subsequence matching ("tsq" -> Town Square), Esc closes without switching.
    app.on_key(ctrl('k')).await;
    type_text(&mut app, "tsq").await;
    assert_eq!(app.entries[app.switcher_matches()[0]].label, "Town Square");
    app.on_key(key(KeyCode::Esc)).await;
    assert_eq!(app.current().unwrap().label, "Off-Topic");

    app.on_key(ctrl('n')).await;
    assert_eq!(app.current().unwrap().label, "Town Square");
    app.on_key(ctrl('p')).await;
    assert_eq!(app.current().unwrap().label, "Off-Topic");

    app.on_key(ctrl('c')).await;
    assert!(app.quit);
}

#[tokio::test(flavor = "multi_thread")]
async fn scrolling_loads_older_history() {
    let server = fake_mattermost::start().await;
    for i in 0..130 {
        server.post_as("bob", "town-square", &format!("message number {i}"));
    }
    let mut app = app_for(&server, "alice").await;
    assert_eq!(app.current_view().unwrap().posts.len(), 60);
    let screen = render(&mut app, 90, 16);
    assert!(screen.contains("message number 129"));

    for _ in 0..200 {
        app.on_key(key(KeyCode::PageUp)).await;
        render(&mut app, 90, 16);
        if !app.current_view().unwrap().has_more && app.scroll == app.max_scroll {
            break;
        }
    }
    assert_eq!(
        app.current_view().unwrap().posts.len(),
        132,
        "all history loaded"
    );
    let screen = render(&mut app, 90, 16);
    println!("{screen}");
    assert!(screen.contains("(beginning of channel)"));
    assert!(screen.contains("Welcome to the fake server!"));
    assert!(screen.contains("more lines"));

    app.on_key(key(KeyCode::Esc)).await;
    assert_eq!(app.scroll, 0);
    assert!(render(&mut app, 90, 16).contains("message number 129"));
}

#[tokio::test(flavor = "multi_thread")]
async fn narrow_terminal_and_long_messages() {
    let server = fake_mattermost::start().await;
    let long = "This is a fairly long message that will certainly need to wrap onto several lines in a narrow terminal window.";
    server.post_as("bob", "town-square", long);
    let mut app = app_for(&server, "alice").await;
    let screen = render(&mut app, 50, 16);
    println!("{screen}");
    assert!(!screen.contains("Dev Team"), "sidebar hidden when narrow");
    // Every word made it on screen, wrapped under the text column.
    let joined: String = screen.split_whitespace().collect::<Vec<_>>().join(" ");
    for w in ["fairly", "certainly", "several", "narrow", "window."] {
        assert!(joined.contains(w), "{w} missing:\n{screen}");
    }
    assert!(!screen.lines().any(|l| l.chars().count() > 50));
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_refetches() {
    let server = fake_mattermost::start().await;
    let mut app = app_for(&server, "alice").await;
    let mut ws = connected_ws(&mut app).await;

    server.kick_websockets();
    pump(&mut app, &mut ws, |a| {
        matches!(a.connection, super::app::Connection::Reconnecting { .. })
    })
    .await;
    assert!(render(&mut app, 90, 16).contains("reconnecting"));
    // Posted while disconnected: the fake server can't replay, so the app refetches.
    server.post_as("bob", "town-square", "sent while you were away");
    pump(&mut app, &mut ws, |a| {
        a.connection == super::app::Connection::Connected
    })
    .await;
    assert!(
        app.current_view()
            .unwrap()
            .posts
            .iter()
            .any(|p| p.message == "sent while you were away"),
        "history refetched after reconnect"
    );
    assert!(render(&mut app, 90, 16).contains("● live"));
    let _ = WsEvent::MissedEvents;
}
