//! The words for a state, in plain English. `status` prints these, and the step table puts them in reports.
use crate::types::ShipState;

pub fn describe(state: &ShipState) -> String {
    match state {
        ShipState::Idle { live, .. } => format!("idle: live is {} at {}, and origin has nothing newer", live.name(), live.commit.short()),
        ShipState::Gating { live, candidate, done } => format!(
            "gating {} for live/{}: {} gate(s) passed so far",
            candidate.short(),
            live.n,
            done.len()
        ),
        ShipState::Blocked { live, candidate, failed, .. } => format!(
            "blocked: {} failed the {} gate. Live is {}; nothing from {} is deployed",
            candidate.short(),
            failed.as_str(),
            live.name(),
            candidate.short()
        ),
        ShipState::Diffing { live, candidate } => format!("building {} and {} to compare them", live.name(), candidate.short()),
        ShipState::Diffed { candidate, report, .. } => format!(
            "waiting for approval: the diff for {} is shown. Approve with `shipd approve SITE {}{}`",
            candidate.short(),
            report.digest.get(..12).unwrap_or(&report.digest),
            if report.flagged.is_empty() { String::new() } else { format!(" --allow {}", report.flagged.iter().map(|k| k.name()).collect::<Vec<_>>().join(",")) }
        ),
        ShipState::Deploying { previous, candidate, .. } => format!("deploying {} over {}", candidate.short(), previous.name()),
        ShipState::Verifying { now, .. } => format!("checking {} ({}) against the live checks", now.name(), now.commit.short()),
        ShipState::Failed { live, candidate, why } => format!(
            "deploy of {} failed: {}. Live is still {}",
            candidate.short(),
            why,
            live.name()
        ),
        ShipState::RollingBack { to, why, .. } => format!("rolling back to {} ({})", to.name(), why),
        ShipState::Held { live, rejected, why } => format!(
            "held: main is ahead of live on purpose. {} is on main, live is {}, and nothing deploys until main moves past it ({})",
            rejected.commit.short(),
            live.name(),
            why
        ),
        ShipState::RollbackFailed { live, rejected, why } => format!(
            "ROLLBACK FAILED: the rollback to {} did not deploy ({}). {} may still be live. Nothing fixes it until main moves past {}",
            live.name(),
            why,
            rejected.commit.short(),
            rejected.commit.short()
        ),
    }
}
