//! The domain: what a site's ship state is made of, and what the machine can ask for. No IO lives here.
//! Text is parsed at the edge (config, git output, the command line) into these types, so nothing past it guesses.
use serde::{Deserialize, Serialize};
use crate::diff::{DiffReport, PathKind};
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

impl ShipState {
    /// The highest live/<n> number this state has used, rejected ones included: a number is never reused.
    pub fn highest_number(&self) -> u32 {
        let tags: Vec<&LiveTag> = match self {
            ShipState::Idle { live, previous } => vec![live, previous],
            ShipState::Gating { live, .. } | ShipState::Diffing { live, .. } | ShipState::Diffed { live, .. } | ShipState::Blocked { live, .. } | ShipState::Failed { live, .. } => vec![live],
            ShipState::Deploying { previous, .. } => vec![previous],
            ShipState::Verifying { previous, now, .. } => vec![previous, now],
            ShipState::RollingBack { failed, to, .. } => vec![failed, to],
            ShipState::Held { live, rejected, .. } | ShipState::RollbackFailed { live, rejected, .. } => vec![live, rejected],
        };
        tags.iter().map(|t| t.n).max().unwrap_or(0)
    }

    /// A state a process can be killed in the middle of: its effect has to run again.
    pub fn is_mid_flight(&self) -> bool {
        matches!(self, ShipState::Gating { .. } | ShipState::Diffing { .. } | ShipState::Deploying { .. } | ShipState::Verifying { .. } | ShipState::RollingBack { .. })
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
    /// The gates passed. The live commit and the candidate are being built into two folders to be compared.
    Diffing { live: LiveTag, candidate: Commit },
    /// The diff was made and shown, and a person has to approve exactly this diff (its digest) before anything deploys.
    Diffed { live: LiveTag, candidate: Commit, report: DiffReport },
    Deploying { previous: LiveTag, candidate: Commit, built: Built },
    /// The deploy ran. The live URLs are being checked, and `now` is the tag that would be live. `version` is the platform's id for it.
    Verifying { previous: LiveTag, now: LiveTag, version: VersionId },
    /// The deploy command itself failed. Live is still `live`.
    Failed { live: LiveTag, candidate: Commit, why: String },
    /// The live check failed, or a person asked. The previous tag is being redeployed.
    RollingBack { failed: LiveTag, to: LiveTag, why: String },
    /// A rollback finished. Main is ahead of live on purpose: `rejected` stays on main and is not redeployed until main moves past it.
    Held { live: LiveTag, rejected: LiveTag, why: String },
    /// The rollback's redeploy failed. `rejected` may still be live. This is loud: it is reported on every pass until a newer commit arrives.
    RollbackFailed { live: LiveTag, rejected: LiveTag, why: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ShipEvent {
    OriginMoved { commit: Commit },
    GateDone { gate: GateName, outcome: GateOutcome },
    /// The diff of the live build against the candidate's build was made.
    DiffMade { report: DiffReport },
    DiffFailed { why: String },
    /// A person approved the diff with this digest for this commit, and said these kinds of change (routing, code) were meant.
    Approved { commit: Commit, digest: String, allow: Vec<PathKind> },
    Deployed { version: VersionId },
    /// The files a release produced were committed on the branch: this commit is the branch head, and the live tag names it, so it is not a new candidate.
    Landed { commit: Commit },
    DeployFailed { why: String },
    Verified,
    VerifyFailed { why: String },
    RollbackRequested,
    RollbackDone,
    RedeployFailed { why: String },
    /// Sent first on a pass that finds a state mid-flight (a process killed during an effect): the state re-runs its effect.
    Resumed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    FetchWorktree(Commit),
    /// Build `live` and `candidate` into two folders and compare them.
    MakeDiff { live: Commit, candidate: Commit },
    RunGate(Gate),
    Deploy { commit: Commit, built: Built },
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

/// What the deploy ships. With a diff step, the folder that was built and approved is kept, and the deploy is handed it and held to its digest;
/// without one, the site's deploy command builds for itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "built", rename_all = "snake_case")]
pub enum Built {
    BySiteDeploy,
    KeptFolder { tree_digest: String },
}

/// Where a site's branch lives: on a remote (fetched before every pass), or only in the local repo (for a site with no remote).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchSource {
    Remote { name: String },
    Local,
}

/// What a site does between its gates and its deploy. `Show` builds both and prints the diff, then goes on (unless the diff needs a
/// word or loses a page). `deploy_files` are git pathspecs of the site's own deploy files (its deploy script, its wrangler config): a change in any of
/// them between the live commit and the candidate is listed and needs `--allow deploy`, though the build output is identical. `Hold` builds both, prints the diff and waits for `shipd approve`. The build command writes the built site to $SHIPD_OUT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffPolicy {
    Off,
    Show { build: Vec<String>, deploy_files: Vec<String> },
    Hold { build: Vec<String>, deploy_files: Vec<String> },
}

/// Where the platform's id for a deploy is found: on the line of the deploy command's output that starts with `prefix`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionSource {
    NotReported,
    DeployLine { prefix: String },
}

/// The platform's id for a deploy, or the plain fact that the deploy did not report one. Never an empty string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "version", content = "id", rename_all = "snake_case")]
pub enum VersionId {
    Found(String),
    NotReported,
}

impl VersionId {
    pub fn words(&self) -> String {
        match self {
            VersionId::Found(id) => format!("version {id}"),
            VersionId::NotReported => "no version id reported".into(),
        }
    }
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
    pub source: BranchSource,
    pub mode: Mode,
    pub gates: Vec<Gate>,
    pub deploy: Vec<String>,
    pub version: VersionSource,
    pub diff: DiffPolicy,
    /// Run in every fresh checkout before anything else (the checkout is cleaned of untracked files first). Empty: nothing to install.
    pub install: Vec<String>,
    /// Files a release produces in its checkout that belong on the branch (for example a list of published addresses): committed by path after a verified deploy.
    pub land: Vec<String>,
    /// The longest any one command of this site may run.
    pub command_limit: std::time::Duration,
    pub verify: Vec<Check>,
}
