//! The human surface. Every key here is one the CLI also has, so nothing is
//! TUI-only: `s` is `mk-feed send`, `t` is `mk-feed schedule`, `c` is `check`.
//!
//! `e` hands the draft to $EDITOR. Writing is the work; a TUI should not try to
//! be a text editor.

use crate::check;
use crate::config::Config;
use crate::publish;
use crate::state::{self, Post, Queue};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Info,
    Warn,
    Error,
}

struct Input {
    prompt: String,
    buffer: String,
}

enum Intent {
    None,
    Send { force: bool },
    Check,
    Schedule(String),
    Cancel,
    Edit,
    Copy,
    Open,
}

struct App<'a> {
    cfg: &'a Config,
    queue: Queue,
    posts: Vec<Post>,
    selected: usize,
    message: String,
    tone: Tone,
    input: Option<Input>,
    quit: bool,
}

pub fn run(cfg: &Config) -> Result<()> {
    let queue = Queue::open(&cfg.queue)?;
    let mut app = App {
        cfg,
        queue,
        posts: Vec::new(),
        selected: 0,
        message: String::new(),
        tone: Tone::Info,
        input: None,
        quit: false,
    };
    app.reload()?;

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = app.event_loop(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

impl<'a> App<'a> {
    fn event_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(300))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    let intent = self.handle_key(key.code, key.modifiers);
                    match intent {
                        Intent::None => {}
                        other => self.act(other, terminal)?,
                    }
                }
            }
            if self.quit {
                return Ok(());
            }
        }
    }

    fn reload(&mut self) -> Result<()> {
        self.posts = self.queue.list()?;
        if self.selected >= self.posts.len() {
            self.selected = self.posts.len().saturating_sub(1);
        }
        Ok(())
    }

    fn current(&self) -> Option<&Post> {
        self.posts.get(self.selected)
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Intent {
        if self.input.is_some() {
            match code {
                KeyCode::Esc => {
                    self.input = None;
                    self.set("cancelled", Tone::Info);
                }
                KeyCode::Enter => {
                    let buffer = self.input.take().map(|i| i.buffer).unwrap_or_default();
                    return Intent::Schedule(buffer);
                }
                KeyCode::Backspace => {
                    if let Some(input) = self.input.as_mut() {
                        input.buffer.pop();
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some(input) = self.input.as_mut() {
                        input.buffer.push(ch);
                    }
                }
                _ => {}
            }
            return Intent::None;
        }

        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.posts.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Char('g') => self.selected = 0,
            KeyCode::Char('G') => self.selected = self.posts.len().saturating_sub(1),
            KeyCode::Char('r') => {
                if let Err(err) = self.reload() {
                    self.set(format!("{err:#}"), Tone::Error);
                }
            }
            KeyCode::Char('c') => return Intent::Check,
            KeyCode::Char('s') => return Intent::Send { force: false },
            KeyCode::Char('S') => return Intent::Send { force: true },
            KeyCode::Char('t') => {
                self.input = Some(Input {
                    prompt: "when? (2026-11-02 09:10 / +2h) — Enter to schedule".into(),
                    buffer: String::new(),
                });
            }
            KeyCode::Char('x') => return Intent::Cancel,
            KeyCode::Char('e') => return Intent::Edit,
            KeyCode::Char('y') => return Intent::Copy,
            KeyCode::Char('o') => return Intent::Open,
            _ => {}
        }
        Intent::None
    }

    fn act(&mut self, intent: Intent, terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
        let Some(post) = self.current().cloned() else {
            self.set("empty queue", Tone::Warn);
            return Ok(());
        };

        match intent {
            Intent::None => {}
            Intent::Check => {
                let issues = check::run(&post, &self.queue.dir);
                if issues.is_empty() {
                    self.set(format!("{}: clean ({} chars)", post.id, post.body_chars()), Tone::Info);
                } else {
                    let fails = issues.iter().filter(|i| i.is_fail()).count();
                    let summary = issues
                        .iter()
                        .map(|i| {
                            format!(
                                "{}{}: {}",
                                if i.is_fail() { "FAIL " } else { "" },
                                i.rule,
                                i.message
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" · ");
                    self.set(
                        summary,
                        if fails > 0 { Tone::Error } else { Tone::Warn },
                    );
                }
            }
            Intent::Send { force } => {
                self.set(format!("publishing {}…", post.id), Tone::Info);
                terminal.draw(|frame| self.draw(frame))?;
                match publish::execute(self.cfg, &self.queue, &post.id, force, false) {
                    Ok(report) => {
                        let tone = match report.state_after.as_str() {
                            "posted" => Tone::Info,
                            "unknown" => Tone::Warn,
                            _ => Tone::Error,
                        };
                        let text = match report.state_after.as_str() {
                            "posted" => format!("published {} → {}", report.id, report.url),
                            other => format!("{}: {} {}", other, report.id, report.message),
                        };
                        self.set(text, tone);
                    }
                    Err(err) => self.set(format!("{err:#}"), Tone::Error),
                }
                self.reload()?;
            }
            Intent::Schedule(when) => {
                match state::parse_instant(&when) {
                    Ok(instant) => {
                        let mut fresh = self.queue.get(&post.id)?;
                        fresh.state = "scheduled".into();
                        fresh.at_raw = instant.to_rfc3339();
                        fresh.at = Some(instant.with_timezone(&chrono::Utc));
                        self.queue.save(&fresh)?;
                        self.set(
                            format!(
                                "{} scheduled {} ({})",
                                post.id,
                                instant.format("%Y-%m-%d %H:%M"),
                                fresh.mode
                            ),
                            Tone::Info,
                        );
                        self.reload()?;
                    }
                    Err(err) => self.set(format!("{err:#}"), Tone::Error),
                }
            }
            Intent::Cancel => {
                let mut fresh = self.queue.get(&post.id)?;
                if fresh.state == "posted" {
                    self.set("already published", Tone::Warn);
                } else {
                    fresh.state = "cancelled".into();
                    self.queue.save(&fresh)?;
                    self.set(format!("{} cancelled", post.id), Tone::Info);
                    self.reload()?;
                }
            }
            Intent::Edit => {
                let editor = std::env::var("VISUAL")
                    .or_else(|_| std::env::var("EDITOR"))
                    .unwrap_or_else(|_| "vi".into());
                disable_raw_mode()?;
                execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                let status = std::process::Command::new(&editor).arg(&post.path).status();
                execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                enable_raw_mode()?;
                terminal.clear()?;
                match status {
                    Ok(_) => {
                        self.reload()?;
                        self.set(format!("edited {}", post.path.display()), Tone::Info);
                    }
                    Err(err) => self.set(format!("editor failed: {err}"), Tone::Error),
                }
            }
            Intent::Copy => {
                let text = publish::compose_commentary(&post);
                match std::process::Command::new("/usr/bin/pbcopy")
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .and_then(|mut child| {
                        use std::io::Write;
                        child
                            .stdin
                            .as_mut()
                            .map(|stdin| stdin.write_all(text.as_bytes()))
                            .unwrap_or(Ok(()))?;
                        child.wait()
                    }) {
                    Ok(_) => self.set("post copied to the clipboard (fallback path)", Tone::Info),
                    Err(err) => self.set(format!("pbcopy failed: {err}"), Tone::Error),
                }
            }
            Intent::Open => {
                if post.urn.is_empty() {
                    self.set("no URL yet — not published", Tone::Warn);
                } else {
                    let url = crate::api::public_url(&post.urn);
                    let _ = std::process::Command::new("/usr/bin/open").arg(&url).status();
                    self.set(url, Tone::Info);
                }
            }
        }
        Ok(())
    }

    fn set(&mut self, message: impl Into<String>, tone: Tone) {
        self.message = message.into();
        self.tone = tone;
    }

    fn draw(&self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(4)])
            .split(frame.area());

        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(chunks[0]);

        let open = self.posts.iter().filter(|p| p.is_open()).count();
        let title = format!(
            " queue · {} posts · {} open · {} ",
            self.posts.len(),
            open,
            self.cfg.queue.display()
        );

        let items: Vec<ListItem> = self
            .posts
            .iter()
            .map(|post| {
                let when = match post.at {
                    Some(at) => at
                        .with_timezone(&chrono::Local)
                        .format("%m-%d %H:%M")
                        .to_string(),
                    None => String::new(),
                };
                let media = if post.media.is_empty() {
                    "   ".to_string()
                } else {
                    format!("▣{} ", post.media.len())
                };
                let line = format!("{:<10}{}{:<10} {}", post.state, media, when, post.id);
                let style = match post.state.as_str() {
                    "posted" => Style::default().fg(Color::Green),
                    "failed" => Style::default().fg(Color::Red),
                    "unknown" | "attempting" => Style::default().fg(Color::Magenta),
                    "scheduled" => Style::default().fg(Color::Yellow),
                    "cancelled" => Style::default().fg(Color::DarkGray),
                    _ => Style::default(),
                };
                ListItem::new(line).style(style)
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("▸ ");
        let mut list_state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, columns[0], &mut list_state);

        let detail = match self.current() {
            Some(post) => {
                let issues = check::run(post, &self.queue.dir);
                let mut lines: Vec<Line> = Vec::new();
                lines.push(Line::from(vec![
                    Span::styled(post.id.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(format!("  {} · {} · {} chars", post.state, post.mode, post.body_chars())),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("artifact  {}", post.artifact),
                    Style::default().fg(Color::DarkGray),
                )));
                if !post.urn.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("url       {}", crate::api::public_url(&post.urn)),
                        Style::default().fg(Color::Green),
                    )));
                }
                if !post.media.is_empty() {
                    let missing = post
                        .media
                        .iter()
                        .filter(|item| !self.queue.dir.join(item).exists())
                        .count();
                    let style = if missing > 0 {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::Cyan)
                    };
                    lines.push(Line::from(Span::styled(
                        format!(
                            "media     {} image{} — {}",
                            post.media.len(),
                            if post.media.len() == 1 { "" } else { "s" },
                            post.media.join(", ")
                        ),
                        style,
                    )));
                }
                if !post.note.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("note      {}", post.note),
                        Style::default().fg(Color::Magenta),
                    )));
                }
                lines.push(Line::from(""));
                for line in post.hook.lines() {
                    lines.push(Line::from(Span::styled(
                        line.to_string(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )));
                }
                lines.push(Line::from(""));
                for line in post.body.lines() {
                    lines.push(Line::from(line.to_string()));
                }
                if !issues.is_empty() {
                    lines.push(Line::from(""));
                    for issue in &issues {
                        let style = if issue.is_fail() {
                            Style::default().fg(Color::Red)
                        } else {
                            Style::default().fg(Color::Yellow)
                        };
                        lines.push(Line::from(Span::styled(
                            format!(
                                "{} {}: {}",
                                if issue.is_fail() { "FAIL" } else { "warn" },
                                issue.rule,
                                issue.message
                            ),
                            style,
                        )));
                    }
                }
                Paragraph::new(lines)
                    .block(Block::default().borders(Borders::ALL).title(" draft "))
                    .wrap(Wrap { trim: false })
            }
            None => Paragraph::new("(empty queue — `mk-feed add` or an agent writes a draft)")
                .block(Block::default().borders(Borders::ALL).title(" draft ")),
        };
        frame.render_widget(detail, columns[1]);

        let footer: Vec<Line> = match &self.input {
            Some(input) => vec![Line::from(Span::styled(
                format!(" {} {}", input.prompt, input.buffer),
                Style::default().fg(Color::Cyan),
            ))],
            None => {
                let tone_style = match self.tone {
                    Tone::Info => Style::default().fg(Color::Green),
                    Tone::Warn => Style::default().fg(Color::Yellow),
                    Tone::Error => Style::default().fg(Color::Red),
                };
                vec![
                    Line::from(Span::styled(format!(" {}", self.message), tone_style)),
                    Line::from(Span::styled(
                        " s send · S force · t schedule · c check · e edit · x cancel · y copy · o open · r reload · q quit",
                        Style::default().fg(Color::DarkGray),
                    )),
                ]
            }
        };
        frame.render_widget(
            Paragraph::new(footer).block(Block::default().borders(Borders::ALL)),
            chunks[1],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn sandbox(name: &str) -> (crate::config::Config, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("mk-feed-tui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("01.png"), b"not really a png").unwrap();
        std::fs::write(
            dir.join("with-images.md"),
            "---\nid: \"with-images\"\nhook: \"un hook con imagenes\"\nartifact: \"https://example.com/c/2\"\n\
             state: \"scheduled\"\nmode: \"confirm\"\nat: \"2026-10-29T09:10:00-05:00\"\nvisibility: \"PUBLIC\"\n\
             media:\n  - \"01.png\"\n  - \"02-missing.png\"\nurn: \"\"\nposted_at: \"\"\nnote: \"\"\n---\nCuerpo con 7 cifras\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("un-draft.md"),
            "---\nid: \"un-draft\"\nhook: \"un hook discutible\"\nartifact: \"https://example.com/c/1\"\n\
             state: \"ready\"\nmode: \"confirm\"\nat: \"\"\nvisibility: \"PUBLIC\"\nmedia: []\n\
             urn: \"\"\nposted_at: \"\"\nnote: \"\"\n---\nCuerpo 42\n",
        )
        .unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.queue = dir.clone();
        (cfg, dir)
    }

    fn render(cfg: &crate::config::Config) -> String {
        let mut app = App {
            cfg,
            queue: Queue::open(&cfg.queue).unwrap(),
            posts: Vec::new(),
            selected: 0,
            message: "ready".into(),
            tone: Tone::Info,
            input: None,
            quit: false,
        };
        app.reload().unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer.content.iter().map(|cell| cell.symbol()).collect()
    }

    #[test]
    fn draws_the_queue_the_draft_and_the_keys() {
        let (cfg, _dir) = sandbox("draw");
        let screen = render(&cfg);
        assert!(screen.contains("queue"), "the list panel is titled");
        assert!(screen.contains("un-draft"), "the draft appears in the list");
        // The scheduled post sorts first, so it is the one previewed.
        assert!(screen.contains("un hook con imagenes"), "the selected hook is previewed");
        assert!(screen.contains("s send"), "the key hints are visible");
        assert!(screen.contains("FAIL") || screen.contains("warn") || screen.contains("artifact"));
    }

    #[test]
    fn media_is_visible_in_the_queue_and_the_detail() {
        let (cfg, _dir) = sandbox("media");
        let screen = render(&cfg);
        assert!(screen.contains("▣2"), "the list marks a post with two images: {screen}");
        assert!(
            screen.contains("02-missing.png"),
            "the detail lists the filenames in order"
        );
        assert!(screen.contains("media"), "there is a media line");
    }

    #[test]
    fn an_empty_queue_says_so() {
        let mut cfg = crate::config::Config::default();
        let dir = std::env::temp_dir().join(format!("mk-feed-tui-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        cfg.queue = dir;
        let screen = render(&cfg);
        assert!(screen.contains("empty queue"), "{screen}");
    }
}
