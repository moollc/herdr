//! Provider allowance editor, using the standard settings panel shell.
use super::input::SettingsAction;
use super::{App, AppState, Mode};
use crate::{allowance_guard::ProviderLimits, usage::Provider};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, style::Style, text::Line, widgets::Paragraph, Frame};

pub(crate) struct AllowanceSettings {
    pub provider: Provider,
    pub draft: ProviderLimits,
    pub selected: usize,
    pub digits: String,
    pub benches: Vec<(String, bool)>,
    pub error: String,
}
impl AllowanceSettings {
    fn values(&self) -> [u64; 8] {
        [
            self.draft.short.wind_down_pct.into(),
            self.draft.short.stop_pct.into(),
            self.draft.short.overseer_pct.into(),
            self.draft.long.wind_down_pct.into(),
            self.draft.long.stop_pct.into(),
            self.draft.long.overseer_pct.into(),
            self.draft.fast_drop_points.into(),
            self.draft.fast_window_minutes,
        ]
    }
    fn set(&mut self, value: u64) {
        let pct = value.min(100) as u8;
        match self.selected {
            1 => self.draft.short.wind_down_pct = pct,
            2 => self.draft.short.stop_pct = pct,
            3 => self.draft.short.overseer_pct = pct,
            4 => self.draft.long.wind_down_pct = pct,
            5 => self.draft.long.stop_pct = pct,
            6 => self.draft.long.overseer_pct = pct,
            7 => self.draft.fast_drop_points = pct,
            8 => self.draft.fast_window_minutes = value.min(1440),
            _ => {}
        }
    }
    fn rows(&self) -> Vec<String> {
        let short = if self.provider == Provider::Cursor {
            "Auto monthly"
        } else {
            "Five-hour"
        };
        let long = if self.provider == Provider::Cursor {
            "Named monthly"
        } else {
            "Weekly"
        };
        let mut rows = vec![
            format!("Guard: {}", if self.draft.enabled { "on" } else { "off" }),
            format!(
                "{short} · Wind-down at: {}%",
                self.draft.short.wind_down_pct
            ),
            format!("{short} · Stop at: {}%", self.draft.short.stop_pct),
            format!(
                "{short} · Overseer keeps going until: {}%",
                self.draft.short.overseer_pct
            ),
            format!("{long} · Wind-down at: {}%", self.draft.long.wind_down_pct),
            format!("{long} · Stop at: {}%", self.draft.long.stop_pct),
            format!(
                "{long} · Overseer keeps going until: {}%",
                self.draft.long.overseer_pct
            ),
            format!("Fast fall: {} points", self.draft.fast_drop_points),
            format!(
                "Fast fall within: {} minutes",
                self.draft.fast_window_minutes
            ),
        ];
        rows.extend(
            self.benches
                .iter()
                .map(|(n, b)| format!("[{}] {n} may work", if *b { " " } else { "x" })),
        );
        rows
    }
    fn toggle(&mut self) {
        if self.selected == 0 {
            self.draft.enabled = !self.draft.enabled;
        }
        if let Some((_, b)) = self
            .selected
            .checked_sub(9)
            .and_then(|i| self.benches.get_mut(i))
        {
            *b = !*b;
        }
    }
}
fn popup(area: Rect) -> Option<Rect> {
    crate::ui::widgets::centered_popup_rect(area, 70, 26)
}
impl AppState {
    pub(crate) fn open_allowance_settings(&mut self, provider: Provider) {
        #[cfg(not(test))]
        let benched =
            std::fs::read_to_string(crate::allowance_guard::benched_path()).unwrap_or_default();
        #[cfg(test)]
        let benched = String::new();
        let mut names: Vec<_> = self
            .terminals
            .iter()
            .filter(|(_, t)| {
                t.effective_known_agent()
                    .and_then(crate::ui::usage_bar::provider_for_agent)
                    == Some(provider)
            })
            .filter_map(|(id, _)| crate::pane_names::effective_name(&self.terminals, id))
            .collect();
        names.sort();
        names.dedup();
        self.allowance_settings = Some(AllowanceSettings {
            provider,
            draft: self.allowance_guard.config.effective(provider),
            selected: 0,
            digits: String::new(),
            benches: names
                .into_iter()
                .map(|n| {
                    let b = benched.lines().any(|l| l.trim() == n);
                    (n, b)
                })
                .collect(),
            error: String::new(),
        });
        self.mode = Mode::Settings;
    }
    fn allowance_apply(&mut self) -> Option<SettingsAction> {
        let panel = self.allowance_settings.as_mut()?;
        if let Err(error) = panel.draft.validate() {
            panel.error = error.into();
            return None;
        }
        let action = SettingsAction::SaveAllowance(
            panel.provider,
            panel.draft.clone(),
            panel.benches.clone(),
        );
        self.allowance_settings = None;
        self.mode = Mode::Terminal;
        Some(action)
    }
    pub(crate) fn allowance_key(&mut self, key: KeyEvent) -> Option<SettingsAction> {
        let panel = self.allowance_settings.as_mut()?;
        match key.code {
            KeyCode::Esc => {
                self.allowance_settings = None;
                self.mode = Mode::Terminal;
            }
            KeyCode::Char('s') => return self.allowance_apply(),
            KeyCode::Up => {
                panel.selected = panel.selected.saturating_sub(1);
                panel.digits.clear();
            }
            KeyCode::Down | KeyCode::Tab => {
                panel.selected = (panel.selected + 1).min(panel.rows().len() - 1);
                panel.digits.clear();
            }
            KeyCode::Char(c) if c.is_ascii_digit() && (1..=8).contains(&panel.selected) => {
                if panel.digits.len() < 4 {
                    panel.digits.push(c);
                    if let Ok(v) = panel.digits.parse() {
                        panel.set(v);
                    }
                }
            }
            KeyCode::Backspace => {
                panel.digits.pop();
                panel.set(panel.digits.parse().unwrap_or(0));
            }
            KeyCode::Right | KeyCode::Char('+') if (1..=8).contains(&panel.selected) => {
                panel.set(panel.values()[panel.selected - 1] + 1);
                panel.digits.clear();
            }
            KeyCode::Left | KeyCode::Char('-') if (1..=8).contains(&panel.selected) => {
                panel.set(panel.values()[panel.selected - 1].saturating_sub(1));
                panel.digits.clear();
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                panel.toggle();
                panel.digits.clear();
            }
            _ => {}
        }
        None
    }
    pub(crate) fn allowance_mouse(&mut self, mouse: MouseEvent) -> Option<SettingsAction> {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        let area = popup(self.screen_rect())?;
        if !area.contains((mouse.column, mouse.row).into()) {
            return None;
        }
        if mouse.row == area.bottom() - 3 {
            if mouse.column < area.x + area.width / 2 {
                return self.allowance_apply();
            }
            self.allowance_settings = None;
            self.mode = Mode::Terminal;
            return None;
        }
        let panel = self.allowance_settings.as_mut()?;
        let idx = mouse.row.saturating_sub(area.y + 3) as usize + panel.selected.saturating_sub(15);
        if mouse.column > area.x
            && mouse.column < area.right()
            && mouse.row >= area.y + 3
            && idx < panel.rows().len()
        {
            panel.selected = idx;
            panel.digits.clear();
            panel.toggle();
        }
        None
    }
}
pub(crate) fn render(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(panel) = &app.allowance_settings else {
        return;
    };
    let Some(rect) = popup(area) else {
        return;
    };
    let p = &app.palette;
    crate::ui::dim_background(frame, area);
    let Some(inner) = crate::ui::widgets::render_panel_shell(frame, rect, p.accent, p.panel_bg)
    else {
        return;
    };
    let title = format!(
        " {} allowance settings",
        crate::ui::usage_bar::provider_name(panel.provider)
    );
    frame.render_widget(
        Paragraph::new(title).style(Style::default().fg(p.text)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(" Select a number, type or use ←/→. Click switches.")
            .style(Style::default().fg(p.overlay0)),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
    for (idx, row) in panel
        .rows()
        .into_iter()
        .skip(panel.selected.saturating_sub(15))
        .take(16)
        .enumerate()
    {
        let style = if idx == panel.selected {
            Style::default().fg(p.panel_bg).bg(p.accent)
        } else {
            Style::default().fg(p.text)
        };
        frame.render_widget(
            Paragraph::new(Line::from(format!(" {row}"))).style(style),
            Rect::new(inner.x, inner.y + 2 + idx as u16, inner.width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(panel.error.as_str()).style(Style::default().fg(p.accent)),
        Rect::new(inner.x, rect.bottom() - 5, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(" [s Save]                         [Esc Cancel]")
            .style(Style::default().fg(p.accent)),
        Rect::new(inner.x, rect.bottom() - 3, inner.width, 1),
    );
}
impl App {
    pub(crate) fn save_allowance(
        &mut self,
        provider: Provider,
        policy: ProviderLimits,
        benches: Vec<(String, bool)>,
    ) {
        if let Err(error) = policy.validate() {
            self.state.config_diagnostic = Some(error.into());
            return;
        }
        let encoded = match serde_json::to_value(&policy) {
            Ok(v) => v,
            Err(e) => {
                self.state.config_diagnostic = Some(e.to_string());
                return;
            }
        };
        let name = match provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Cursor => "cursor",
        };
        // Read the current BENCHED file at save time, preserving panes outside this panel.
        let path = crate::allowance_guard::benched_path();
        let original = match std::fs::read_to_string(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                self.state.config_diagnostic = Some(format!("Could not read pane switches: {e}"));
                return;
            }
        };
        let mut lines: Vec<String> = original.lines().map(str::to_owned).collect();
        for (name, benched) in benches {
            lines.retain(|l| l.trim() != name);
            if benched {
                lines.push(name);
            }
        }
        let updated = if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n") + "\n"
        };
        if updated != original {
            let tmp = path.with_extension("allowance-settings.tmp");
            if let Err(error) =
                std::fs::write(&tmp, &updated).and_then(|()| std::fs::rename(&tmp, &path))
            {
                self.state.config_diagnostic =
                    Some(format!("Could not save pane switches: {error}"));
                return;
            }
        }
        if self.update_config_file("allowance settings", |content| {
            let mut out = crate::config::upsert_section_bool(
                content,
                "allowance_guard",
                name,
                policy.enabled,
            );
            for key in ["enabled", "fast_drop_points", "fast_window_minutes"] {
                out = crate::config::upsert_section_value(
                    &out,
                    &format!("allowance_guard.providers.{name}"),
                    key,
                    &encoded[key].to_string(),
                );
            }
            for window in ["short", "long"] {
                for key in ["wind_down_pct", "stop_pct", "overseer_pct"] {
                    out = crate::config::upsert_section_value(
                        &out,
                        &format!("allowance_guard.providers.{name}.{window}"),
                        key,
                        &encoded[window][key].to_string(),
                    );
                }
            }
            out
        }) {
            self.apply_config_from_disk(false);
        } else if updated != original {
            let _ = std::fs::write(&path, original);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_panel_edits_numbers_refuses_invalid_stop_and_closes() {
        for provider in [Provider::Claude, Provider::Codex, Provider::Cursor] {
            let mut app = AppState::test_new();
            app.open_allowance_settings(provider);
            assert_eq!(app.mode, Mode::Settings);
            assert_eq!(app.allowance_settings.as_ref().unwrap().provider, provider);
            app.allowance_settings.as_mut().unwrap().selected = 1;
            app.allowance_key(KeyEvent::from(KeyCode::Char('5')));
            assert_eq!(
                app.allowance_settings
                    .as_ref()
                    .unwrap()
                    .draft
                    .short
                    .wind_down_pct,
                5
            );
            app.allowance_settings
                .as_mut()
                .unwrap()
                .draft
                .short
                .overseer_pct = 4;
            app.allowance_settings
                .as_mut()
                .unwrap()
                .draft
                .short
                .stop_pct = 5;
            assert!(app
                .allowance_key(KeyEvent::from(KeyCode::Char('s')))
                .is_none());
            assert!(app
                .allowance_settings
                .as_ref()
                .unwrap()
                .error
                .contains("below"));
            app.allowance_settings
                .as_mut()
                .unwrap()
                .draft
                .short
                .stop_pct = 3;
            assert!(
                matches!(app.allowance_key(KeyEvent::from(KeyCode::Char('s'))),Some(SettingsAction::SaveAllowance(p,_,_)) if p==provider)
            );
            assert_eq!(app.mode, Mode::Terminal);
        }
    }
    #[test]
    fn panel_mouse_toggle_save_cancel_and_visible_values() {
        let mut app = AppState::test_new();
        app.open_allowance_settings(Provider::Codex);
        app.allowance_settings.as_mut().unwrap().benches = vec![("birch".into(), false)];
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(&app, f, f.area())).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("GPT allowance settings"));
        assert!(text.contains("Overseer keeps going until"));
        assert!(text.contains("Wind-down at: 5%"));
        assert!(text.contains("birch may work"));
        app.allowance_settings.as_mut().unwrap().selected = 9;
        app.allowance_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.allowance_settings.as_ref().unwrap().benches[0].1);
        app.allowance_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.allowance_settings.is_none());
    }
    #[test]
    fn each_mouse_control_selects_changes_saves_or_cancels() {
        let mut app = AppState::test_new();
        app.view.terminal_area = Rect::new(0, 0, 120, 40);
        app.open_allowance_settings(Provider::Codex);
        app.allowance_settings.as_mut().unwrap().benches = vec![("birch".into(), false)];
        let rect = popup(app.screen_rect()).unwrap();
        for idx in 0..10 {
            let event = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 2,
                row: rect.y + 3 + idx,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
            app.allowance_mouse(event);
            assert_eq!(
                app.allowance_settings.as_ref().unwrap().selected,
                usize::from(idx)
            );
            if (1..=8).contains(&idx) {
                let before =
                    app.allowance_settings.as_ref().unwrap().values()[usize::from(idx - 1)];
                app.allowance_key(KeyEvent::from(KeyCode::Right));
                assert_eq!(
                    app.allowance_settings.as_ref().unwrap().values()[usize::from(idx - 1)],
                    before + 1
                );
            }
        }
        assert!(!app.allowance_settings.as_ref().unwrap().draft.enabled);
        assert!(app.allowance_settings.as_ref().unwrap().benches[0].1);
        let action = app.allowance_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 2,
            row: rect.bottom() - 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(matches!(
            action,
            Some(SettingsAction::SaveAllowance(_, _, _))
        ));
        app.open_allowance_settings(Provider::Codex);
        app.allowance_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.right() - 2,
            row: rect.bottom() - 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(app.allowance_settings.is_none());
    }
}
