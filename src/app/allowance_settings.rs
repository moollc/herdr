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
    pub spend_reading: Option<crate::usage::CursorSpend>,
    pub error: String,
}
impl AllowanceSettings {
    fn values(&self) -> Vec<u64> {
        let mut values = vec![
            self.draft.short.wind_down_pct.into(),
            self.draft.short.stop_pct.into(),
            self.draft.short.overseer_pct.into(),
            self.draft.long.wind_down_pct.into(),
            self.draft.long.stop_pct.into(),
            self.draft.long.overseer_pct.into(),
            self.draft.fast_drop_points.into(),
            self.draft.fast_window_minutes,
        ];
        if self.provider == Provider::Cursor {
            values.extend([
                0,
                self.draft.extra_spend.limits.wind_down_pct.into(),
                self.draft.extra_spend.limits.overseer_pct.into(),
                self.draft.extra_spend.limits.stop_pct.into(),
            ]);
        }
        values
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
            10 => self.draft.extra_spend.limits.wind_down_pct = pct,
            11 => self.draft.extra_spend.limits.overseer_pct = pct,
            12 => self.draft.extra_spend.limits.stop_pct = pct,
            _ => {}
        }
    }
    fn numeric(&self) -> bool {
        (1..=8).contains(&self.selected)
            || (self.provider == Provider::Cursor && (10..=12).contains(&self.selected))
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
        if self.provider == Provider::Cursor {
            let cap = &self.draft.extra_spend;
            rows.extend([
                format!(
                    "Extra spend cap: {}",
                    if cap.enabled { "on" } else { "off" }
                ),
                format!("Extra spend · Wind-down at: {}%", cap.limits.wind_down_pct),
                format!("Extra spend · Overseer until: {}%", cap.limits.overseer_pct),
                format!("Extra spend · Stop at: {}%", cap.limits.stop_pct),
                match self.spend_reading.as_ref().and_then(|r| r.left_pct()) {
                    Some(left) => format!("Extra spend: {left}% left of total (bonus + on-demand)"),
                    None => "Extra spend: unknown; refresh usage".into(),
                },
            ]);
        }
        rows
    }
    fn toggle(&mut self) {
        if self.selected == 0 {
            self.draft.enabled = !self.draft.enabled;
        }
        if self.provider == Provider::Cursor && self.selected == 9 {
            self.draft.extra_spend.enabled = !self.draft.extra_spend.enabled;
        }
    }
}
fn popup(area: Rect) -> Option<Rect> {
    crate::ui::widgets::centered_popup_rect(area, 70, 26)
}
impl AppState {
    pub(crate) fn open_allowance_settings(&mut self, provider: Provider) {
        let spend_reading = self.usage_snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .providers
                .iter()
                .find(|u| u.provider == provider)
                .filter(|u| {
                    u.problem.is_none()
                        && u.seen_at.is_some_and(|at| {
                            let now = crate::usage::now();
                            at <= now && now - at <= crate::allowance_guard::MAX_READING_AGE_SECS
                        })
                })
                .and_then(|u| u.extra_spend.clone())
        });
        self.allowance_settings = Some(AllowanceSettings {
            provider,
            draft: self.allowance_guard.config.effective(provider),
            selected: 0,
            digits: String::new(),
            spend_reading,
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
        let action = SettingsAction::SaveAllowance(panel.provider, panel.draft.clone());
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
            KeyCode::Char(c) if c.is_ascii_digit() && panel.numeric() => {
                if panel.digits.len() < 9 {
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
            KeyCode::Right | KeyCode::Char('+') if panel.numeric() => {
                panel.set(panel.values()[panel.selected - 1] + 1);
                panel.digits.clear();
            }
            KeyCode::Left | KeyCode::Char('-') if panel.numeric() => {
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
        Paragraph::new(" Type percentages left, or use ←/→. Click switches.")
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
    pub(crate) fn save_allowance(&mut self, provider: Provider, policy: ProviderLimits) {
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
            Provider::OpenRouter => "openrouter",
        };
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
            if provider == Provider::Cursor {
                out = crate::config::upsert_section_value(
                    &out,
                    &format!("allowance_guard.providers.{name}"),
                    "extra_spend",
                    &toml::Value::try_from(&policy.extra_spend)
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                );
            }
            out
        }) {
            self.state.allowance_guard.reset_provider(provider);
            self.apply_config_from_disk(false);
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
                matches!(app.allowance_key(KeyEvent::from(KeyCode::Char('s'))),Some(SettingsAction::SaveAllowance(p,_)) if p==provider)
            );
            assert_eq!(app.mode, Mode::Terminal);
        }
    }
    #[test]
    fn panel_mouse_toggle_save_cancel_and_visible_values() {
        let mut app = AppState::test_new();
        app.open_allowance_settings(Provider::Codex);
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
        assert!(!text.contains("may work"));
        assert_eq!(app.allowance_settings.as_ref().unwrap().rows().len(), 9);
        app.allowance_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.allowance_settings.is_none());
    }
    #[test]
    fn each_mouse_control_selects_changes_saves_or_cancels() {
        let mut app = AppState::test_new();
        app.view.terminal_area = Rect::new(0, 0, 120, 40);
        app.open_allowance_settings(Provider::Codex);
        let rect = popup(app.screen_rect()).unwrap();
        for idx in 0..9 {
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
        let action = app.allowance_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 2,
            row: rect.bottom() - 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(matches!(action, Some(SettingsAction::SaveAllowance(_, _))));
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

#[cfg(test)]
mod spend_panel_tests {
    use super::*;
    #[test]
    fn allowance_settings_cursor_controls_reset_edit_validate_save_and_cancel() {
        let mut app = AppState::test_new();
        app.view.terminal_area = Rect::new(0, 0, 120, 40);
        app.open_allowance_settings(Provider::Cursor);
        let rect = popup(app.screen_rect()).unwrap();
        let panel = app.allowance_settings.as_mut().unwrap();
        panel.spend_reading = Some(crate::usage::CursorSpend {
            bonus_units: 43354,
            on_demand_units: 0,
            total_units: 49500,
            cycle_end: 10000,
        });
        app.allowance_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 2,
            row: rect.y + 12,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(
            app.allowance_settings
                .as_ref()
                .unwrap()
                .draft
                .extra_spend
                .enabled
        );
        for (idx, digits, expected) in [(10, "20", 20), (11, "10", 10), (12, "5", 5)] {
            app.allowance_settings.as_mut().unwrap().selected = idx;
            app.allowance_settings.as_mut().unwrap().digits.clear();
            for c in digits.chars() {
                app.allowance_key(KeyEvent::from(KeyCode::Char(c)));
            }
            assert_eq!(
                app.allowance_settings.as_ref().unwrap().values()[idx - 1],
                expected
            );
        }
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
        assert!(text.contains("Extra spend · Stop at: 5%"));
        assert!(!text.contains("may work"));
        assert!(matches!(
            app.allowance_key(KeyEvent::from(KeyCode::Char('s'))),
            Some(SettingsAction::SaveAllowance(Provider::Cursor, _))
        ));
        app.open_allowance_settings(Provider::Cursor);
        app.allowance_settings
            .as_mut()
            .unwrap()
            .draft
            .extra_spend
            .enabled = true;
        app.allowance_settings.as_mut().unwrap().selected = 10;
        app.allowance_key(KeyEvent::from(KeyCode::Char('2')));
        assert!(app
            .allowance_key(KeyEvent::from(KeyCode::Char('s')))
            .is_none());
        assert!(app
            .allowance_settings
            .as_ref()
            .unwrap()
            .error
            .contains("Stop"));
        app.allowance_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.allowance_settings.is_none());
    }
}
