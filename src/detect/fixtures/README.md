# Stopped agent errors

`stopped-errors.tsv` stores only error rows, without conversation contents.
Literal `\n` characters reproduce the surrounding prompt and wrapped row forms.

Evidence used for these rows:

- Codex 0.163.0's installed native diagnostics: `Selected model is at capacity.
  Please try a different model.`, `Flex capacity unavailable.`, typographic
  `You’ve hit your usage limit`, `Quota exceeded. Check your plan and billing
  details.`, `Your access token could not be refreshed`, and the four final
  transport error prefixes. Its error cell uses `■`; a newer `•` block clears
  the previous error. Transport rows contain the diagnostic prefix only, rather
  than inventing an underlying network exception. The capacity incident was
  also captured in the task report.
- Claude Code's synthetic API error messages in its local project logs: weekly
  limit, expired login, DNS failure and
  interrupted response. The terminal error child row uses `⎿`. Ordinary `●`
  response rows do not match. The installed renderer also contains the generic
  `API Error:` form and `You've hit your limit` diagnostic.
- Cursor Agent 2026.10.16-e3cb207's bundled `src/utils/error-display.ts`, in
  `index.js`: errors render as `Error: ${title}` and persist until input;
  connection failed/stalled and generic server error strings, plus the DNS and
  unreachable-network formatter. `9136.index.js` contains the credential and
  stored authentication messages. Dynamic error titles use that same error-row
  form; title words choose the short reason only after the structural match.

These are captured diagnostic strings and source-rendered screens. No provider
request, credential expiration or deliberate live failure is needed to run the
tests. A confirmed foreground process returning to its shell is tested through
state events and the offline native fixture; it has no error-text pattern.

# Completed turns and live activity

- `claude-done-shells.txt` is the exact captured lower screen, from the completed
  summary through the prompt and status footer. Conversation text is omitted.
  It includes the completion clock, update notice, two shells and two agents.
- `claude-done-agents.txt` is a derived isolation case: only shell counters are
  removed from that capture; the agent counter and completion remain.
- `claude-working.txt` reuses the real screen recorded by the existing
  `claude_spinner_above_prompt_box_is_working` test, including its tip row.
- `claude-long-working-shells.txt` is a derived stress case from that working
  screen: the elapsed duration is extended to 1h 30m 9s and two remaining shells
  are added. It is not a separate live capture of a ninety-minute turn.

The regression tests combine these recorded UI rows to exercise stale activity
above a completion and fresh activity below it. Those combinations are derived
frames, not claims of additional live provider turns.
