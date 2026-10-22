//! The optional right column, with the same chrome and width as Spaces.
use crate::{
    app::AppState,
    tasks::{Mark, Row},
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Narrowest a pane may get before the open column folds itself. Twenty cells
/// still fits a prompt line and a short status, anything less is unusable.
const MIN_PANE_WIDTH: u16 = 20;

/// Open count, whether any row is running, and rows waiting on the user.
fn summary(app: &AppState) -> (usize, bool, usize) {
    app.tasks.tasks.as_ref().map_or((0, false, 0), |tasks| {
        tasks
            .rows
            .iter()
            .fold((0, false, 0), |(count, running, waiting), row| {
                (
                    count + usize::from(row.mark != Mark::Done),
                    running || row.mark == Mark::Run,
                    waiting + usize::from(row.mark == Mark::Approve),
                )
            })
    })
}

/// Match the one-cell folded Spaces sidebar, independent of task counts.
fn folded_width(available: u16) -> u16 {
    1.min(available.saturating_sub(MIN_PANE_WIDTH))
}

/// Counts read downward when they need more than one digit. The waiting mark
/// and its count share the accent, while the open count stays muted.
fn folded_lines(app: &AppState) -> Vec<Line<'static>> {
    let (count, running, waiting) = summary(app);
    let accent = Style::default().fg(app.palette.accent);
    let mut lines = vec![Line::from(Span::styled(
        "‹",
        accent.add_modifier(Modifier::BOLD),
    ))];
    for digit in count.to_string().chars() {
        lines.push(Line::from(Span::styled(
            digit.to_string(),
            Style::default().fg(app.palette.overlay0),
        )));
    }
    if waiting > 0 {
        lines.push(Line::from(Span::styled("◆", accent)));
        for digit in waiting.to_string().chars() {
            lines.push(Line::from(Span::styled(digit.to_string(), accent)));
        }
    }
    if running {
        lines.push(Line::from(Span::styled(
            super::spinner_frame(app.spinner_tick),
            Style::default().fg(app.palette.green),
        )));
    }
    lines
}

fn desired_width(app: &AppState) -> u16 {
    app.sidebar_width
        .clamp(app.sidebar_min_width, app.sidebar_max_width)
}

pub(super) fn column_width(app: &AppState, available: u16) -> u16 {
    if !app.tasks.configured || available <= MIN_PANE_WIDTH {
        return 0;
    }
    if app.tasks_collapsed {
        return folded_width(available);
    }
    // An open column keeps its full width, the panes shrink to make room.
    let desired = desired_width(app);
    let remaining = available.saturating_sub(desired);
    let panes_fit = app
        .active
        .and_then(|i| app.workspaces.get(i))
        .is_none_or(|ws| {
            ws.zoomed
                || app.agent_peek.is_some()
                || ws
                    .layout
                    .panes(Rect::new(0, 0, remaining, 40))
                    .iter()
                    .all(|pane| pane.rect.width >= MIN_PANE_WIDTH)
        });
    if remaining >= MIN_PANE_WIDTH && panes_fit {
        desired
    } else {
        folded_width(available)
    }
}

fn folded(app: &AppState, area: Rect) -> bool {
    app.tasks_collapsed || area.width < desired_width(app)
}

pub(crate) fn has_animation(app: &AppState) -> bool {
    let area = app.view.tasks_rect;
    if area.is_empty() {
        return false;
    }
    if folded(app, area) {
        let running = summary(app).1;
        return running && area.height as usize >= folded_lines(app).len();
    }
    app.tasks.tasks.as_ref().is_some_and(|tasks| {
        tasks
            .rows
            .iter()
            .filter(|r| r.mark != Mark::Done)
            .take(area.height.saturating_sub(1) as usize)
            .any(|r| r.mark == Mark::Run)
    })
}

pub(crate) fn mouse(app: &mut AppState, event: MouseEvent) -> bool {
    if !matches!(
        app.mode,
        crate::app::Mode::Terminal
            | crate::app::Mode::Navigate
            | crate::app::Mode::Resize
            | crate::app::Mode::Composer
            | crate::app::Mode::PlayerInput
    ) {
        return false;
    }
    let area = app.view.tasks_rect;
    if area.is_empty() || !area.contains((event.column, event.row).into()) {
        return false;
    }
    if (event.row == area.y || folded(app, area))
        && matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
    {
        app.toggle_shown(&crate::app::view::Fold::Tasks);
    }
    true
}

fn fit(text: &str, width: usize) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.width() <= width {
        return clean;
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    clean
        .chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used < width
        })
        .chain(std::iter::once('…'))
        .collect()
}

fn row_line(app: &AppState, row: &Row, width: usize) -> Line<'static> {
    let dim = Style::default().fg(app.palette.overlay0);
    let normal = Style::default().fg(app.palette.text);
    let (mark, color) = match row.mark {
        // A single animation source keeps task and agent working marks in sync.
        Mark::Run => (super::spinner_frame(app.spinner_tick), app.palette.green),
        Mark::Approve => ("◆", app.palette.accent),
        Mark::Ready => ("○", app.palette.text),
        Mark::Wait => ("·", app.palette.overlay0),
        Mark::Stuck => ("!", app.palette.red),
        Mark::Done => ("✓", app.palette.overlay0),
    };
    let style = match row.mark {
        Mark::Done | Mark::Wait => dim,
        Mark::Approve => Style::default().fg(color),
        _ => normal,
    };
    let room = width.saturating_sub(3);
    // Long owners cannot consume the title. Both fields measure terminal cells.
    let who = fit(&row.who, room / 3);
    let who_gap = usize::from(!who.is_empty());
    let title = fit(&row.title, room.saturating_sub(who.width() + who_gap));
    let gap = room.saturating_sub(title.width() + who.width());
    Line::from(vec![
        Span::raw(" "),
        Span::styled(mark, Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(title, style),
        Span::raw(" ".repeat(gap)),
        Span::styled(who, dim),
    ])
}

pub(super) fn render(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.is_empty() {
        return;
    }
    let dim = Style::default().fg(app.palette.overlay0);
    let divider = Style::default().fg(app.palette.surface_dim);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &mut frame.buffer_mut()[(x, y)];
            cell.set_symbol(" ").set_style(Style::default());
        }
        if area.width > 1 {
            frame.buffer_mut()[(area.x, y)]
                .set_symbol("│")
                .set_style(divider);
        }
    }
    if folded(app, area) {
        frame.render_widget(Paragraph::new(folded_lines(app)), area);
        return;
    }
    let content = Rect::new(area.x + 1, area.y, area.width - 1, area.height);
    frame.render_widget(
        Paragraph::new(" tasks ▾").style(dim),
        Rect::new(content.x, content.y, content.width, 1),
    );
    let mut y = area.y + 1;
    if let Some(tasks) = &app.tasks.tasks {
        for row in tasks.rows.iter().filter(|r| r.mark != Mark::Done) {
            if y >= area.bottom() {
                break;
            }
            frame.render_widget(
                Paragraph::new(row_line(app, row, content.width as usize)),
                Rect::new(content.x, y, content.width, 1),
            );
            y += 1;
        }
        let done_count = tasks.rows.iter().filter(|r| r.mark == Mark::Done).count();
        if done_count > 0 && y < area.bottom() {
            frame.render_widget(
                Paragraph::new("─".repeat(content.width as usize)).style(divider),
                Rect::new(content.x, y, content.width, 1),
            );
            y += 1;
            for row in tasks
                .rows
                .iter()
                .filter(|r| r.mark == Mark::Done)
                .skip(done_count.saturating_sub(3))
            {
                if y >= area.bottom() {
                    break;
                }
                frame.render_widget(
                    Paragraph::new(row_line(app, row, content.width as usize)),
                    Rect::new(content.x, y, content.width, 1),
                );
                y += 1;
            }
        }
    } else if let Some(error) = &app.tasks.error {
        if y < area.bottom() {
            frame.render_widget(
                Paragraph::new(fit(error, content.width as usize)).style(dim),
                Rect::new(content.x, y, content.width, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        tasks::{Tasks, TasksSnapshot},
        workspace::Workspace,
    };
    use ratatui::style::Modifier;
    fn state() -> AppState {
        let mut app = AppState::test_new();
        app.workspaces.push(Workspace::test_new("example"));
        app.active = Some(0);
        app.tasks = TasksSnapshot {
            configured: true,
            tasks: Some(Tasks {
                rows: [
                    Mark::Run,
                    Mark::Approve,
                    Mark::Ready,
                    Mark::Wait,
                    Mark::Stuck,
                    Mark::Done,
                    Mark::Done,
                    Mark::Done,
                    Mark::Done,
                ]
                .into_iter()
                .enumerate()
                .map(|(i, mark)| Row {
                    mark,
                    title: format!("Task {i}"),
                    who: "owner".into(),
                })
                .collect(),
            }),
            error: None,
        };
        app
    }
    fn draw(app: &mut AppState, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let area = Rect::new(0, 0, width, height);
        crate::ui::compute_view(app, area);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| crate::ui::render(app, f)).unwrap();
        terminal.backend().buffer().clone()
    }
    fn text(buf: &ratatui::buffer::Buffer) -> String {
        buf.content.iter().map(|c| c.symbol()).collect()
    }
    fn strip_text(buf: &ratatui::buffer::Buffer, area: Rect) -> String {
        (area.y..area.bottom())
            .map(|y| buf[(area.x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn folded_tasks_match_sidebar_and_return_width_to_five_panes() {
        let mut app = state();
        app.sidebar_collapsed = true;
        for direction in [
            ratatui::layout::Direction::Vertical,
            ratatui::layout::Direction::Horizontal,
            ratatui::layout::Direction::Horizontal,
            ratatui::layout::Direction::Vertical,
        ] {
            app.workspaces[0].test_split(direction);
        }
        draw(&mut app, 240, 40);
        let open_tasks_width = app.view.tasks_rect.width;
        let open_panes_width = app.view.terminal_area.width;
        app.tasks_collapsed = true;
        draw(&mut app, 240, 40);
        assert_eq!(app.view.pane_infos.len(), 5);
        assert_eq!(app.view.tasks_rect.width, app.view.sidebar_rect.width);
        assert_eq!(app.view.tasks_rect.width, 1);
        assert_eq!(app.view.tasks_rect.right(), 240);
        assert_eq!(app.view.terminal_area.right(), app.view.tasks_rect.x);
        assert_eq!(
            app.view.terminal_area.width,
            open_panes_width + open_tasks_width - 1
        );
        assert_eq!(column_width(&app, MIN_PANE_WIDTH), 0);
        assert_eq!(column_width(&app, MIN_PANE_WIDTH + 1), 1);
    }

    #[test]
    fn folded_tasks_click_target_covers_only_the_strip() {
        for at_bottom in [false, true] {
            let mut app = state();
            app.tasks_collapsed = true;
            draw(&mut app, 160, 40);
            let area = app.view.tasks_rect;
            let event = |column, row| MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
            assert!(!mouse(&mut app, event(area.x - 1, area.y)));
            assert!(!mouse(&mut app, event(area.right(), area.y)));
            assert!(!mouse(&mut app, event(area.x, area.bottom())));
            assert!(app.tasks_collapsed);
            let row = if at_bottom { area.bottom() - 1 } else { area.y };
            assert!(mouse(&mut app, event(area.x, row)));
            assert!(!app.tasks_collapsed);
            assert!(text(&draw(&mut app, 160, 40)).contains("Task 0"));
        }
    }

    #[test]
    fn folded_tasks_preserve_large_counts_and_clip_short_windows() {
        let mut app = state();
        let rows = &mut app.tasks.tasks.as_mut().unwrap().rows;
        rows.extend((0..12).map(|i| Row {
            mark: Mark::Approve,
            title: format!("Extra {i}"),
            who: String::new(),
        }));
        app.tasks_collapsed = true;
        let buf = draw(&mut app, 160, 40);
        assert_eq!(strip_text(&buf, app.view.tasks_rect), "‹17◆13◐");
        assert_eq!(app.view.tasks_rect.width, 1);
        assert!(has_animation(&app));
        app.view.tasks_rect.height = 6;
        assert!(!has_animation(&app));
        for height in 1..8 {
            draw(&mut app, 160, height);
        }
    }

    #[test]
    fn tasks_column_stays_open_until_panes_would_be_unusable() {
        let mut app = state();
        draw(&mut app, 160, 40);
        assert_eq!(app.view.tasks_rect.width, app.view.sidebar_rect.width);
        assert_eq!(app.view.tasks_rect.right(), 160);
        // A narrow window still shows the full column, the pane shrinks.
        draw(&mut app, 100, 40);
        assert_eq!(app.view.tasks_rect.width, app.view.sidebar_rect.width);
        assert!(app.view.terminal_area.width >= MIN_PANE_WIDTH);
        // Three panes side by side used to fold it at 160 columns.
        app.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        draw(&mut app, 160, 40);
        assert_eq!(app.view.tasks_rect.width, app.view.sidebar_rect.width);
        assert!(app
            .view
            .pane_infos
            .iter()
            .all(|p| p.rect.width >= MIN_PANE_WIDTH));
        // Only when a pane would drop under the floor does it fold.
        draw(&mut app, 130, 40);
        assert!(app.view.tasks_rect.width < app.view.sidebar_rect.width);
        assert_eq!(app.view.tasks_rect.width, 1);
        draw(&mut app, 240, 40);
        assert_eq!(app.view.tasks_rect.width, app.view.sidebar_rect.width);
        app.tasks = TasksSnapshot::default();
        draw(&mut app, 160, 40);
        assert!(app.view.tasks_rect.is_empty());
    }
    #[test]
    fn tasks_column_rows_done_limit_error_and_click_fold() {
        let mut app = state();
        let buf = draw(&mut app, 160, 40);
        let rendered = text(&buf);
        for title in [
            "Task 0", "Task 1", "Task 2", "Task 3", "Task 4", "Task 6", "Task 7", "Task 8",
        ] {
            assert!(rendered.contains(title), "{title}");
        }
        assert!(!rendered.contains("Task 5"));
        assert!(has_animation(&app));
        let area = app.view.tasks_rect;
        assert!(mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x + 2,
                row: area.y,
                modifiers: crossterm::event::KeyModifiers::NONE
            }
        ));
        let folded = draw(&mut app, 160, 40);
        assert_eq!(app.view.tasks_rect.width, 1);
        assert!(!text(&folded).contains("Task 0"));
        assert!(has_animation(&app));
        assert_eq!(strip_text(&folded, app.view.tasks_rect), "‹5◆1◐");
        assert_eq!(folded[(app.view.tasks_rect.x, 0)].fg, app.palette.accent);
        assert!(folded[(app.view.tasks_rect.x, 0)]
            .modifier
            .contains(Modifier::BOLD));
        let area = app.view.tasks_rect;
        assert!(mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: crossterm::event::KeyModifiers::NONE
            }
        ));
        assert!(text(&draw(&mut app, 160, 40)).contains("Task 0"));
        app.tasks.tasks = None;
        app.tasks.error = Some("Invalid tasks file".into());
        let buf = draw(&mut app, 160, 40);
        assert!(text(&buf).contains("Invalid tasks file"));
        assert_eq!(buf[(app.view.tasks_rect.x + 1, 1)].fg, app.palette.overlay0);
        assert_eq!(fit("一二三", 4), "一…");
        assert_eq!(fit("a\nb", 8), "a b");
        let line = row_line(
            &app,
            &Row {
                mark: Mark::Ready,
                title: "wide 一 title".into(),
                who: "owner".into(),
            },
            25,
        );
        assert_eq!(line.width(), 25);
    }

    #[test]
    fn tasks_column_prefix_key_and_quiet_summary_are_visible() {
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.state = state();
        let open = draw(&mut app.state, 160, 40);
        assert!(text(&open).contains("Task 0"));
        app.state.mode = crate::app::Mode::Prefix;
        app.handle_prefix_key(crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('t'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.state.tasks_collapsed);
        let strip = draw(&mut app.state, 160, 40);
        assert!(!text(&strip).contains("Task 0"));
        let area = app.state.view.tasks_rect;
        assert_eq!(strip_text(&strip, area), "‹5◆1◐");
        app.state.mode = crate::app::Mode::Prefix;
        app.handle_prefix_key(crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('t'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!app.state.tasks_collapsed);
        assert!(text(&draw(&mut app.state, 160, 40)).contains("Task 0"));
        app.state.tasks_collapsed = true;
        app.state
            .tasks
            .tasks
            .as_mut()
            .unwrap()
            .rows
            .iter_mut()
            .for_each(|r| r.mark = Mark::Done);
        let quiet = draw(&mut app.state, 160, 40);
        let area = app.state.view.tasks_rect;
        assert_eq!(strip_text(&quiet, area), "‹0");
        assert!(!has_animation(&app.state));
    }
    #[test]
    fn tasks_column_render_example() {
        let mut app = state();
        let Ok(output) = std::env::var("HERDR_TASKS_RENDER_OUTPUT") else {
            return;
        };
        let mut watcher = crate::tasks::TasksWatcher::new(crate::tasks::configured_path(None));
        watcher.poll(std::time::Instant::now(), &mut app.tasks);
        assert!(app.tasks.error.is_none());
        app.mode = crate::app::Mode::Terminal;
        app.workspaces[0].cached_git_branch = None;
        app.ensure_test_terminals();
        for terminal in app.terminals.values_mut() {
            terminal.cwd = std::env::temp_dir().join("synthetic-project");
            terminal.set_agent_name("builder".into());
            terminal.set_detected_state(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Working,
            );
            terminal.session_title = Some("Prepare release".into());
        }
        // Wider synthetic sidebar shows the external dispatcher's current titles.
        app.sidebar_width = 42;
        app.sidebar_max_width = 50;
        let mut cells = Vec::new();
        for (collapsed, caption) in [
            (false, "OPEN — tasks header folds"),
            (true, "FOLDED — prefix+t opens"),
        ] {
            app.tasks_collapsed = collapsed;
            let buf = draw(&mut app, 180, 24);
            if collapsed {
                assert!(!text(&buf).contains("tasks ▾"));
            } else {
                assert!(text(&buf).contains("tasks ▾"));
            }
            let mut caption_row =
                vec![serde_json::json!({"text":" ","fg":"Rgb(205, 214, 244)","bold":true}); 180];
            for (x, ch) in caption.chars().enumerate() {
                caption_row[x]["text"] = serde_json::json!(ch.to_string());
            }
            cells.push(caption_row);
            for y in 0..buf.area.height {
                cells.push((0..buf.area.width).map(|x| {let c=&buf[(x,y)];serde_json::json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bold":c.modifier.contains(Modifier::BOLD)})}).collect::<Vec<_>>());
            }
        }
        std::fs::write(output, serde_json::to_vec_pretty(&cells).unwrap()).unwrap();
    }
}
