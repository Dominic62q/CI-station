"""Station hook: append one minimal metadata line per Claude Code hook event.

Rules this script keeps:
- Fail open. Any error is swallowed and the script exits 0.
- Print nothing to stdout (Claude Code would read it as hook output).
- Store metadata only: no prompts, no file contents, no full command text,
  no tool output, no environment variables.
- One write() per event with O_APPEND, so concurrent sessions don't interleave.

Spool location: $STATION_SPOOL_DIR, else ~/.station/spool. One file per UTC day.
"""

import json
import os
import re
import shlex
import sys
import time
import uuid

SCHEMA_VERSION = 1
MAX_STDIN = 4 * 1024 * 1024  # ignore anything bigger rather than stall

FILE_TOOLS = {"Edit", "Write", "MultiEdit", "NotebookEdit"}
TEST_RE = re.compile(
    r"(^|[\s/;&|(])("
    r"pytest|py\.test|manage\.py\s+test|tox|nox|"
    r"jest|vitest|mocha|playwright\s+test|"
    r"(npm|pnpm|yarn|bun)\s+(run\s+)?test|"
    r"cargo\s+(test|nextest)|go\s+test|rspec|phpunit|mix\s+test|"
    r"(deno|swift|dotnet|mvn|gradle|gradlew|make)\s+(test|check)|"
    r"python[\d.]*\s+-m\s+(pytest|unittest)|"
    r"python[\d.]*\s+[^\s;&|]*test[^\s;&|]*\.py"
    r")(\s|$)"
)


SETUP_CMDS = {"cd", "pushd", "popd", "export", "source", ".", "set", "env"}


def command_head(cmd):
    """Program name of the first real step in a shell command.

    Skips VAR=value prefixes and setup steps like `cd x &&`. Only the
    program name is kept, never its arguments.
    """
    first = None
    for segment in re.split(r"&&|\|\||;|\|", cmd):
        try:
            parts = shlex.split(segment, posix=True)
        except ValueError:
            parts = segment.split()
        parts = [p for p in parts if not re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", p)]
        if not parts:
            continue
        head = os.path.basename(parts[0])[:64]
        first = first or head
        if head not in SETUP_CMDS:
            return head
    return first


def build_record(payload):
    event = payload.get("hook_event_name")
    rec = {
        "v": SCHEMA_VERSION,
        "id": str(uuid.uuid4()),
        "ts_ms": int(time.time() * 1000),
        "event": event,
        "session_id": payload.get("session_id"),
        "cwd": payload.get("cwd"),
    }
    for key in ("agent_id", "agent_type", "transcript_path"):
        if payload.get(key):
            rec[key] = payload[key]

    if event == "SessionStart" and payload.get("source"):
        rec["source"] = payload["source"]
    if event == "SessionEnd" and payload.get("reason"):
        rec["reason"] = payload["reason"]
    if event == "Notification" and payload.get("notification_type"):
        rec["notification_type"] = payload["notification_type"]

    tool = payload.get("tool_name")
    if tool:
        rec["tool"] = tool
        if payload.get("tool_use_id"):
            rec["tool_use_id"] = payload["tool_use_id"]
        tin = payload.get("tool_input") or {}
        if isinstance(tin, dict):
            if tool in FILE_TOOLS:
                path = tin.get("file_path") or tin.get("notebook_path")
                if isinstance(path, str):
                    rec["file"] = path
            elif tool == "Bash" and isinstance(tin.get("command"), str):
                cmd = tin["command"]
                rec["cmd_head"] = command_head(cmd)
                if TEST_RE.search(cmd):
                    rec["cmd_kind"] = "test"  # inferred from the command text
        if event == "PostToolUse":
            rec["status"] = "ok"
        elif event == "PostToolUseFailure":
            rec["status"] = "failed"
    return rec


def spool_path():
    base = os.environ.get("STATION_SPOOL_DIR") or os.path.join(
        os.path.expanduser("~"), ".station", "spool"
    )
    os.makedirs(base, mode=0o700, exist_ok=True)
    day = time.strftime("%Y-%m-%d", time.gmtime())
    return os.path.join(base, f"claude-{day}.jsonl")


def main():
    raw = sys.stdin.buffer.read(MAX_STDIN + 1)
    if not raw or len(raw) > MAX_STDIN:
        return
    payload = json.loads(raw)
    if not isinstance(payload, dict) or not payload.get("hook_event_name"):
        return
    line = (json.dumps(build_record(payload), separators=(",", ":")) + "\n").encode()
    fd = os.open(spool_path(), os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(fd, line)
    finally:
        os.close(fd)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        pass
    sys.exit(0)
