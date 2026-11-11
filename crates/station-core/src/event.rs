//! Normalized event model. One struct per spool line; stored as one row.

use serde::{Deserialize, Serialize};

/// Highest spool schema version this build understands.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// Where an event's facts came from.
///
/// `Observed` means a hook or process reported it directly.
/// `Inferred` means Station derived it (for example "this Bash command looks
/// like a test run"). The UI should show the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Observed,
    Inferred,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Observed => "observed",
            Provenance::Inferred => "inferred",
        }
    }
}

/// One spool line, as written by the Claude plugin. Unknown fields are
/// ignored so newer plugins stay readable by older builds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub v: u32,
    pub id: String,
    pub ts_ms: i64,
    pub event: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// Absolute path, only for file-editing tools.
    #[serde(default)]
    pub file: Option<String>,
    /// Program name of a Bash command, never its arguments.
    #[serde(default)]
    pub cmd_head: Option<String>,
    /// `"test"` when the command text looked like a test run (inferred).
    #[serde(default)]
    pub cmd_kind: Option<String>,
    /// `"ok"` or `"failed"` for completed tool calls.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub notification_type: Option<String>,
}

impl Event {
    pub fn provenance_of_command_kind(&self) -> Provenance {
        if self.cmd_kind.is_some() {
            Provenance::Inferred
        } else {
            Provenance::Observed
        }
    }
}
