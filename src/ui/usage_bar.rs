//! Usage meters: ten squares, a number and a countdown for each allowance
//! window, in a pane's header and in a sidebar block.
//!
//! Everything here is pure. Callers pass the snapshot and `now`; nothing
//! reads a file or a clock.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    Frame,
};

use crate::app::AppState;
use crate::usage::{
    countdown::countdown,
    palette::{self, EMPTY},
    Provider, ProviderUsage, UsageProblem, UsageSnapshot, WindowKind,
};

/// A Claude or Codex reading older than this is drawn dimmed. Both update on
/// every agent turn, so dim honestly means "not used lately".
pub(crate) const STALE_AFTER_SECS: u64 = 15 * 60;

/// Cursor is fetched at most every `CURSOR_MIN_INTERVAL_SECS` and only when a
/// worker refresh comes round, so its readings are legitimately that old.
const CURSOR_STALE_MARGIN_SECS: u64 = 60;

/// How old a provider's reading may get before it is drawn dimmed.
pub(crate) fn stale_after_secs(provider: Provider) -> u64 {
    match provider {
        Provider::Claude | Provider::Codex => STALE_AFTER_SECS,
        Provider::Cursor => {
            crate::usage::CURSOR_MIN_INTERVAL_SECS
                + crate::app::USAGE_REFRESH_INTERVAL.as_secs()
                + CURSOR_STALE_MARGIN_SECS
        }
    }
}

/// Rows of the spaces list the block never takes away: a space and a few agents.
const SIDEBAR_LIST_MIN_ROWS: u16 = 5;

const SQUARE: &str = "■";

/// One window, resolved to what is drawn.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Meter {
    pub label: &'static str,
    pub lit: usize,
    pub rgb: (u8, u8, u8),
    pub number: String,
    pub countdown: String,
    pub stale: bool,
}

/// What a narrow row keeps. Dropped in order: labels, the second window,
/// squares, the number, then the countdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fit {
    pub labels: bool,
    pub windows: usize,
    pub squares: bool,
    pub number: bool,
    pub countdown: bool,
}

pub(crate) fn provider_for_agent(agent: crate::detect::Agent) -> Option<Provider> {
    match agent {
        crate::detect::Agent::Claude => Some(Provider::Claude),
        crate::detect::Agent::Codex => Some(Provider::Codex),
        crate::detect::Agent::Cursor => Some(Provider::Cursor),
        _ => None,
    }
}

pub(crate) fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        Provider::Codex => "GPT",
        Provider::Cursor => "Cursor",
    }
}

pub(crate) fn window_label(kind: WindowKind) -> &'static str {
    match kind {
        WindowKind::FiveHour => "5h",
        WindowKind::Weekly => "wk",
        WindowKind::Monthly => "mo",
    }
}

pub(crate) fn window_phrase(kind: WindowKind) -> &'static str {
    match kind {
        WindowKind::FiveHour => "five hour",
        WindowKind::Weekly => "weekly",
        WindowKind::Monthly => "monthly",
    }
}

/// Spoken alert at 10% left: `Claude five hour at 10 percent`.
pub(crate) fn alert_phrase(provider: Provider, kind: WindowKind) -> String {
    format!(
        "{} {} at 10 percent",
        provider_name(provider),
        window_phrase(kind)
    )
}

fn default_kinds(provider: Provider) -> &'static [WindowKind] {
    match provider {
        Provider::Claude | Provider::Codex => &[WindowKind::FiveHour, WindowKind::Weekly],
        Provider::Cursor => &[WindowKind::Monthly],
    }
}

fn is_stale(usage: &ProviderUsage, now: u64) -> bool {
    usage
        .seen_at
        .is_some_and(|seen| now.saturating_sub(seen) > stale_after_secs(usage.provider))
}

/// Ten squares, each 10%: `left` rounded up to the next ten, so 1% still lights one.
pub(crate) fn lit_squares(left_pct: Option<u8>) -> usize {
    left_pct.map_or(0, |left| usize::from(left.min(100)).div_ceil(10))
}

/// The meters for one provider. A provider with no windows yet gets
/// placeholders so the bar says `?` rather than vanishing.
pub(crate) fn meters_for(usage: &ProviderUsage, now: u64) -> Vec<Meter> {
    let stale = is_stale(usage, now);
    let text_instead = match usage.problem {
        Some(UsageProblem::LoginExpired) => Some("login expired"),
        Some(UsageProblem::NoLogin) => Some("no login"),
        _ => None,
    };
    let one = |kind: WindowKind, left: Option<u8>, resets_at: Option<u64>| {
        let cd = countdown(resets_at, now);
        // A window whose reset has passed is over: full again, and idle.
        let over = resets_at.is_some_and(|at| at <= now);
        let left = if over {
            Some(100)
        } else {
            left.map(|l| l.min(100))
        };
        let number = match (text_instead, left) {
            (Some(text), _) => text.to_string(),
            (None, Some(left)) => format!("{left}%"),
            (None, None) => "?".to_string(),
        };
        Meter {
            label: window_label(kind),
            lit: if text_instead.is_some() {
                0
            } else {
                lit_squares(left)
            },
            rgb: palette::level_rgb(usage.provider, kind, left),
            number,
            countdown: cd.text,
            stale,
        }
    };
    if usage.windows.is_empty() {
        return default_kinds(usage.provider)
            .iter()
            .map(|kind| one(*kind, None, None))
            .collect();
    }
    usage
        .windows
        .iter()
        .map(|w| one(w.kind, w.left_pct, w.resets_at))
        .collect()
}

fn dim_rgb(rgb: (u8, u8, u8)) -> (u8, u8, u8) {
    let mix = |a: u8, b: u8| ((u16::from(a) + u16::from(b) * 3) / 4) as u8;
    (
        mix(rgb.0, EMPTY.0),
        mix(rgb.1, EMPTY.1),
        mix(rgb.2, EMPTY.2),
    )
}

fn rgb_color(rgb: (u8, u8, u8)) -> Color {
    Color::Rgb(rgb.0, rgb.1, rgb.2)
}

fn meter_width(meter: &Meter, fit: Fit) -> usize {
    let mut parts = Vec::new();
    if fit.labels {
        parts.push(meter.label.chars().count());
    }
    if fit.squares {
        parts.push(10);
    }
    if fit.number {
        parts.push(meter.number.chars().count());
    }
    if fit.countdown {
        parts.push(meter.countdown.chars().count());
    }
    parts.iter().sum::<usize>() + parts.len().saturating_sub(1)
}

/// Columns the meters take at `fit`, with one space between windows.
pub(crate) fn row_width(meters: &[Meter], fit: Fit) -> usize {
    let shown = &meters[..fit.windows.min(meters.len())];
    let widths: usize = shown.iter().map(|m| meter_width(m, fit)).sum();
    widths + shown.len().saturating_sub(1)
}

/// The layouts to try, fullest first, for rows of up to `n` windows: labels,
/// then the second window, then the squares, then the number.
fn fit_steps(n: usize, allow_labels: bool) -> Vec<Fit> {
    let mut steps = Vec::new();
    if allow_labels {
        steps.push(Fit {
            labels: true,
            windows: n,
            squares: true,
            number: true,
            countdown: true,
        });
    }
    steps.push(Fit {
        labels: false,
        windows: n,
        squares: true,
        number: true,
        countdown: true,
    });
    if n > 1 {
        steps.push(Fit {
            labels: false,
            windows: 1,
            squares: true,
            number: true,
            countdown: true,
        });
    }
    steps.push(Fit {
        labels: false,
        windows: 1,
        squares: false,
        number: true,
        countdown: true,
    });
    steps.push(Fit {
        labels: false,
        windows: 1,
        squares: false,
        number: false,
        countdown: true,
    });
    steps
}

/// The fullest layout that fits `avail` columns, or `None` when not even the
/// countdown does.
pub(crate) fn fit_row(meters: &[Meter], avail: usize, allow_labels: bool) -> Option<Fit> {
    if meters.is_empty() {
        return None;
    }
    fit_steps(meters.len(), allow_labels)
        .into_iter()
        .find(|fit| row_width(meters, *fit) <= avail)
}

/// `fit` for one row: the same choices, with the window count capped to the
/// windows that row has.
pub(crate) fn fit_for(meters: &[Meter], fit: Fit) -> Fit {
    Fit {
        windows: fit.windows.min(meters.len()),
        ..fit
    }
}

/// The fullest layout that fits `avail` columns for every row at once, each
/// row counting only its own windows. `None` when not even the countdown fits
/// the widest row, or when no row has meters.
pub(crate) fn fit_rows(rows: &[Vec<Meter>], avail: usize, allow_labels: bool) -> Option<Fit> {
    let n = rows.iter().map(Vec::len).max().filter(|n| *n > 0)?;
    fit_steps(n, allow_labels).into_iter().find(|fit| {
        rows.iter()
            .filter(|meters| !meters.is_empty())
            .all(|meters| row_width(meters, fit_for(meters, *fit)) <= avail)
    })
}

fn meter_spans(meter: &Meter, fit: Fit, dim: Color, out: &mut Vec<Span<'static>>) {
    let rgb = if meter.stale {
        dim_rgb(meter.rgb)
    } else {
        meter.rgb
    };
    let lit = Style::default().fg(rgb_color(rgb)).bg(Color::Reset);
    let unlit = Style::default()
        .fg(rgb_color(if meter.stale { dim_rgb(EMPTY) } else { EMPTY }))
        .bg(Color::Reset);
    let dim_style = Style::default().fg(dim).bg(Color::Reset);
    let mut first = true;
    let sep = |out: &mut Vec<Span<'static>>, first: &mut bool| {
        if !*first {
            out.push(Span::raw(" "));
        }
        *first = false;
    };
    if fit.labels {
        sep(out, &mut first);
        out.push(Span::styled(meter.label, dim_style));
    }
    if fit.squares {
        sep(out, &mut first);
        out.push(Span::styled(SQUARE.repeat(meter.lit), lit));
        out.push(Span::styled(SQUARE.repeat(10 - meter.lit), unlit));
    }
    if fit.number {
        sep(out, &mut first);
        out.push(Span::styled(meter.number.clone(), lit));
    }
    if fit.countdown {
        sep(out, &mut first);
        out.push(Span::styled(meter.countdown.clone(), dim_style));
    }
}

/// The spans for a row of meters at `fit`, windows joined by one space.
pub(crate) fn row_spans(meters: &[Meter], fit: Fit, dim: Color) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (idx, meter) in meters.iter().take(fit.windows).enumerate() {
        if idx > 0 {
            out.push(Span::raw(" "));
        }
        meter_spans(meter, fit, dim, &mut out);
    }
    out
}

/// What a pane header carries for its provider, resolved before layout.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HeaderUsage {
    pub meters: Vec<Meter>,
}

/// Header meters for the provider of the agent in a pane. `None` when the
/// field is off, the pane has no known agent, or no snapshot has arrived.
pub(crate) fn header_usage_for(
    app: &AppState,
    agent: Option<crate::detect::Agent>,
) -> Option<HeaderUsage> {
    if !app.usage_headers_on() {
        return None;
    }
    let provider = provider_for_agent(agent?)?;
    let snapshot = app.usage_snapshot.as_ref()?;
    let usage = snapshot.providers.iter().find(|u| u.provider == provider)?;
    Some(HeaderUsage {
        meters: meters_for(usage, app.usage_now),
    })
}

/// Columns a header's meters take in `avail`, or zero when nothing fits.
/// Labels (`5h`, `wk`, `mo`) are shown when there is room and are the first
/// thing dropped, so a half width pane loses them and a full width one keeps them.
pub(crate) fn header_width(usage: &HeaderUsage, avail: usize) -> usize {
    fit_row(&usage.meters, avail, true).map_or(0, |fit| row_width(&usage.meters, fit))
}

pub(crate) fn header_spans(usage: &HeaderUsage, avail: usize, dim: Color) -> Vec<Span<'static>> {
    match fit_row(&usage.meters, avail, true) {
        Some(fit) => row_spans(&usage.meters, fit, dim),
        None => Vec::new(),
    }
}

/// The meters for the provider of the agent in a terminal, for the agent
/// table's Allowance column. Empty when the column is off, the terminal has no
/// known provider, or no snapshot has arrived. Same meters as the headers.
pub(crate) fn table_meters_for(
    app: &AppState,
    terminal_id: &crate::terminal::TerminalId,
) -> Vec<Meter> {
    if !app.usage_table_on() {
        return Vec::new();
    }
    let provider = app
        .terminals
        .get(terminal_id)
        .and_then(|terminal| terminal.effective_known_agent())
        .and_then(provider_for_agent);
    let (Some(provider), Some(snapshot)) = (provider, app.usage_snapshot.as_ref()) else {
        return Vec::new();
    };
    snapshot
        .providers
        .iter()
        .find(|u| u.provider == provider)
        .map(|usage| meters_for(usage, app.usage_now))
        .unwrap_or_default()
}

/// The columns a row of meters wants at full length: labels, every window,
/// squares, number and countdown.
pub(crate) fn full_width(meters: &[Meter]) -> usize {
    if meters.is_empty() {
        return 0;
    }
    row_width(
        meters,
        Fit {
            labels: true,
            windows: meters.len(),
            squares: true,
            number: true,
            countdown: true,
        },
    )
}

/// The least a table cell may be squeezed to and still say something: one
/// window's number and countdown, sized for `100%` and a five character reset.
pub(crate) const TABLE_MIN_WIDTH: usize = 10;

/// Providers of the agents in panes, in a fixed order.
pub(crate) fn providers_present(app: &AppState) -> Vec<Provider> {
    let mut found = [false; 3];
    for terminal in app.terminals.values() {
        if let Some(provider) = terminal
            .effective_known_agent()
            .and_then(provider_for_agent)
        {
            found[provider_index(provider)] = true;
        }
    }
    [Provider::Claude, Provider::Codex, Provider::Cursor]
        .into_iter()
        .filter(|p| found[provider_index(*p)])
        .collect()
}

fn provider_index(provider: Provider) -> usize {
    match provider {
        Provider::Claude => 0,
        Provider::Codex => 1,
        Provider::Cursor => 2,
    }
}

/// Seconds until the next thing a visible meter shows changes: a countdown
/// text, or a reading turning stale. `None` when nothing will.
pub(crate) fn next_change_secs(
    snapshot: &UsageSnapshot,
    present: &[Provider],
    now: u64,
) -> Option<u64> {
    let mut next: Option<u64> = None;
    let mut take = |secs: u64| next = Some(next.map_or(secs, |cur| cur.min(secs)));
    for usage in snapshot
        .providers
        .iter()
        .filter(|u| present.contains(&u.provider))
    {
        for window in &usage.windows {
            if let Some(secs) = countdown(window.resets_at, now).next_change_secs {
                take(secs);
            }
        }
        if let Some(seen) = usage.seen_at {
            let stale_at = seen + stale_after_secs(usage.provider) + 1;
            if stale_at > now {
                take(stale_at - now);
            }
        }
    }
    next
}

// ---- sidebar block ----

/// One line of the sidebar block.
#[derive(Clone, Debug, PartialEq)]
enum BlockRow {
    Title,
    Provider(Provider, Option<&'static str>),
    Meter(Vec<Meter>),
}

fn block_rows(app: &AppState) -> Vec<BlockRow> {
    let Some(snapshot) = app.usage_snapshot.as_ref() else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for provider in providers_present(app) {
        let Some(usage) = snapshot.providers.iter().find(|u| u.provider == provider) else {
            continue;
        };
        if rows.is_empty() {
            rows.push(BlockRow::Title);
        }
        rows.push(BlockRow::Provider(provider, None));
        for meter in meters_for(usage, app.usage_now) {
            rows.push(BlockRow::Meter(vec![meter]));
        }
    }
    rows
}

/// Rows the sidebar block reserves under the list. Zero when off, when there
/// is nothing to show, or unless everything else still fits above the player:
/// the spaces header, a minimum list, and the `+ new` footer. A full player
/// keeps only a few rows above it, so with it open the block usually yields.
pub(crate) fn sidebar_rows(app: &AppState, sidebar_h: u16) -> u16 {
    if !app.usage_sidebar_on() || app.sidebar_collapsed {
        return 0;
    }
    let rows = block_rows(app).len() as u16;
    if rows == 0 {
        return 0;
    }
    let player = crate::ui::player::player_rows_for_sidebar(app, sidebar_h);
    let needed = super::sidebar::SECTION_HEADER_ROWS
        + SIDEBAR_LIST_MIN_ROWS
        + super::sidebar::SECTION_FOOTER_ROWS
        + rows;
    if sidebar_h.saturating_sub(player) < needed {
        return 0;
    }
    rows
}

/// The one call the sidebar makes: draws the block in the rows it reserved,
/// just above the player box at the sidebar's floor.
pub(crate) fn render_sidebar_block(app: &AppState, frame: &mut Frame, sidebar: Rect) {
    let rows = sidebar_rows(app, sidebar.height);
    if rows == 0 {
        return;
    }
    let player = crate::ui::player::player_rows_for_sidebar(app, sidebar.height);
    let bottom = sidebar.y + sidebar.height.saturating_sub(player);
    let area = Rect::new(sidebar.x, bottom.saturating_sub(rows), sidebar.width, rows);
    render_allowance_block(app, frame, area);
}

fn render_allowance_block(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.width < 6 || area.height == 0 {
        return;
    }
    let dim = app.palette.overlay0;
    let rows = block_rows(app);
    for (idx, row) in rows.iter().take(usize::from(area.height)).enumerate() {
        let rect = Rect::new(
            area.x + 1,
            area.y + idx as u16,
            area.width.saturating_sub(2),
            1,
        );
        let line = match row {
            BlockRow::Title => Line::from(Span::styled(" allowance", Style::default().fg(dim))),
            BlockRow::Provider(provider, _) => {
                let kind = *default_kinds(*provider)
                    .last()
                    .unwrap_or(&WindowKind::Weekly);
                let rgb = palette::window_rgb(*provider, kind);
                Line::from(Span::styled(
                    format!(" {}", provider_name(*provider)),
                    Style::default().fg(rgb_color(rgb)),
                ))
            }
            BlockRow::Meter(meters) => {
                let avail = usize::from(rect.width).saturating_sub(1);
                match fit_row(meters, avail, true) {
                    Some(fit) => {
                        let mut spans = vec![Span::raw(" ")];
                        spans.extend(row_spans(meters, fit, dim));
                        Line::from(spans)
                    }
                    None => Line::default(),
                }
            }
        };
        frame.render_widget(ratatui::widgets::Paragraph::new(line), rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageWindow;

    const NOW: u64 = 1_000_000;

    fn window(kind: WindowKind, left: Option<u8>, secs: Option<u64>) -> UsageWindow {
        UsageWindow {
            kind,
            left_pct: left,
            resets_at: secs.map(|s| NOW + s),
            note: None,
        }
    }

    fn usage(provider: Provider, windows: Vec<UsageWindow>) -> ProviderUsage {
        ProviderUsage {
            provider,
            windows,
            seen_at: Some(NOW),
            source: "test".into(),
            problem: None,
        }
    }

    fn one(left: Option<u8>) -> Meter {
        meters_for(
            &usage(
                Provider::Claude,
                vec![window(WindowKind::FiveHour, left, Some(3 * 3600 + 10))],
            ),
            NOW,
        )
        .remove(0)
    }

    #[test]
    fn squares_round_up_to_the_next_ten() {
        for (left, lit) in [
            (Some(0), 0),
            (Some(1), 1),
            (Some(10), 1),
            (Some(11), 2),
            (Some(20), 2),
            (Some(21), 3),
            (Some(100), 10),
            (None, 0),
        ] {
            assert_eq!(one(left).lit, lit, "left {left:?}");
        }
    }

    #[test]
    fn number_is_the_percent_or_a_question_mark() {
        assert_eq!(one(Some(70)).number, "70%");
        assert_eq!(one(None).number, "?");
    }

    #[test]
    fn pink_at_20_not_21_and_red_at_10_not_11() {
        let base = palette::window_rgb(Provider::Claude, WindowKind::FiveHour);
        assert_eq!(one(Some(21)).rgb, base);
        assert_eq!(one(Some(20)).rgb, palette::PINK);
        assert_eq!(one(Some(11)).rgb, palette::PINK);
        assert_eq!(one(Some(10)).rgb, palette::RED);
        assert_eq!(one(Some(0)).rgb, palette::RED);
    }

    #[test]
    fn a_reading_over_15_minutes_old_is_stale() {
        let mut u = usage(
            Provider::Claude,
            vec![window(WindowKind::FiveHour, Some(50), Some(100))],
        );
        assert!(!meters_for(&u, NOW + STALE_AFTER_SECS)[0].stale);
        assert!(meters_for(&u, NOW + STALE_AFTER_SECS + 1)[0].stale);
        u.seen_at = None;
        assert!(!meters_for(&u, NOW + 99_999)[0].stale);
        let stale = Meter {
            stale: true,
            ..one(Some(50))
        };
        let mut spans = Vec::new();
        meter_spans(
            &stale,
            Fit {
                labels: false,
                windows: 1,
                squares: true,
                number: true,
                countdown: true,
            },
            Color::Gray,
            &mut spans,
        );
        let fresh_fg = Style::default().fg(rgb_color(one(Some(50)).rgb)).fg;
        assert_ne!(spans[0].style.fg, fresh_fg);
    }

    #[test]
    fn a_cursor_reading_is_stale_only_after_its_fetch_gap() {
        let mut u = usage(
            Provider::Cursor,
            vec![window(WindowKind::Monthly, Some(50), Some(100_000))],
        );
        let at = |mins: u64| meters_for(&u, NOW + mins * 60)[0].stale;
        assert!(!at(16));
        assert!(!at(20));
        assert!(!at(21));
        assert!(at(22));
        // Claude and Codex keep 15 minutes.
        for provider in [Provider::Claude, Provider::Codex] {
            u.provider = provider;
            assert!(!meters_for(&u, NOW + 15 * 60)[0].stale);
            assert!(meters_for(&u, NOW + 16 * 60)[0].stale);
        }
    }

    #[test]
    fn login_problems_replace_the_number() {
        let mut u = usage(
            Provider::Cursor,
            vec![window(WindowKind::Monthly, None, Some(17 * 86400))],
        );
        u.problem = Some(UsageProblem::LoginExpired);
        let m = &meters_for(&u, NOW)[0];
        assert_eq!(m.number, "login expired");
        assert_eq!(m.lit, 0);
        assert_eq!(m.countdown, "2/w");
        u.problem = Some(UsageProblem::NoLogin);
        assert_eq!(meters_for(&u, NOW)[0].number, "no login");
    }

    #[test]
    fn a_window_past_its_reset_is_full_and_idle() {
        let u = usage(
            Provider::Claude,
            vec![window(WindowKind::FiveHour, Some(30), Some(0))],
        );
        let m = &meters_for(&u, NOW)[0];
        assert_eq!(m.number, "100%");
        assert_eq!(m.lit, 10);
        assert_eq!(m.countdown, "idle");
    }

    #[test]
    fn no_windows_shows_question_marks() {
        let u = usage(Provider::Claude, Vec::new());
        let ms = meters_for(&u, NOW);
        assert_eq!(ms.len(), 2);
        assert!(ms
            .iter()
            .all(|m| m.number == "?" && m.lit == 0 && m.countdown == "?"));
    }

    fn claude_pair() -> Vec<Meter> {
        meters_for(
            &usage(
                Provider::Claude,
                vec![
                    window(WindowKind::FiveHour, Some(70), Some(3 * 3600 + 42 * 60)),
                    window(WindowKind::Weekly, Some(80), Some(4 * 86400 + 5 * 3600)),
                ],
            ),
            NOW,
        )
    }

    fn text(meters: &[Meter], fit: Fit) -> String {
        row_spans(meters, fit, Color::Gray)
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn narrow_panes_drop_labels_then_weekly_then_squares_then_number() {
        let m = claude_pair();
        let at = |w: usize| fit_row(&m, w, true);
        let full = at(200).unwrap();
        assert!(full.labels && full.windows == 2 && full.squares);
        assert_eq!(
            text(&m, full),
            "5h ■■■■■■■■■■ 70% 3/h wk ■■■■■■■■■■ 80% 4/d"
        );
        let width = row_width(&m, full);
        assert_eq!(width, 43);
        let no_labels = at(width - 1).unwrap();
        assert!(!no_labels.labels && no_labels.windows == 2 && no_labels.squares);
        let w2 = row_width(&m, no_labels);
        let one_window = at(w2 - 1).unwrap();
        assert!(!one_window.labels && one_window.windows == 1 && one_window.squares);
        assert_eq!(text(&m, one_window), "■■■■■■■■■■ 70% 3/h");
        let w3 = row_width(&m, one_window);
        let no_squares = at(w3 - 1).unwrap();
        assert!(!no_squares.squares && no_squares.number && no_squares.countdown);
        assert_eq!(text(&m, no_squares), "70% 3/h");
        let w4 = row_width(&m, no_squares);
        assert_eq!(w4, 7);
        let only_countdown = at(w4 - 1).unwrap();
        assert!(!only_countdown.number && only_countdown.countdown);
        assert_eq!(text(&m, only_countdown), "3/h");
        assert_eq!(at(2), None);
    }

    #[test]
    fn sidebar_rows_keep_labels_off_the_drop_list_until_needed() {
        let m = vec![claude_pair().remove(0)];
        assert_eq!(fit_row(&m, 21, true).map(|f| f.labels), Some(true));
        assert_eq!(fit_row(&m, 20, true).map(|f| f.labels), Some(false));
    }

    fn sidebar_app(providers: usize, player_open: bool) -> AppState {
        let mut app = AppState::test_new();
        app.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        app.active = Some(0);
        app.player_expanded = player_open;
        let agents = [
            crate::detect::Agent::Claude,
            crate::detect::Agent::Codex,
            crate::detect::Agent::Cursor,
        ];
        let mut provs = Vec::new();
        for (idx, agent) in agents.iter().take(providers).enumerate() {
            let id = crate::terminal::TerminalId::alloc();
            let mut terminal =
                crate::terminal::TerminalState::new(id.clone(), std::path::PathBuf::from("/tmp"));
            terminal.detected_agent = Some(*agent);
            app.terminals.insert(id, terminal);
            let provider = provider_for_agent(*agent).unwrap();
            let kinds: &[WindowKind] = if idx == 2 {
                &[WindowKind::Monthly]
            } else {
                &[WindowKind::FiveHour, WindowKind::Weekly]
            };
            provs.push(usage(
                provider,
                kinds
                    .iter()
                    .map(|k| window(*k, Some(50), Some(3600)))
                    .collect(),
            ));
        }
        app.usage_now = NOW;
        app.usage_snapshot = Some(UsageSnapshot {
            version: 1,
            taken_at: NOW,
            providers: provs,
        });
        app
    }

    #[test]
    fn the_sidebar_block_only_takes_rows_when_everything_else_still_fits() {
        use super::super::sidebar::{SECTION_FOOTER_ROWS, SECTION_HEADER_ROWS};
        for providers in 1..=3 {
            for open in [false, true] {
                let app = sidebar_app(providers, open);
                let wanted = block_rows(&app).len() as u16;
                assert!(wanted >= 3, "{providers} providers want {wanted} rows");
                for h in 0..=80u16 {
                    let rows = sidebar_rows(&app, h);
                    assert!(rows == 0 || rows == wanted, "h {h}: {rows}");
                    if rows > 0 {
                        let player = crate::ui::player::player_rows_for_sidebar(&app, h);
                        // The header, a minimum list, the `+ new` footer and the
                        // player all still fit above the floor with the block.
                        assert!(
                            SECTION_HEADER_ROWS
                                + SIDEBAR_LIST_MIN_ROWS
                                + SECTION_FOOTER_ROWS
                                + rows
                                + player
                                <= h,
                            "{providers} providers, player open {open}, height {h}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_full_player_leaves_no_room_for_the_block_and_the_new_button_stays() {
        let area_for = |h| Rect::new(0, 0, 26, h);
        for providers in 1..=3 {
            let app = sidebar_app(providers, true);
            let mut kept = 0;
            for h in [20u16, 24, 30, 40, 60] {
                assert_eq!(sidebar_rows(&app, h), 0, "{providers} providers, h {h}");
                // `+ new` sits where it would with the block switched off.
                let with = crate::ui::new_workspace_button_rect(&app, area_for(h));
                let mut off = sidebar_app(providers, true);
                off.usage_config.sidebar = false;
                let without = crate::ui::new_workspace_button_rect(&off, area_for(h));
                assert_eq!(with, without, "{providers} providers, h {h}");
                kept += usize::from(with.height > 0);
            }
            assert!(
                kept > 0,
                "+ new shows at some height with {providers} providers"
            );
        }
        // The same sidebar with the small player has room at a tall height.
        let app = sidebar_app(3, false);
        assert!(sidebar_rows(&app, 40) > 0);
        assert_eq!(sidebar_rows(&app, 15), 0);
    }

    #[test]
    fn the_spoken_alert_names_provider_and_window() {
        assert_eq!(
            alert_phrase(Provider::Claude, WindowKind::FiveHour),
            "Claude five hour at 10 percent"
        );
        assert_eq!(
            alert_phrase(Provider::Codex, WindowKind::Weekly),
            "GPT weekly at 10 percent"
        );
        assert_eq!(
            alert_phrase(Provider::Cursor, WindowKind::Monthly),
            "Cursor monthly at 10 percent"
        );
    }

    #[test]
    fn next_change_takes_the_smallest_countdown_and_the_stale_edge() {
        let snapshot = UsageSnapshot {
            version: 1,
            taken_at: NOW,
            providers: vec![
                usage(
                    Provider::Claude,
                    vec![window(WindowKind::FiveHour, Some(70), Some(3 * 3600 + 5))],
                ),
                usage(
                    Provider::Codex,
                    vec![window(WindowKind::FiveHour, Some(20), Some(90))],
                ),
            ],
        };
        // Claude 3h5s steps after 6s; Codex 90s is 1/m and steps after 31s.
        assert_eq!(
            next_change_secs(&snapshot, &[Provider::Claude, Provider::Codex], NOW),
            Some(6)
        );
        assert_eq!(
            next_change_secs(&snapshot, &[Provider::Codex], NOW),
            Some(31)
        );
        // A provider with no agent in a pane is not visible, so it is ignored.
        assert_eq!(
            next_change_secs(&snapshot, &[Provider::Claude], NOW),
            Some(6)
        );
        // The stale edge counts when it is the nearest.
        let later = NOW + STALE_AFTER_SECS - 2;
        let snap = UsageSnapshot {
            providers: vec![ProviderUsage {
                windows: vec![],
                ..usage(Provider::Claude, vec![])
            }],
            ..snapshot
        };
        assert_eq!(next_change_secs(&snap, &[Provider::Claude], later), Some(3));
        assert_eq!(next_change_secs(&snap, &[Provider::Cursor], later), None);
    }
}
