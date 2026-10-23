//! Lines between overseers. Only the overseers named by the caller's role can
//! send them, and a space overseer can only reach the chief.
use super::responses::{encode_error, encode_success};
use crate::api::schema::{OverseerEscalateParams, OverseerMessageParams, ResponseResult};
use crate::app::App;

impl App {
    pub(super) fn handle_overseer_escalate(
        &mut self,
        id: String,
        params: OverseerEscalateParams,
    ) -> String {
        let Some(caller) = self.parse_reporting_pane_id(&params.caller_pane_id) else {
            return encode_error(id, "pane_not_found", "calling pane not found");
        };
        match self.overseer_escalate(caller, &params.text, params.wake, std::time::Instant::now()) {
            Ok(_) => encode_success(id, ResponseResult::Ok {}),
            Err((code, why)) => encode_error(id, code, why),
        }
    }

    pub(super) fn handle_overseer_message(
        &mut self,
        id: String,
        params: OverseerMessageParams,
    ) -> String {
        let Some(caller) = self.parse_reporting_pane_id(&params.caller_pane_id) else {
            return encode_error(id, "pane_not_found", "calling pane not found");
        };
        let Some((_, target)) = self.parse_pane_id(&params.to) else {
            return encode_error(id, "pane_not_found", "target pane not found");
        };
        match self.overseer_message(caller, target, &params.text, std::time::Instant::now()) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err((code, why)) => encode_error(id, code, why),
        }
    }
}
