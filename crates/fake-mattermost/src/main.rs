//! Run the fake server for manual testing:
//!
//! ```sh
//! cargo run -p fake-mattermost            # listens on 127.0.0.1:8065
//! rustmost --server http://127.0.0.1:8065 login -u alice   # password: secret
//! ```

#[tokio::main]
async fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8065".to_string());
    let server = fake_mattermost::start_on(&addr).await;
    println!(
        "Fake Mattermost {} on {}",
        fake_mattermost::VERSION,
        server.url()
    );
    println!("Users: alice, bob (password: secret); carol (password: secret, MFA code: 123456)");
    tokio::signal::ctrl_c().await.ok();
}
