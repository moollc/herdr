#!/usr/bin/env bash
set -uo pipefail

project_dir="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project_dir"
log_path="${VALIDATION_LOG:-evidence/recent-agent-files/validation.log}"
render_path="${HERDR_RECENT_AGENT_FILES_RENDER:-evidence/recent-agent-files/render.txt}"
mkdir -p "$(dirname "$log_path")" "$(dirname "$render_path")"

run_filter() {
    filter="$1"
    if command -v just >/dev/null 2>&1; then
        just test-one "$filter"
    elif cargo nextest --version >/dev/null 2>&1; then
        cargo nextest run --locked "$filter" --status-level fail --final-status-level fail \
            --failure-output final --success-output never
    else
        cargo test --offline --locked --bin herdr "$filter"
    fi
}

run_checks() {
    cargo fmt --check || return
    python3 scripts/test_recent_agent_files_hook.py || return
    export HERDR_RECENT_AGENT_FILES_RENDER="$render_path"
    run_filter recent || return
    unset HERDR_RECENT_AGENT_FILES_RENDER
    run_filter opener
    run_filter native_turn_cursor_hooks_preserve_user_hooks
    run_filter install_codex_is_idempotent_for_hook_entries_and_feature_flag
}

if run_checks >"$log_path" 2>&1; then
    status=0
    result="passed"
else
    status=$?
    result="failed"
fi

printf 'validation %s (exit %s)\nlog: %s\nrender: %s\n' "$result" "$status" "$log_path" "$render_path" >>"$log_path"

if [[ "${SPEAK:-1}" != "0" && "$(uname -s)" == "Darwin" ]]; then
    message="Recent agent files validation finished, $result"
    task_home="$HOME"
    if [[ -x "$task_home/.local/bin/announce" ]]; then
        "$task_home/.local/bin/announce" Sophie "$message" >/dev/null 2>&1 &
    elif command -v say >/dev/null 2>&1; then
        say "$message" >/dev/null 2>&1 &
    fi
fi

exit "$status"
