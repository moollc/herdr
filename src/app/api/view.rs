use crate::api::schema::{ResponseResult, ViewChange, ViewPart, ViewPartInfo, ViewSetParams};
use crate::app::view::Fold;
use crate::app::App;

use super::responses::{encode_error, encode_success};

/// A fold a request names, with the public ids of its space and tab.
type Named = (Fold, Option<String>, Option<String>);
/// Why a request names nothing: an error code and a message.
type Refusal = (&'static str, String);

impl App {
    /// Show, hide or toggle a part of the interface. A request covering
    /// several parts at once, such as every minimap, toggles them together:
    /// all are hidden when any is shown, and all are shown otherwise.
    pub(super) fn handle_view_set(&mut self, id: String, params: ViewSetParams) -> String {
        let marker = (!self.no_session).then(crate::usage::statusline::visibility_file);
        self.handle_view_set_with_usage_marker(id, params, marker.as_deref())
    }
    fn handle_view_set_with_usage_marker(
        &mut self,
        id: String,
        params: ViewSetParams,
        marker: Option<&std::path::Path>,
    ) -> String {
        let folds = match self.view_folds(&params) {
            Ok(folds) => folds,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let shown = match params.change {
            ViewChange::Show => true,
            ViewChange::Hide => false,
            ViewChange::Toggle => !folds.iter().any(|(fold, _, _)| self.state.is_shown(fold)),
        };
        if matches!(params.part, ViewPart::Usage) {
            if let Some(marker) = marker {
                if let Err(error) = crate::usage::statusline::save_visibility(marker, !shown) {
                    return encode_error(id, "usage_visibility_save_failed", error.to_string());
                }
            }
        }
        let mut parts = Vec::with_capacity(folds.len());
        for (fold, workspace_id, tab_id) in folds {
            self.state.set_shown(&fold, shown);
            parts.push(ViewPartInfo {
                part: params.part,
                workspace_id,
                tab_id,
                shown: self.state.is_shown(&fold),
            });
        }
        self.schedule_session_save();
        encode_success(id, ResponseResult::ViewSet { parts })
    }

    /// The folds a request names, each with the public ids of the space and
    /// tab it belongs to.
    fn view_folds(&self, params: &ViewSetParams) -> Result<Vec<Named>, Refusal> {
        let global = |fold: Fold| -> Result<_, Refusal> {
            if params.workspace_id.is_some() || params.tab_id.is_some() {
                return Err((
                    "invalid_request",
                    format!("{:?} is not per space or tab", params.part).to_lowercase(),
                ));
            }
            Ok(vec![(fold, None, None)])
        };
        let space = || -> Result<usize, Refusal> {
            match &params.workspace_id {
                Some(raw) => self
                    .parse_workspace_id(raw)
                    .ok_or_else(|| ("workspace_not_found", format!("workspace {raw} not found"))),
                None => self
                    .state
                    .active
                    .ok_or_else(|| ("workspace_not_found", "no space is open".to_string())),
            }
        };
        match params.part {
            ViewPart::Sidebar => global(Fold::Sidebar),
            ViewPart::Spaces => global(Fold::Spaces),
            ViewPart::AgentTable => global(Fold::AgentTable),
            ViewPart::Usage => global(Fold::Usage),
            ViewPart::SpaceAgents => {
                let ws = space()?;
                Ok(vec![(
                    Fold::SpaceAgents(ws),
                    Some(self.public_workspace_id(ws)),
                    None,
                )])
            }
            ViewPart::SpaceGroup => {
                let ws = space()?;
                let fold = self.state.space_group(ws).ok_or_else(|| {
                    (
                        "invalid_request",
                        "that space heads no group of worktree spaces".to_string(),
                    )
                })?;
                Ok(vec![(fold, Some(self.public_workspace_id(ws)), None)])
            }
            ViewPart::Minimap => {
                let tabs: Vec<(usize, usize)> = match (&params.tab_id, &params.workspace_id) {
                    (Some(raw), _) => vec![self
                        .parse_tab_id(raw)
                        .ok_or_else(|| ("tab_not_found", format!("tab {raw} not found")))?],
                    (None, Some(_)) => {
                        let ws = space()?;
                        (0..self.state.workspaces[ws].tabs.len())
                            .map(|tab| (ws, tab))
                            .collect()
                    }
                    (None, None) => self
                        .state
                        .workspaces
                        .iter()
                        .enumerate()
                        .flat_map(|(ws, w)| (0..w.tabs.len()).map(move |tab| (ws, tab)))
                        .collect(),
                };
                Ok(tabs
                    .into_iter()
                    .map(|(ws, tab)| {
                        (
                            Fold::Minimap { ws, tab },
                            Some(self.public_workspace_id(ws)),
                            self.public_tab_id(ws, tab),
                        )
                    })
                    .collect())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{Method, Request, ViewChange, ViewPart, ViewSetParams};
    use crate::app::view::Fold;
    use crate::app::App;
    use crate::config::Config;
    use crate::workspace::Workspace;

    fn app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        app.state.active = Some(1);
        app
    }

    fn set(app: &mut App, part: ViewPart, change: ViewChange, workspace: Option<&str>) -> String {
        app.handle_api_request(Request {
            id: "t".into(),
            method: Method::ViewSet(ViewSetParams {
                part,
                change,
                workspace_id: workspace.map(str::to_string),
                tab_id: None,
            }),
        })
    }

    #[test]
    fn a_space_part_defaults_to_the_space_on_screen() {
        let mut app = app();
        set(&mut app, ViewPart::SpaceAgents, ViewChange::Toggle, None);
        assert!(app.state.is_shown(&Fold::SpaceAgents(0)));
        assert!(!app.state.is_shown(&Fold::SpaceAgents(1)));
    }

    #[test]
    fn every_minimap_toggles_together() {
        let mut app = app();
        app.state.set_shown(&Fold::Minimap { ws: 0, tab: 0 }, false);
        let reply = set(&mut app, ViewPart::Minimap, ViewChange::Toggle, None);
        // One was still shown, so the toggle hides them all.
        assert!(!app.state.is_shown(&Fold::Minimap { ws: 0, tab: 0 }));
        assert!(!app.state.is_shown(&Fold::Minimap { ws: 1, tab: 0 }));
        assert!(reply.contains("\"shown\":false"), "{reply}");
    }

    #[test]
    fn a_global_part_refuses_a_space() {
        let mut app = app();
        let reply = set(&mut app, ViewPart::Sidebar, ViewChange::Hide, Some("1"));
        assert!(reply.contains("invalid_request"), "{reply}");
        assert!(!app.state.sidebar_collapsed);
    }

    #[test]
    fn a_space_without_worktrees_has_no_group() {
        let mut app = app();
        let reply = set(&mut app, ViewPart::SpaceGroup, ViewChange::Hide, None);
        assert!(reply.contains("invalid_request"), "{reply}");
    }
    #[test]
    fn usage_hides_and_shows_the_table_the_headers_and_the_sidebar_together() {
        let mut app = app();
        // Table and pane headers are opt-in. Turn them on so hide and show
        // still cover all three surfaces.
        app.state.usage_config.table = true;
        app.state.pane_header.usage = true;
        let all_on = |app: &App| {
            app.state.usage_table_on()
                && app.state.usage_headers_on()
                && app.state.usage_sidebar_on()
        };
        let all_off = |app: &App| {
            !app.state.usage_table_on()
                && !app.state.usage_headers_on()
                && !app.state.usage_sidebar_on()
        };
        assert!(all_on(&app));
        let reply = set(&mut app, ViewPart::Usage, ViewChange::Hide, None);
        assert!(reply.contains("\"shown\":false"), "{reply}");
        assert!(all_off(&app));
        // The config switches are untouched: showing brings back what they say.
        assert!(app.state.usage_config.table && app.state.pane_header().usage);
        let reply = set(&mut app, ViewPart::Usage, ViewChange::Show, None);
        assert!(reply.contains("\"shown\":true"), "{reply}");
        assert!(all_on(&app));
        set(&mut app, ViewPart::Usage, ViewChange::Toggle, None);
        assert!(all_off(&app));
        set(&mut app, ViewPart::Usage, ViewChange::Toggle, None);
        assert!(all_on(&app));
        // A switch that is off in the config stays off after show.
        app.state.usage_config.sidebar = false;
        set(&mut app, ViewPart::Usage, ViewChange::Show, None);
        assert!(app.state.usage_table_on() && !app.state.usage_sidebar_on());
    }

    #[test]
    fn usage_is_not_per_space() {
        let mut app = app();
        let reply = set(&mut app, ViewPart::Usage, ViewChange::Hide, Some("1"));
        assert!(reply.contains("invalid_request"), "{reply}");
        assert!(!app.state.usage_hidden);
    }
    #[test]
    fn usage_view_writes_tiny_marker_before_reporting_success() {
        let mut app = app();
        let root = std::env::temp_dir().join(format!(
            "herdr-visibility-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let marker = root.join("visibility.json");
        for (change, hidden) in [
            (ViewChange::Hide, true),
            (ViewChange::Hide, true),
            (ViewChange::Show, false),
            (ViewChange::Toggle, true),
            (ViewChange::Toggle, false),
        ] {
            let reply = app.handle_view_set_with_usage_marker(
                "synthetic".into(),
                ViewSetParams {
                    part: ViewPart::Usage,
                    change,
                    workspace_id: None,
                    tab_id: None,
                },
                Some(&marker),
            );
            assert!(!reply.contains("error"), "{reply}");
            assert_eq!(crate::usage::read_json::<bool>(&marker).unwrap(), hidden);
            assert_eq!(app.state.usage_hidden, hidden);
            assert!(std::fs::metadata(&marker).unwrap().len() <= 5);
        }
        let failed = root.join("directory");
        std::fs::create_dir_all(&failed).unwrap();
        let reply = app.handle_view_set_with_usage_marker(
            "synthetic".into(),
            ViewSetParams {
                part: ViewPart::Usage,
                change: ViewChange::Hide,
                workspace_id: None,
                tab_id: None,
            },
            Some(&failed),
        );
        assert!(reply.contains("usage_visibility_save_failed"));
        assert!(
            !app.state.usage_hidden,
            "a failed write must not hide only part of the UI"
        );
    }
}
