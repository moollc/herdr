# Recent agent files: capability and design note

Date: 2026-10-24
Branch base: `514e5910ee8f2d9326b92dc068b683781a8e8d3d`

## Alternatives considered

1. Keep the 20-second workspace `mtime` scan, expand it to `~`, or scan common output folders. Rejected: it has no author provenance, is incomplete at its caps, and inclusion would depend on path/depth policy.
2. Infer ownership from a busy pane, current working directory, or a recent agent turn. Rejected: those signals cannot prove which process wrote a path.
3. Parse agent session transcript/rollout history. Rejected as the primary source: Herdr's Codex model probe reads rollouts for model/title data, while three sampled recent Codex rollouts exposed generic `exec` items and no normalized file-write event. Parsing command or tool payloads from those logs would also risk retaining or exposing private content.
4. Consume provider post-write hooks and retain only explicit paths plus event provenance. Chosen for direct writes. Use a bounded, durable Herdr state journal independent of workspace and file location.

## Evidence and capabilities found

- `src/recent_files.rs` originally scanned only `workspace.identity_cwd`, every 20 seconds, and ordered by filesystem modification time. It could not attribute a write.
- `src/integration/mod.rs` already registers Codex `PostToolUse` hooks; the shipped state hook used them only for turn state. OpenAI's [Hooks documentation](https://developers.openai.com/de-DE/docs/hooks) describes post-tool `tool_name`, `tool_input`, and `tool_response`, including tool calls named `apply_patch` and shell tools. The installed source also samples Codex rollout records through `src/agent_model.rs`, but not as a write event source.
- Cursor's [Hooks documentation](https://prod.cursor.com/docs/hooks) defines command hooks receiving JSON on stdin and `afterFileEdit` firing after an agent edit with an absolute `file_path`. Herdr's original `src/integration/turns.rs` registered only `sessionStart`, `beforeSubmitPrompt`, and `stop`.
- The actual local Codex hook event inventory contained only `SessionStart` and `UserPromptSubmit`; the actual local Cursor inventory contained `beforeSubmitPrompt`, `preToolUse`, `sessionStart`, and `stop`. Three recent Codex rollout files were sampled by metadata only: records included `response_item`, `event_msg`, `turn_context`, and generic `exec`; no normalized file event was present in the sampled structure. No provider configuration was edited.
- Herdr's established state root is `config::state_dir()` (`~/.local/state/herdr` or `herdr-dev` for development, with XDG override). The captured journal path is exposed only to child processes in a Herdr pane. No live Herdr/provider state is changed by this source-only branch.

## Capture boundary

- Codex `PostToolUse` and Cursor `afterFileEdit` record paths explicitly named by successful provider write/edit events, including outside-cwd paths, untracked files, and any extension. The journal stores an absolute path, hook-observed timestamp, provider, hook/tool provenance, and an opaque event id only. It stores no content, prompts, command text, credentials, or file metadata scans.
- Codex shell capture recognizes explicit output destinations in the successful `PostToolUse` payload (redirection, `tee`, `touch`, `cp`, `mv`, and `install`). This supports a harmless shell-created output outside cwd. It does not run or persist command text. It is intentionally a limited syntax recognizer, not a shell interpreter.
- Cursor's documented `afterShellExecution` payload contains `command`, `output`, `duration`, and `sandbox`, but no exit status or working directory. That hook cannot safely prove a successful output write, so this implementation does not register it or claim Cursor shell-output coverage. Other subprocess side effects not explicitly named by a supported successful write event remain unsupported; they must not be inferred from mtimes or a current-turn window.
- Repeated writes to one path deduplicate to the newest hook event. A capped 500-record / 512 KiB journal persists across restart and marks loss of older history as partial. The activity journal and its atomic-update files cannot appear through directory scanning because the product no longer scans output directories; an explicit self-path event is ignored.
- Existing panel rendering, mouse mapping, and literal-argv default-app opener remain. Keyboard focus requires a visible viewport, and partial/corrupt journal state is surfaced. Rows show provider when space allows; the journal retains tool provenance.

## Validation evidence boundary

The hook tests replay small documented Codex/Cursor payloads into the actual capture helper and create harmless text/HTML fixture outputs. This proves the helper's supported workflow with mock/capture evidence; it is not a live Codex or Cursor session. Current local configs lack the new hook registrations, so live provider delivery remains unproven until the user installs/updates the integration after review.
