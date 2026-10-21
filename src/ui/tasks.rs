//! The optional right column, with the same chrome and width as Spaces.
use crate::{
    app::AppState,
    tasks::{Mark, Row},
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MIN_PANE_WIDTH: u16 = 60;

fn summary(app: &AppState) -> (usize, bool) {
    app.tasks.tasks.as_ref().map_or((0, false), |tasks| {
        tasks.rows.iter().fold((0, false), |(count, running), row| {
            (
                count + usize::from(row.mark != Mark::Done),
                running || row.mark == Mark::Run,
            )
        })
    })
}

fn folded_width(app: &AppState, available: u16) -> u16 {
    // Divider, count, gap and running circle. Keep pane space first.
    (summary(app).0.to_string().len() as u16 + 3).min(available.saturating_sub(MIN_PANE_WIDTH))
}

pub(super) fn column_width(app: &AppState, available: u16) -> u16 {
    if !app.tasks.configured || available <= MIN_PANE_WIDTH {
        return 0;
    }
    if app.tasks_collapsed {
        return folded_width(app, available);
    }
    let desired = app
        .sidebar_width
        .clamp(app.sidebar_min_width, app.sidebar_max_width);
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
        folded_width(app, available)
    }
}

fn folded(app: &AppState, area: Rect) -> bool {
    app.tasks_collapsed
        || area.width
            < app
                .sidebar_width
                .clamp(app.sidebar_min_width, app.sidebar_max_width)
}

pub(crate) fn has_animation(app: &AppState) -> bool {
    let area = app.view.tasks_rect;
    if area.is_empty() {
        return false;
    }
    if folded(app, area) {
        let (count, running) = summary(app);
        return running && area.width >= count.to_string().len() as u16 + 3;
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
        let (count, running) = summary(app);
        let mut spans = vec![Span::styled(count.to_string(), dim)];
        if running {
            spans.push(Span::styled(
                format!(" {}", super::spinner_frame(app.spinner_tick)),
                Style::default().fg(app.palette.green),
            ));
        }
        let content = if area.width > 1 {
            Rect::new(area.x + 1, area.y, area.width - 1, 1)
        } else {
            Rect::new(area.x, area.y, 1, 1)
        };
        frame.render_widget(Paragraph::new(Line::from(spans)), content);
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
    #[test]
    fn tasks_column_layout_folds_before_panes_shrink_and_recovers() {
        let mut app = state();
        draw(&mut app, 160, 40);
        assert_eq!(app.view.tasks_rect.width, app.view.sidebar_rect.width);
        assert_eq!(app.view.tasks_rect.right(), 160);
        assert!(app.view.terminal_area.width >= MIN_PANE_WIDTH);
        draw(&mut app, 100, 40);
        assert_eq!(app.view.tasks_rect.width, 4);
        assert!(app.view.terminal_area.width >= MIN_PANE_WIDTH);
        app.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        draw(&mut app, 160, 40);
        assert_eq!(app.view.tasks_rect.width, 4);
        draw(&mut app, 240, 40);
        assert!(app.view.tasks_rect.width > 1);
        assert!(app
            .view
            .pane_infos
            .iter()
            .all(|p| p.rect.width >= MIN_PANE_WIDTH));
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
        assert_eq!(app.view.tasks_rect.width, 4);
        assert!(!text(&folded).contains("Task 0"));
        assert!(has_animation(&app));
        let folded_header: String = (app.view.tasks_rect.x + 1..app.view.tasks_rect.right())
            .map(|x| folded[(x, 0)].symbol())
            .collect();
        assert_eq!(folded_header, "5 ◐");
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
        let header: String = (area.x + 1..area.right())
            .map(|x| strip[(x, 0)].symbol())
            .collect();
        assert_eq!(header, "5 ◐");
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
        let header: String = (area.x + 1..area.right())
            .map(|x| quiet[(x, 0)].symbol())
            .collect();
        assert_eq!(header, "0  ");
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
