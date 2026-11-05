//! Run the real `rustmost` binary against the fake server.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};

struct Env {
    server: fake_mattermost::FakeServer,
    config: PathBuf,
}

impl Env {
    async fn new(name: &str) -> Env {
        let server = fake_mattermost::start().await;
        let dir = std::env::temp_dir().join(format!("rustmost-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Env {
            server,
            config: dir.join("config.toml"),
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_rustmost"));
        c.args(args)
            .env("RUSTMOST_CONFIG", &self.config)
            .env("RUSTMOST_PASSWORD", "secret")
            .env("NO_COLOR", "1")
            .env_remove("RUSTMOST_SERVER")
            .env_remove("RUSTMOST_TOKEN");
        c
    }

    /// Run on a blocking thread so the in-process server keeps serving.
    async fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut c = self.cmd(args);
        let stdin = stdin.to_string();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut child = c
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(stdin.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap()
    }

    async fn run(&self, args: &[&str]) -> Output {
        self.run_with_stdin(args, "").await
    }

    async fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args).await;
        assert!(
            out.status.success(),
            "rustmost {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    async fn login(&self, user: &str) {
        let url = self.server.url();
        let out = self.run(&["--server", &url, "login", "-u", user]).await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn login_read_send_reply_thread() {
    let env = Env::new("basic").await;
    env.login("alice").await;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&env.config).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "config holding the token must be private"
        );
    }

    let who = env.ok(&["whoami"]).await;
    assert!(
        who.contains("Alice Anders (@alice)") && who.contains("server 11.7.3"),
        "{who}"
    );

    assert!(env.ok(&["teams"]).await.contains("dev"));

    let read = env.ok(&["read", "town-square"]).await;
    assert!(read.contains("bob: Welcome to the fake server!"), "{read}");
    assert!(
        read.find("Welcome").unwrap() < read.find("Thanks, Bob.").unwrap(),
        "oldest first"
    );

    // Display names and ~ prefixes resolve too.
    assert!(
        env.ok(&["read", "Town Square", "-n", "1"])
            .await
            .contains("Thanks, Bob.")
    );
    assert!(
        env.ok(&["read", "~town-square", "-n", "1"])
            .await
            .contains("Thanks, Bob.")
    );
    assert!(
        env.ok(&["read", "dev/town-square", "-n", "1"])
            .await
            .contains("Thanks, Bob.")
    );

    env.ok(&["send", "off-topic", "hello", "from", "the", "cli"])
        .await;
    assert_eq!(env.server.messages("off-topic"), ["hello from the cli"]);

    // Message from stdin.
    let out = env
        .run_with_stdin(&["send", "off-topic"], "multi\nline\n")
        .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(env.server.messages("off-topic")[1], "multi\nline");

    // Reply: find the id with --ids, reply to it, then show the thread.
    let read = env.ok(&["read", "off-topic", "--ids"]).await;
    let line = read
        .lines()
        .find(|l| l.contains("hello from the cli"))
        .unwrap();
    let id = line.split_whitespace().nth(1).unwrap().to_string();
    assert_eq!(id.len(), 26, "{line}");
    env.ok(&["send", "off-topic", "--reply", &id, "a threaded reply"])
        .await;
    assert_eq!(env.server.root_of("a threaded reply"), id);

    let thread = env.ok(&["thread", &id]).await;
    let lines: Vec<_> = thread.lines().collect();
    assert_eq!(lines.len(), 2, "{thread}");
    assert!(lines[1].contains("↳ alice: a threaded reply"), "{thread}");

    env.ok(&["react", &id, "tada"]).await;
    assert!(env.ok(&["read", "off-topic"]).await.contains(":tada:"));

    let found = env.ok(&["search", "threaded"]).await;
    assert!(
        found.contains("~off-topic") && found.contains("a threaded reply"),
        "{found}"
    );

    // Multi-line messages are indented under the first line.
    let read = env.ok(&["read", "off-topic"]).await;
    let i = read
        .lines()
        .position(|l| l.ends_with("alice: multi"))
        .expect(&read);
    assert!(read.lines().nth(i + 1).unwrap().trim() == "line");
}

#[tokio::test(flavor = "multi_thread")]
async fn unread_dm_and_files() {
    let env = Env::new("unread").await;
    env.login("alice").await;

    env.server.post_as("bob", "town-square", "ping @alice");
    let unread = env.ok(&["channels", "--unread"]).await;
    assert!(
        unread.contains("town-square") && unread.contains("1 new, 1 mention"),
        "{unread}"
    );
    assert!(!unread.contains("off-topic"));

    env.ok(&["read", "town-square", "--mark-read"]).await;
    assert!(
        !env.ok(&["channels", "--unread"])
            .await
            .contains("town-square")
    );

    env.ok(&["send", "@bob", "psst"]).await;
    let chans = env.ok(&["channels"]).await;
    assert!(chans.contains("@bob"), "{chans}");
    assert!(env.ok(&["read", "@bob"]).await.contains("alice: psst"));

    let file = env.config.parent().unwrap().join("report.txt");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "data").unwrap();
    env.ok(&[
        "send",
        "off-topic",
        "see attached",
        "--file",
        file.to_str().unwrap(),
    ])
    .await;
    assert!(
        env.ok(&["read", "off-topic"])
            .await
            .contains("[file: report.txt]")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mfa_login_logout_and_errors() {
    let env = Env::new("mfa").await;
    let url = env.server.url();

    let out = env.run(&["whoami"]).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("rustmost login"));

    let out = env
        .run_with_stdin(&["--server", &url, "login", "-u", "carol"], "123456\n")
        .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("MFA code"));
    assert!(env.ok(&["whoami"]).await.contains("@carol"));

    let out = env.run(&["read", "no-such-channel"]).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no channel 'no-such-channel'"));

    env.ok(&["logout"]).await;
    let out = env.run(&["whoami"]).await;
    assert!(!out.status.success());

    let mut bad = env.cmd(&["--server", &url, "login", "-u", "alice"]);
    bad.env("RUSTMOST_PASSWORD", "wrong");
    let out = tokio::task::spawn_blocking(move || bad.output().unwrap())
        .await
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("valid email or username"));
}

#[tokio::test(flavor = "multi_thread")]
async fn tail_follows_new_messages() {
    let env = Env::new("tail").await;
    env.login("alice").await;

    let mut child = tokio::process::Command::from(env.cmd(&["tail", "town-square"]))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();

    let started = tokio::time::timeout(Duration::from_secs(10), stderr.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(started.contains("Following town-square"), "{started}");

    env.server.post_as("bob", "off-topic", "filtered out");
    env.server.post_as("bob", "town-square", "hi @alice");
    let line = tokio::time::timeout(Duration::from_secs(10), stdout.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        line.contains("~town-square")
            && line.contains("bob: hi @alice")
            && line.contains("mention"),
        "{line}"
    );

    env.server.kick_websockets();
    let mut saw_reconnect = false;
    for _ in 0..4 {
        let l = tokio::time::timeout(Duration::from_secs(10), stderr.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if l.contains("Disconnected") {
            saw_reconnect = true;
            break;
        }
    }
    assert!(saw_reconnect);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    env.server.post_as("bob", "town-square", "still here");
    let line = tokio::time::timeout(Duration::from_secs(10), stdout.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(line.contains("still here"), "{line}");
    child.kill().await.ok();
}
