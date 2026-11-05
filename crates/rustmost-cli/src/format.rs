//! Plain-text rendering of posts for the terminal.

use std::collections::BTreeMap;
use std::io::IsTerminal;

use chrono::{DateTime, Local, TimeZone};
use rustmost_api::Post;

#[derive(Clone, Copy)]
pub struct Style {
    pub color: bool,
    pub show_ids: bool,
}

impl Style {
    pub fn detect(show_ids: bool) -> Self {
        let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Style { color, show_ids }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }

    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }

    /// A stable color per username so conversations are easy to follow.
    pub fn user(&self, name: &str) -> String {
        const COLORS: [&str; 6] = ["1;31", "1;32", "1;33", "1;34", "1;35", "1;36"];
        let h = name
            .bytes()
            .fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
        self.paint(COLORS[(h as usize) % COLORS.len()], name)
    }
}

pub fn local_time(ms: i64) -> DateTime<Local> {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Local::now)
}

/// `14:03` for today, `Nov 04 14:03` this year, `2025-11-04 14:03` otherwise.
pub fn timestamp(ms: i64) -> String {
    let t = local_time(ms);
    let now = Local::now();
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else if t.format("%Y").to_string() == now.format("%Y").to_string() {
        t.format("%b %d %H:%M").to_string()
    } else {
        t.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// Render one post. `author` is the display name to show.
pub fn post(style: &Style, p: &Post, author: &str, prefix: Option<&str>) -> String {
    let mut head = String::new();
    head.push_str(&style.dim(&format!("[{}]", timestamp(p.create_at))));
    head.push(' ');
    if let Some(prefix) = prefix {
        head.push_str(&style.dim(prefix));
        head.push(' ');
    }
    if style.show_ids {
        head.push_str(&style.dim(&p.id));
        head.push(' ');
    }
    let reply = if p.is_reply() { "↳ " } else { "" };

    if p.is_system() {
        let text = if p.message.is_empty() {
            p.post_type.clone()
        } else {
            p.message.clone()
        };
        return format!("{head}{reply}{}", style.dim(&format!("* {text}")));
    }

    let indent =
        " ".repeat(visible_len(&head) + reply.chars().count() + author.chars().count() + 2);
    let mut body = p
        .message
        .lines()
        .collect::<Vec<_>>()
        .join(&format!("\n{indent}"));
    if p.delete_at > 0 {
        body = style.dim("(message deleted)");
    }
    let mut extras = Vec::new();
    if p.is_edited() {
        extras.push(style.dim("(edited)"));
    }
    if let Some(meta) = &p.metadata {
        for f in &meta.files {
            extras.push(style.dim(&format!("[file: {}]", f.name)));
        }
        if !meta.reactions.is_empty() {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for r in &meta.reactions {
                *counts.entry(&r.emoji_name).or_default() += 1;
            }
            let r: Vec<String> = counts
                .iter()
                .map(|(e, n)| {
                    if *n > 1 {
                        format!(":{e}:×{n}")
                    } else {
                        format!(":{e}:")
                    }
                })
                .collect();
            extras.push(style.dim(&r.join(" ")));
        }
    }
    if p.metadata.is_none() && !p.file_ids.is_empty() {
        extras.push(style.dim(&format!("[{} file(s)]", p.file_ids.len())));
    }
    if p.reply_count > 0 && !p.is_reply() {
        extras.push(style.dim(&format!(
            "[{} repl{}]",
            p.reply_count,
            if p.reply_count == 1 { "y" } else { "ies" }
        )));
    }
    let extras = if extras.is_empty() {
        String::new()
    } else {
        format!(" {}", extras.join(" "))
    };
    format!("{head}{reply}{}: {body}{extras}", style.user(author))
}

/// Length of a string without ANSI escape codes.
fn visible_len(s: &str) -> usize {
    let mut n = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c == 'm' {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
        } else {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_multiline_and_extras() {
        let style = Style {
            color: false,
            show_ids: false,
        };
        let p = Post {
            id: "p1".into(),
            create_at: Local::now().timestamp_millis(),
            edit_at: 1,
            message: "line one\nline two".into(),
            ..Default::default()
        };
        let s = post(&style, &p, "alice", None);
        let lines: Vec<&str> = s.lines().collect();
        assert!(lines[0].ends_with("alice: line one"), "{s}");
        assert!(
            lines[1].trim_start().starts_with("line two (edited)"),
            "{s}"
        );
        // continuation lines align under the message
        assert_eq!(lines[0].find("line one"), lines[1].find("line two"));
    }

    #[test]
    fn visible_len_ignores_ansi() {
        assert_eq!(visible_len("\x1b[2m[12:00]\x1b[0m "), 8);
    }
}
