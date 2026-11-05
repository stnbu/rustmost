//! End-to-end tests of the client against the in-memory fake server.

use std::time::Duration;

use rustmost_api::{Client, CreatePost, OptionalExt, PostsQuery, Unread, WebSocket, WsEvent};

async fn logged_in(server: &fake_mattermost::FakeServer, user: &str) -> Client {
    let mut c = Client::new(&server.url()).unwrap();
    c.login(user, "secret", None).await.unwrap();
    c
}

/// Wait for the first event matching `f`, skipping others.
async fn wait_for<T>(ws: &mut WebSocket, mut f: impl FnMut(WsEvent) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let ev = ws.next().await.expect("stream ended");
            if let Some(v) = f(ev) {
                return v;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

#[tokio::test]
async fn ping_reports_version() {
    let server = fake_mattermost::start().await;
    let c = Client::new(&server.url()).unwrap();
    assert_eq!(c.ping().await.unwrap().as_deref(), Some("11.7.3"));
    assert_eq!(c.client_config().await.unwrap()["Version"], "11.7.3");
}

#[tokio::test]
async fn login_errors_and_mfa() {
    let server = fake_mattermost::start().await;
    let mut c = Client::new(&server.url()).unwrap();

    let e = c.login("alice", "wrong", None).await.unwrap_err();
    assert_eq!(e.status(), Some(401));
    assert!(!e.is_mfa_error());
    assert!(c.token().is_none());

    let e = c.login("carol", "secret", None).await.unwrap_err();
    assert!(e.is_mfa_error(), "{e:?}");
    let e = c
        .login("carol", "secret", Some("000000"))
        .await
        .unwrap_err();
    assert!(e.is_mfa_error());

    let (user, token) = c.login("carol", "secret", Some("123456")).await.unwrap();
    assert_eq!(user.username, "carol");
    assert_eq!(c.token(), Some(token.as_str()));
    assert_eq!(c.me().await.unwrap().id, user.id);

    // Login by email works too.
    let mut c2 = Client::new(&server.url()).unwrap();
    assert_eq!(
        c2.login("alice@example.com", "secret", None)
            .await
            .unwrap()
            .0
            .username,
        "alice"
    );

    c.logout().await.unwrap();
    assert!(c.token().is_none());
    let stale = Client::new(&server.url()).unwrap().with_token(token);
    let e = stale.me().await.unwrap_err();
    assert!(e.is_unauthorized());
    assert_eq!(e.api_id(), Some("api.context.session_expired.app_error"));
}

async fn unread(c: &Client, team: String, channel: String) -> Unread {
    let chans = c.my_channels(&team).await.unwrap();
    let members = c.my_channel_members(&team).await.unwrap();
    let ch = chans.iter().find(|x| x.id == channel).unwrap();
    let m = members.iter().find(|x| x.channel_id == channel).unwrap();
    Unread::compute(ch, m, false)
}

#[tokio::test]
async fn teams_channels_and_unread() {
    let server = fake_mattermost::start().await;
    let alice = logged_in(&server, "alice").await;
    let me = alice.me().await.unwrap();

    let teams = alice.my_teams().await.unwrap();
    assert_eq!(teams.len(), 1);
    assert_eq!(alice.team_by_name("dev").await.unwrap().id, teams[0].id);
    assert!(
        alice
            .team_by_name("nope")
            .await
            .optional()
            .unwrap()
            .is_none()
    );

    let town = alice
        .channel_by_name(&teams[0].id, "town-square")
        .await
        .unwrap();
    assert_eq!(town.display_name, "Town Square");

    // Seed data: bob posted, then alice replied -> alice has read everything.
    assert_eq!(
        unread(&alice, teams[0].id.clone(), town.id.clone())
            .await
            .messages,
        0
    );

    server.post_as("bob", "town-square", "one");
    server.post_as("bob", "town-square", "two @alice");
    let u = unread(&alice, teams[0].id.clone(), town.id.clone()).await;
    assert_eq!(
        u,
        Unread {
            messages: 2,
            mentions: 1
        }
    );
    assert_eq!(
        alice
            .my_channel_member(&town.id)
            .await
            .unwrap()
            .mention_count,
        1
    );

    alice.view_channel(&town.id).await.unwrap();
    assert!(
        !unread(&alice, teams[0].id.clone(), town.id.clone())
            .await
            .is_unread()
    );

    // DMs
    let bob_id = server.user_id("bob");
    let dm = alice.direct_channel(&me.id, &bob_id).await.unwrap();
    assert!(dm.channel_type.is_dm_or_group());
    assert_eq!(dm.dm_other_user(&me.id).as_deref(), Some(bob_id.as_str()));
    // Asking again returns the same channel.
    assert_eq!(
        alice.direct_channel(&me.id, &bob_id).await.unwrap().id,
        dm.id
    );

    let users = alice
        .users_by_ids(&[me.id.clone(), bob_id.clone()])
        .await
        .unwrap();
    assert_eq!(users.len(), 2);
    assert_eq!(
        alice.user_by_username("@bob").await.unwrap().display_name(),
        "Bob Brown"
    );
}

#[tokio::test]
async fn posts_threads_edits_reactions_files() {
    let server = fake_mattermost::start().await;
    let alice = logged_in(&server, "alice").await;
    let me = alice.me().await.unwrap();
    let town = server.channel_id("town-square");

    let root = alice
        .create_post(&CreatePost::new(&town, "root message"))
        .await
        .unwrap();
    let reply = alice
        .create_post(&CreatePost::new(&town, "a reply").reply_to(&root.id))
        .await
        .unwrap();
    assert!(reply.is_reply());
    assert_eq!(reply.root_id, root.id);

    // Replying to a reply is rejected by the server (must target the root).
    let e = alice
        .create_post(&CreatePost::new(&town, "x").reply_to(&reply.id))
        .await
        .unwrap_err();
    assert_eq!(e.status(), Some(400));

    let latest = alice.posts(&town, &PostsQuery::latest(3)).await.unwrap();
    let msgs: Vec<_> = latest
        .oldest_first()
        .iter()
        .map(|p| p.message.clone())
        .collect();
    assert_eq!(msgs, ["Thanks, Bob.", "root message", "a reply"]);
    assert_eq!(latest.newest_first()[0].message, "a reply");

    let thread = alice.thread(&reply.id).await.unwrap();
    let msgs: Vec<_> = thread
        .chronological()
        .iter()
        .map(|p| p.message.clone())
        .collect();
    assert_eq!(msgs, ["root message", "a reply"]);

    let edited = alice
        .edit_post(&root.id, "root message (fixed)")
        .await
        .unwrap();
    assert!(edited.is_edited());

    alice.add_reaction(&me.id, &root.id, ":+1:").await.unwrap();
    let got = alice.post_by_id(&root.id).await.unwrap();
    let meta = got.metadata.unwrap();
    assert_eq!(meta.reactions[0].emoji_name, "+1");

    let file = alice
        .upload_file(&town, "notes.txt", b"hello".to_vec())
        .await
        .unwrap();
    assert_eq!(file.name, "notes.txt");
    assert_eq!(file.size, 5);
    let mut p = CreatePost::new(&town, "with file");
    p.file_ids.push(file.id.clone());
    let post = alice.create_post(&p).await.unwrap();
    assert_eq!(post.file_ids, vec![file.id]);
    assert_eq!(post.metadata.unwrap().files[0].name, "notes.txt");

    let team = &alice.my_teams().await.unwrap()[0];
    let found = alice.search_posts(&team.id, "fixed").await.unwrap();
    assert_eq!(found.order.len(), 1);

    alice.delete_post(&post.id).await.unwrap();
    assert!(
        alice
            .post_by_id(&post.id)
            .await
            .optional()
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn websocket_events_typing_and_reconnect() {
    let server = fake_mattermost::start().await;
    let alice = logged_in(&server, "alice").await;
    let bob = logged_in(&server, "bob").await;
    let alice_id = server.user_id("alice");
    let town = server.channel_id("town-square");

    let mut ws = alice.websocket().await.unwrap();
    let (first_conn, version) = wait_for(&mut ws, |e| match e {
        WsEvent::Connected {
            connection_id,
            server_version,
            resumed,
        } => {
            assert!(!resumed);
            Some((connection_id, server_version))
        }
        _ => None,
    })
    .await;
    assert!(version.starts_with("11.7.3"));

    bob.create_post(&CreatePost::new(&town, "hey @alice"))
        .await
        .unwrap();
    let p = wait_for(&mut ws, |e| match e {
        WsEvent::Posted(p) => Some(p),
        _ => None,
    })
    .await;
    assert_eq!(p.post.message, "hey @alice");
    assert_eq!(p.sender_name, "@bob");
    assert_eq!(p.channel_name, "town-square");
    assert_eq!(p.channel_display_name, "Town Square");
    assert_eq!(p.mentions, vec![alice_id.clone()]);

    // Bob sees alice typing.
    let mut bob_ws = bob.websocket().await.unwrap();
    wait_for(&mut bob_ws, |e| {
        matches!(e, WsEvent::Connected { .. }).then_some(())
    })
    .await;
    ws.handle().send_typing(&town, None);
    let (ch, who) = wait_for(&mut bob_ws, |e| match e {
        WsEvent::Typing {
            channel_id,
            user_id,
            ..
        } => Some((channel_id, user_id)),
        _ => None,
    })
    .await;
    assert_eq!((ch, who), (town.clone(), alice_id.clone()));

    // Marking read on "another device" reaches alice's stream.
    alice.view_channel(&town).await.unwrap();
    let ids = wait_for(&mut ws, |e| match e {
        WsEvent::ChannelsViewed { channel_ids } => Some(channel_ids),
        _ => None,
    })
    .await;
    assert_eq!(ids, vec![town.clone()]);

    // Drop every connection; the client reconnects by itself.
    server.kick_websockets();
    wait_for(&mut ws, |e| {
        matches!(e, WsEvent::Disconnected { .. }).then_some(())
    })
    .await;
    let second_conn = wait_for(&mut ws, |e| match e {
        WsEvent::Connected { connection_id, .. } => Some(connection_id),
        _ => None,
    })
    .await;
    assert_ne!(first_conn, second_conn);

    server.post_as("bob", "town-square", "after reconnect");
    let p = wait_for(&mut ws, |e| match e {
        WsEvent::Posted(p) => Some(p),
        _ => None,
    })
    .await;
    assert_eq!(p.post.message, "after reconnect");
}

#[tokio::test]
async fn websocket_rejects_bad_token() {
    let server = fake_mattermost::start().await;
    let c = Client::new(&server.url()).unwrap().with_token("nope");
    let err = c.websocket().await.err().expect("should fail");
    assert!(err.to_string().contains("401"), "{err}");

    let unauth = Client::new(&server.url()).unwrap();
    assert!(matches!(
        unauth.websocket().await,
        Err(rustmost_api::Error::NotAuthenticated)
    ));
}
