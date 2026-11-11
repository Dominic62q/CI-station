# Station plugin for Claude Code

Sends minimal metadata about Claude Code sessions and tool calls to a local
spool file that the Station desktop app reads. Nothing leaves your machine.

## What it records

One JSON line per hook event in `~/.station/spool/claude-YYYY-MM-DD.jsonl`
(override with `STATION_SPOOL_DIR`):

- event name, timestamp, session ID, working directory, transcript path
- tool name and tool-use ID
- the file path for Edit / Write / MultiEdit / NotebookEdit
- for Bash: only the program name (e.g. `pytest`) and whether it looks like a test command
- ok / failed for completed tool calls

It does **not** record prompts, file contents, full commands or arguments,
tool output, or environment variables.

## Safety

Every hook runs with `async: true`, prints nothing, and exits 0 even on
error, so it cannot block or change Claude's work. If Station isn't running,
events wait in the spool file until it starts.

## Install for testing

```bash
claude --plugin-dir ./plugin/station
```

Disable or uninstall the plugin to stop collection. Your own `settings.json`
hooks are never touched. Delete `~/.station/spool/` to remove collected data.
