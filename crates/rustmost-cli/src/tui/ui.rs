//! Drawing. Reads `App`; the only things it writes back are the scroll
//! limits it discovers (`max_scroll`, `page`).

use chrono::Datelike;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use rustmost_api::{ChannelType, Post};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{App, Connection, Entry};
use crate::format::local_time;

const NAME_COL_MAX: usize = 16;
const INPUT_ROWS_MAX: usize = 6;
const GROUP_WITHIN_MS: i64 = 5 * 60 * 1000;

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, status] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(f.area());

    let show_sidebar = main.width >= 60;
    let sidebar_w = if show_sidebar {
        let longest = app
            .entries
            .iter()
            .map(|e| e.label.width())
            .max()
            .unwrap_or(10);
        (longest as u16 + 8).clamp(18, 32)
    } else {
        0
    };
    let [sidebar, right] =
        Layout::horizontal([Constraint::Length(sidebar_w), Constraint::Min(20)]).areas(main);

    let input_inner_w = right.width.saturating_sub(2).max(1) as usize;
    let (input_rows, cursor) = layout_input(app.input.chars(), app.input.cursor(), input_inner_w);
    let input_h = input_rows.len().clamp(1, INPUT_ROWS_MAX) as u16 + 2;
    let [messages, typing, input] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(input_h),
    ])
    .areas(right);

    if show_sidebar {
        draw_sidebar(f, app, sidebar);
    }
    draw_messages(f, app, messages);
    draw_typing(f, app, typing);
    draw_input(f, app, input, input_rows, cursor);
    draw_status(f, app, status);
    if app.switcher.is_some() {
        draw_switcher(f, app, main);
    }
}

// ------------------------------------------------------------------ helpers

fn user_color(name: &str) -> Color {
    const COLORS: [Color; 6] = [
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
    ];
    let h = name
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    COLORS[(h as usize) % COLORS.len()]
}

/// Cut `s` to `width` columns, adding `…` if it was cut.
pub fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Word-wrap `text` to `width` columns. Newlines start new lines; words
/// longer than the width are broken.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut line_w = 0;
        for word in para.split(' ') {
            let word = word.replace('\t', "    ");
            let ww = word.width();
            if line_w > 0 && line_w + 1 + ww > width {
                lines.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if line_w > 0 {
                line.push(' ');
                line_w += 1;
            }
            if ww > width {
                for c in word.chars() {
                    let cw = c.width().unwrap_or(0);
                    if line_w + cw > width {
                        lines.push(std::mem::take(&mut line));
                        line_w = 0;
                    }
                    line.push(c);
                    line_w += cw;
                }
            } else {
                line.push_str(&word);
                line_w += ww;
            }
        }
        lines.push(line);
    }
    lines
}

fn sigil(e: &Entry) -> &'static str {
    match e.channel.channel_type {
        ChannelType::Open => "#",
        ChannelType::Private => "*",
        ChannelType::Direct => "@",
        ChannelType::Group => "&",
    }
}

fn entry_name(e: &Entry) -> &str {
    e.label.strip_prefix('@').unwrap_or(&e.label)
}

// ------------------------------------------------------------------ sidebar

fn draw_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let title = format!(" {} ", app.team().display_name);
    let block = Block::bordered().title(title.bold());
    let inner_w = area.width.saturating_sub(2) as usize;

    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|e| {
            let badge = if e.unread.mentions > 0 {
                format!(" {}", e.unread.mentions)
            } else {
                String::new()
            };
            let name_w = inner_w.saturating_sub(2 + badge.width());
            let name = truncate(entry_name(e), name_w);
            let pad = inner_w.saturating_sub(2 + name.width() + badge.width());
            let mut name_style = Style::default();
            if e.muted {
                name_style = name_style.add_modifier(Modifier::DIM);
            } else if e.unread.is_unread() {
                name_style = name_style.add_modifier(Modifier::BOLD);
            }
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", sigil(e)),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(name, name_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(
                    badge,
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(list, area, &mut state);
}

// ----------------------------------------------------------------- messages

pub fn message_lines(app: &App, posts: &[Post], width: usize) -> Vec<Line<'static>> {
    let authors: Vec<String> = posts.iter().map(|p| author(app, p)).collect();
    let name_w = authors
        .iter()
        .map(|a| a.width())
        .max()
        .unwrap_or(4)
        .min(NAME_COL_MAX);
    // "HH:MM " + name column + " "
    let text_col = 6 + name_w + 1;
    let text_w = width.saturating_sub(text_col).max(10);
    let indent = " ".repeat(text_col);
    let me_tag = format!("@{}", app.my_username);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut prev: Option<(&Post, &str)> = None;

    for (p, author) in posts.iter().zip(authors.iter()) {
        let t = local_time(p.create_at);
        let new_day =
            prev.is_none_or(|(q, _)| local_time(q.create_at).date_naive() != t.date_naive());
        if new_day {
            let label = format!(
                " {} ",
                t.format(if t.year() == chrono::Local::now().year() {
                    "%a %b %d"
                } else {
                    "%a %b %d %Y"
                })
            );
            let side = width.saturating_sub(label.width()) / 2;
            out.push(
                Line::from(format!("{}{}{}", "─".repeat(side), label, "─".repeat(side))).dim(),
            );
        }
        let grouped = !new_day
            && prev.is_some_and(|(q, a)| {
                a == author
                    && p.create_at - q.create_at < GROUP_WITHIN_MS
                    && !q.is_system()
                    && !p.is_system()
            });
        prev = Some((p, author));

        let time = if grouped {
            "      ".to_string()
        } else {
            format!("{} ", t.format("%H:%M"))
        };
        let name = if grouped {
            " ".repeat(name_w + 1)
        } else {
            let n = truncate(author, name_w);
            format!("{}{} ", " ".repeat(name_w - n.width()), n)
        };
        let name_style = Style::default()
            .fg(user_color(author))
            .add_modifier(Modifier::BOLD);

        let (text, text_style) = if p.delete_at > 0 {
            (
                "(message deleted)".to_string(),
                Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
            )
        } else if p.is_system() {
            let m = if p.message.is_empty() {
                p.post_type.clone()
            } else {
                p.message.clone()
            };
            (
                m,
                Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
            )
        } else {
            let mentioned = p.message.contains(&me_tag)
                || ["@channel", "@here", "@all"]
                    .iter()
                    .any(|m| p.message.contains(m));
            (
                p.message.clone(),
                if mentioned {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                },
            )
        };
        let text = if p.is_reply() {
            format!("↳ {text}")
        } else {
            text
        };

        let mut wrapped = wrap(&text, text_w);
        if wrapped.is_empty() {
            wrapped.push(String::new());
        }
        let n = wrapped.len();
        for (i, line) in wrapped.into_iter().enumerate() {
            let mut spans = if i == 0 {
                vec![
                    Span::styled(time.clone(), Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(name.clone(), name_style),
                ]
            } else {
                vec![Span::raw(indent.clone())]
            };
            let lw = line.width();
            spans.push(Span::styled(line, text_style));
            if i == n - 1 && p.is_edited() && p.delete_at == 0 {
                if lw + 9 <= text_w {
                    spans.push(Span::raw(" (edited)").dim());
                } else {
                    out.push(Line::from(spans));
                    spans = vec![Span::raw(indent.clone()), Span::raw("(edited)").dim()];
                }
            }
            out.push(Line::from(spans));
        }

        // Files, reactions and reply counts on one dim line.
        let mut extras: Vec<String> = Vec::new();
        if let Some(meta) = &p.metadata {
            extras.extend(meta.files.iter().map(|f| format!("[file: {}]", f.name)));
            let mut counts: Vec<(String, usize)> = Vec::new();
            for r in &meta.reactions {
                match counts.iter_mut().find(|(e, _)| e == &r.emoji_name) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((r.emoji_name.clone(), 1)),
                }
            }
            extras.extend(counts.into_iter().map(|(e, n)| {
                if n > 1 {
                    format!(":{e}: {n}")
                } else {
                    format!(":{e}:")
                }
            }));
        } else if !p.file_ids.is_empty() {
            extras.push(format!("[{} file(s)]", p.file_ids.len()));
        }
        if !p.is_reply() && p.reply_count > 0 {
            extras.push(format!(
                "{} repl{}",
                p.reply_count,
                if p.reply_count == 1 { "y" } else { "ies" }
            ));
        }
        if !extras.is_empty() {
            for line in wrap(&extras.join("  "), text_w) {
                out.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::raw(line).dim(),
                ]));
            }
        }
    }
    out
}

fn author(app: &App, p: &Post) -> String {
    if p.is_system() {
        return "*".to_string();
    }
    p.override_username()
        .map(str::to_string)
        .unwrap_or_else(|| app.s.username(&p.user_id))
}

fn draw_messages(f: &mut Frame, app: &mut App, area: Rect) {
    let (title, purpose) = match app.current() {
        Some(e) => (
            format!(" {}{} ", sigil(e), entry_name(e)),
            e.channel.header.clone(),
        ),
        None => (" (no channel) ".into(), String::new()),
    };
    let mut block = Block::bordered().title(title.clone().bold());
    if !purpose.is_empty() {
        let w = area.width.saturating_sub(title.width() as u16 + 6) as usize;
        block = block.title(
            Span::raw(format!(
                " {} ",
                truncate(purpose.lines().next().unwrap_or_default(), w)
            ))
            .dim(),
        );
    }
    let inner = block.inner(area);
    let h = inner.height as usize;
    let w = inner.width as usize;

    let lines = match app.current_view() {
        Some(v) => {
            let mut l = message_lines(app, &v.posts, w);
            if v.posts.is_empty() {
                l.push(Line::from("No messages yet.").dim());
            } else if v.has_more {
                l.insert(0, Line::from("↑ scroll up for older messages").dim());
            } else {
                l.insert(0, Line::from("(beginning of channel)").dim());
            }
            l
        }
        None => vec![Line::from("Loading…").dim()],
    };

    app.page = h.max(1);
    app.max_scroll = lines.len().saturating_sub(h);
    app.scroll = app.scroll.min(app.max_scroll);
    let end = lines.len() - app.scroll;
    let start = end.saturating_sub(h);
    let mut visible: Vec<Line> = lines[start..end].to_vec();
    // Keep messages anchored to the bottom when the channel is short.
    while visible.len() < h {
        visible.insert(0, Line::raw(""));
    }

    if app.scroll > 0 {
        block = block.title_bottom(
            Line::from(format!(" ↓ {} more lines · Esc to jump down ", app.scroll))
                .right_aligned()
                .yellow(),
        );
    }
    f.render_widget(Paragraph::new(visible).block(block), area);
}

fn draw_typing(f: &mut Frame, app: &App, area: Rect) {
    let names = app.typing_here();
    let text = match names.as_slice() {
        [] => String::new(),
        [one] => format!(" {one} is typing…"),
        [a, b] => format!(" {a} and {b} are typing…"),
        _ => " Several people are typing…".to_string(),
    };
    f.render_widget(Paragraph::new(text).dim().italic(), area);
}

// -------------------------------------------------------------------- input

/// Split the input into screen rows (wrapping by character) and find the
/// cursor's (row, column).
fn layout_input(chars: &[char], cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let mut rows = vec![String::new()];
    let mut col = 0;
    let mut cur = (0, 0);
    for (i, &c) in chars.iter().enumerate() {
        if i == cursor {
            cur = (rows.len() - 1, col);
        }
        if c == '\n' {
            rows.push(String::new());
            col = 0;
            continue;
        }
        let cw = c.width().unwrap_or(0);
        if col + cw > width {
            rows.push(String::new());
            col = 0;
            if i == cursor {
                cur = (rows.len() - 1, 0);
            }
        }
        rows.last_mut().unwrap().push(c);
        col += cw;
    }
    if cursor >= chars.len() {
        if col >= width {
            rows.push(String::new());
            col = 0;
        }
        cur = (rows.len() - 1, col);
    }
    (rows, cur)
}

fn draw_input(f: &mut Frame, app: &App, area: Rect, rows: Vec<String>, cursor: (usize, usize)) {
    let target = app
        .current()
        .map(|e| format!("{}{}", sigil(e), entry_name(e)))
        .unwrap_or_default();
    let block = Block::bordered().title(format!(" Message {target} "));
    let inner = block.inner(area);
    let h = inner.height as usize;
    let first = cursor.0.saturating_sub(h.saturating_sub(1));

    let body: Vec<Line> = if app.input.is_empty() {
        vec![Line::from("Enter to send · Alt-Enter for a new line").dim()]
    } else {
        rows.into_iter()
            .skip(first)
            .take(h)
            .map(Line::from)
            .collect()
    };
    f.render_widget(Paragraph::new(body).block(block), area);
    if app.switcher.is_none() {
        f.set_cursor_position(Position::new(
            inner.x + cursor.1 as u16,
            inner.y + (cursor.0 - first) as u16,
        ));
    }
}

// ------------------------------------------------------------------- status

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let mut left: Vec<Span> = match &app.connection {
        Connection::Connecting => vec![Span::raw(" ◌ connecting").yellow()],
        Connection::Connected => vec![Span::raw(" ● live").green()],
        Connection::Reconnecting { reason, retry_in } => {
            vec![
                Span::raw(format!(
                    " ○ reconnecting in {}s ({})",
                    retry_in.as_secs(),
                    truncate(reason, 40)
                ))
                .yellow(),
            ]
        }
        Connection::Lost(why) => vec![Span::raw(format!(" ✕ {why}")).red()],
    };
    if let Some(flash) = &app.flash {
        left.push(Span::raw("  "));
        left.push(if flash.error {
            Span::raw(flash.text.clone()).red().bold()
        } else {
            Span::raw(flash.text.clone())
        });
    }
    let used: usize = left.iter().map(|s| s.content.width()).sum();
    let hints = if app.switcher.is_some() {
        "type to filter · ↑↓ choose · Enter open · Esc close "
    } else if app.teams.len() > 1 {
        "^K switch · Tab unread · ^N/^P · PgUp/PgDn · ^T team · ^C quit "
    } else {
        "^K switch · Tab unread · ^N/^P · PgUp/PgDn · ^C quit "
    };
    let room = (area.width as usize).saturating_sub(used + 1);
    if hints.width() <= room {
        left.push(Span::raw(" ".repeat(room - hints.width() + 1)));
        left.push(Span::raw(hints).dim());
    }
    f.render_widget(Paragraph::new(Line::from(left)), area);
}

// ----------------------------------------------------------------- switcher

fn draw_switcher(f: &mut Frame, app: &App, area: Rect) {
    let w = area.width.saturating_sub(4).min(60);
    let h = area.height.saturating_sub(2).min(20);
    let popup = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height.saturating_sub(h)) / 3,
        w,
        h,
    );
    f.render_widget(Clear, popup);
    let block = Block::bordered().title(" Switch channel ".bold());
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let [query_area, list_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    let sw = app.switcher.as_ref().unwrap();
    let q = sw.query.text();
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![Span::raw("> ").bold(), Span::raw(q.clone())]),
            Line::raw(""),
        ]),
        query_area,
    );
    f.set_cursor_position(Position::new(
        query_area.x + 2 + q[..].width() as u16,
        query_area.y,
    ));

    let matches = app.switcher_matches();
    let items: Vec<ListItem> = matches
        .iter()
        .map(|&i| {
            let e = &app.entries[i];
            let mut spans = vec![
                Span::raw(format!("{} ", sigil(e))).dim(),
                Span::raw(entry_name(e).to_string()),
            ];
            if e.channel.channel_type != ChannelType::Direct
                && e.channel.name != e.label.to_lowercase()
            {
                spans.push(Span::raw(format!("  ~{}", e.channel.name)).dim());
            }
            if e.unread.mentions > 0 {
                spans.push(Span::raw(format!("  ({})", e.unread.mentions)).red().bold());
            } else if e.unread.is_unread() {
                spans.push(Span::raw("  •").bold());
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let empty = items.is_empty();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected((!empty).then_some(sw.selected));
    f.render_stateful_widget(list, list_area, &mut state);
    if empty {
        f.render_widget(Paragraph::new("No matching channels.").dim(), list_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words_and_long_tokens() {
        assert_eq!(wrap("hello there world", 11), ["hello there", "world"]);
        assert_eq!(wrap("a\n\nb", 5), ["a", "", "b"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
    }

    #[test]
    fn truncates_by_width() {
        assert_eq!(truncate("town-square", 20), "town-square");
        assert_eq!(truncate("town-square", 6), "town-…");
    }

    #[test]
    fn input_layout_tracks_cursor() {
        let chars: Vec<char> = "abcdef\ngh".chars().collect();
        let (rows, cur) = layout_input(&chars, chars.len(), 4);
        assert_eq!(rows, ["abcd", "ef", "gh"]);
        assert_eq!(cur, (2, 2));
        let (_, cur) = layout_input(&chars, 4, 4);
        assert_eq!(cur, (1, 0));
        let (rows, cur) = layout_input(&['a', 'b', 'c', 'd'], 4, 4);
        assert_eq!((rows.len(), cur), (2, (1, 0)));
    }
}
