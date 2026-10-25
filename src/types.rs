//! The domain: what a site's ship state is made of, and what the machine can ask for. No IO lives here.
//! Text is parsed at the edge (config, git output, the command line) into these types, so nothing past it guesses.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A full 40-character lowercase commit id, as git prints it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Commit(String);

impl Commit {
    pub fn parse(text: &str) -> Result<Commit, String> {
        let t = text.trim();
        if t.len() == 40 && t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            Ok(Commit(t.to_string()))
        } else {
            Err(format!("{t:?} is not a full commit id (40 lowercase hex characters)"))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn short(&self) -> &str {
        &self.0[..7]
    }
}

/// A deployed commit's number: the tag `live/<n>` names it, and the commit is what it points at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveTag {
    pub n: u32,
    pub commit: Commit,
}

impl LiveTag {
    pub fn name(&self) -> String {
        format!("live/{}", self.n)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GateName(String);

impl GateName {
    pub fn parse(text: &str) -> Result<GateName, String> {
        let t = text.trim();
        if t.is_empty() || t.contains('\n') {
            Err(format!("gate name {t:?} must be one line of words"))
        } else {
            Ok(GateName(t.to_string()))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One check that must pass before anything deploys: its name, and the command run in the candidate's worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    pub name: GateName,
    pub command: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateOutcome {
    Passed,
    Failed { report: String },
}

/// Where a `Report` is said. Stage 1 says it only on the console and in the history file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Where {
    Console,
}

/// One site's ship state. Each variant says what the machine is waiting for, and every variant carries its words.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ShipState {
    /// Live is what origin's main was when shipd last settled. `previous` is the tag a rollback goes back to.
    Idle { live: LiveTag, previous: LiveTag },
    Gating { live: LiveTag, candidate: Commit, done: Vec<GateName> },
    /// A gate failed. Nothing deploys from here until origin moves to a newer commit.
    Blocked { live: LiveTag, candidate: Commit, failed: GateName, report: String },
    Deploying { previous: LiveTag, candidate: Commit },
    /// The deploy ran. The live URLs are being checked, and `now` is the tag that would be live.
    Verifying { previous: LiveTag, now: LiveTag },
    /// The deploy command itself failed. Live is still `live`.
    Failed { live: LiveTag, candidate: Commit, why: String },
    /// The live check failed, or a person asked. The previous tag is being redeployed.
    RollingBack { failed: LiveTag, to: LiveTag, why: String },
    /// A rollback finished. Main is ahead of live on purpose: `rejected` stays on main and is not redeployed until main moves past it.
    Held { live: LiveTag, rejected: Commit, why: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ShipEvent {
    OriginMoved { commit: Commit },
    GateDone { gate: GateName, outcome: GateOutcome },
    Deployed,
    DeployFailed { why: String },
    Verified,
    VerifyFailed { why: String },
    RollbackRequested,
    RollbackDone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    FetchWorktree(Commit),
    RunGate(Gate),
    Deploy(Commit),
    Tag(LiveTag),
    VerifyLive,
    Redeploy(LiveTag),
    Report(Where, String),
}

/// Whether a site's deploy is real. Shipd never flips this itself: only the config's `mode` line does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    DryRun,
    Live,
}

/// One URL the live site must answer with 200 and without any of the strings the change removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub url: String,
    pub must_not_contain: Vec<String>,
}

/// One registered site: a row of shipd.toml, parsed at the edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    pub name: String,
    pub repo: PathBuf,
    pub branch: String,
    pub mode: Mode,
    pub gates: Vec<Gate>,
    pub deploy: Vec<String>,
    pub verify: Vec<Check>,
}
