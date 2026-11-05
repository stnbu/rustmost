//! Full-screen terminal UI (`rustmost` with no command, or `rustmost tui`).

mod app;
mod input;
mod ui;

#[cfg(test)]
mod tests;

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use rustmost_api::WebSocket;

use crate::session::Session;
pub use app::App;

pub async fn run(session: Session, team: Option<&str>) -> Result<()> {
    // Load everything before taking over the screen, so errors print normally.
    eprintln!("Connecting to {}…", session.client.server_url());
    let mut app = App::new(session, team).await?;
    let ws = app.s.client.websocket().await?;
    app.ws = Some(ws.handle());

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = event_loop(&mut terminal, &mut app, ws).await;
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, ws: WebSocket) -> Result<()> {
    let mut keys = EventStream::new();
    let mut ws = Some(ws);
    let mut tick = tokio::time::interval(Duration::from_millis(500));

    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;
        tokio::select! {
            ev = keys.next() => match ev {
                Some(Ok(Event::Key(k))) if k.kind != KeyEventKind::Release => app.on_key(k).await,
                Some(Ok(Event::Paste(text))) => app.on_paste(&text),
                Some(Ok(_)) => {} // resize etc.: just redraw
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            ev = async { ws.as_mut().unwrap().next().await }, if ws.is_some() => match ev {
                Some(ev) => app.on_ws(ev).await,
                None => {
                    ws = None;
                    if !matches!(app.connection, app::Connection::Lost(_)) {
                        app.connection = app::Connection::Lost("live updates stopped".into());
                    }
                }
            },
            _ = tick.tick() => app.tick().await,
        }
    }
    Ok(())
}
