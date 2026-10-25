//! The whole transition table for one site, as one match, read top to bottom. Each arm is one row of the design's table.
//! Pure: no clock, no IO. The `Plan` is the site's ordered gates, and the highest live/<n> number git and the state already
//! use, both passed in because "the next gate" and "the next number" need them. An event a state does not expect comes back
//! as a Report with words; nothing panics and nothing is dropped.
use crate::types::*;
use crate::words::describe;

type Step = (ShipState, Vec<Effect>);

pub struct Plan<'a> {
    pub gates: &'a [Gate],
    /// The highest live/<n> in git or in the state. The next deploy's tag is one more; a number is never reused.
    pub highest: u32,
}

pub fn step(state: ShipState, event: ShipEvent, plan: &Plan) -> Step {
    use ShipEvent as E;
    use ShipState as S;
    let gates = plan.gates;
    match (state, event) {
        // Idle: origin's main moving ahead starts the gates. The live commit itself changes nothing.
        (S::Idle { live, previous }, E::OriginMoved { commit }) => {
            if commit == live.commit {
                (S::Idle { live, previous }, vec![])
            } else {
                start(live, commit, gates)
            }
        }
        (S::Idle { live, previous }, E::RollbackRequested) => {
            if previous.n == live.n {
                say(S::Idle { live, previous }, "there is no earlier live tag to go back to".into())
            } else {
                let text = format!("rolling back from {} to {}", live.name(), previous.name());
                (
                    S::RollingBack { failed: live, to: previous.clone(), why: "asked to roll back".into() },
                    vec![Effect::Redeploy(previous), Effect::Report(Where::Console, text)],
                )
            }
        }

        // Gating: each gate result moves to the next gate, or to Deploying once none is left. A failure blocks.
        (S::Gating { live, candidate, done }, E::GateDone { gate, outcome }) => {
            if gates.get(done.len()).map(|g| &g.name) != Some(&gate) {
                let text = format!("gate {} came back out of order; nothing changes", gate.as_str());
                return say(S::Gating { live, candidate, done }, text);
            }
            match outcome {
                GateOutcome::Failed { report } => {
                    let text = format!("gate {} failed on {}; nothing is deployed. {}", gate.as_str(), candidate.short(), report);
                    (
                        S::Blocked { live, candidate, failed: gate, report },
                        vec![Effect::Report(Where::Console, text)],
                    )
                }
                GateOutcome::Passed => {
                    let mut done = done;
                    done.push(gate);
                    match gates.get(done.len()) {
                        Some(following) => (S::Gating { live, candidate, done }, vec![Effect::RunGate(following.clone())]),
                        None => (
                            S::Deploying { previous: live, candidate: candidate.clone() },
                            vec![Effect::Deploy(candidate)],
                        ),
                    }
                }
            }
        }

        // Blocked: nothing deploys until origin moves to a newer commit. The same candidate changes nothing.
        (S::Blocked { live, candidate, failed, report }, E::OriginMoved { commit }) => {
            let kept = S::Blocked { live: live.clone(), candidate: candidate.clone(), failed, report };
            waiting(kept, live, candidate, commit, gates)
        }

        // Deploying: the deploy command's result decides between Verifying and Failed. The tag waits for a verified deploy.
        (S::Deploying { previous, candidate }, E::Deployed) => {
            let now = LiveTag { n: plan.highest + 1, commit: candidate.clone() };
            (S::Verifying { previous, now }, vec![Effect::VerifyLive])
        }
        (S::Deploying { previous, candidate }, E::DeployFailed { why }) => {
            let text = format!("deploy of {} failed: {}. Live is still {}", candidate.short(), why, previous.name());
            (S::Failed { live: previous, candidate, why }, vec![Effect::Report(Where::Console, text)])
        }

        // Failed: as Blocked. Live stays where it was, and only a newer commit on origin moves it on.
        (S::Failed { live, candidate, why }, E::OriginMoved { commit }) => {
            let kept = S::Failed { live: live.clone(), candidate: candidate.clone(), why };
            waiting(kept, live, candidate, commit, gates)
        }

        // Verifying: the live checks decide. Passing tags the new live/<n> and makes it live; failing rolls back, with no tag.
        (S::Verifying { previous, now }, E::Verified) => {
            let text = format!("{} at {} passed the live checks", now.name(), now.commit.short());
            (
                S::Idle { live: now.clone(), previous },
                vec![Effect::Tag(now), Effect::Report(Where::Console, text)],
            )
        }
        (S::Verifying { previous, now }, E::VerifyFailed { why }) => {
            let text = format!("the live check failed: {}. Rolling back to {}", why, previous.name());
            (
                S::RollingBack { failed: now, to: previous.clone(), why: why.clone() },
                vec![Effect::Redeploy(previous), Effect::Report(Where::Console, text)],
            )
        }

        // RollingBack: when the redeploy is done, live is the previous tag, and the failed commit is held on purpose.
        (S::RollingBack { failed, to, why }, E::RollbackDone) => {
            let text = format!(
                "main is ahead of live on purpose: {} stays on main while live is {}. {}",
                failed.commit.short(),
                to.name(),
                why
            );
            (
                S::Held { live: to, rejected: failed, why },
                vec![Effect::Report(Where::Console, text)],
            )
        }
        // The redeploy itself failed: the rejected commit may still be live. This is loud, and it stays loud.
        (S::RollingBack { failed, to, why }, E::RedeployFailed { why: redeploy_why }) => {
            let text = format!(
                "rollback to {} failed: {}. {} may still be live; nothing has fixed it",
                to.name(),
                redeploy_why,
                failed.commit.short()
            );
            (
                S::RollbackFailed { live: to, rejected: failed, why },
                vec![Effect::Report(Where::Console, text)],
            )
        }

        // Held: the rejected commit is not redeployed. A newer commit starts the gates again.
        (S::Held { live, rejected, why }, E::OriginMoved { commit }) => {
            let kept = S::Held { live: live.clone(), rejected: rejected.clone(), why };
            waiting(kept, live, rejected.commit, commit, gates)
        }

        // RollbackFailed: reported again on every pass, until a newer commit leaves it for Gating, as Held does.
        (S::RollbackFailed { live, rejected, why }, E::OriginMoved { commit }) => {
            if commit == rejected.commit {
                let text = format!("still not fixed: {} may be live, and the rollback to {} failed: {}", rejected.commit.short(), live.name(), why);
                say(S::RollbackFailed { live, rejected, why }, text)
            } else {
                let kept = S::RollbackFailed { live: live.clone(), rejected: rejected.clone(), why };
                waiting(kept, live, rejected.commit, commit, gates)
            }
        }

        // Resumed: a state that was killed mid-effect runs that effect again. Deploys are idempotent by commit.
        (S::Gating { live, candidate, .. }, E::Resumed) => start(live, candidate, gates),
        (S::Deploying { previous, candidate }, E::Resumed) => (
            S::Deploying { previous: previous.clone(), candidate: candidate.clone() },
            vec![Effect::Deploy(candidate)],
        ),
        (S::Verifying { previous, now }, E::Resumed) => (S::Verifying { previous, now }, vec![Effect::VerifyLive]),
        (S::RollingBack { failed, to, why }, E::Resumed) => (
            S::RollingBack { failed, to: to.clone(), why },
            vec![Effect::Redeploy(to)],
        ),

        // Anything else: the state keeps what it has, and the report says why nothing changed.
        (s, e) => {
            let text = format!("{} ignores {}; nothing changes", describe(&s), event_name(&e));
            say(s, text)
        }
    }
}

/// Starts the gates on a new candidate, or goes straight to the deploy when the site has no gates.
fn start(live: LiveTag, candidate: Commit, gates: &[Gate]) -> Step {
    match gates.first() {
        Some(first) => (
            ShipState::Gating { live, candidate: candidate.clone(), done: vec![] },
            vec![Effect::FetchWorktree(candidate), Effect::RunGate(first.clone())],
        ),
        None => (
            ShipState::Deploying { previous: live, candidate: candidate.clone() },
            vec![Effect::Deploy(candidate)],
        ),
    }
}

/// A state that waits on `held_on` (a blocked candidate, or a rejected commit): origin at that commit changes nothing,
/// origin back at live goes to Idle, and any other commit starts the gates.
fn waiting(kept: ShipState, live: LiveTag, held_on: Commit, commit: Commit, gates: &[Gate]) -> Step {
    if commit == held_on {
        (kept, vec![])
    } else if commit == live.commit {
        (ShipState::Idle { live: live.clone(), previous: live }, vec![])
    } else {
        start(live, commit, gates)
    }
}

fn say(state: ShipState, text: String) -> Step {
    (state, vec![Effect::Report(Where::Console, text)])
}

fn event_name(event: &ShipEvent) -> &'static str {
    match event {
        ShipEvent::OriginMoved { .. } => "a new commit on origin",
        ShipEvent::GateDone { .. } => "a gate result",
        ShipEvent::Deployed | ShipEvent::DeployFailed { .. } => "a deploy result",
        ShipEvent::Verified | ShipEvent::VerifyFailed { .. } => "a live check result",
        ShipEvent::RollbackRequested => "a rollback request",
        ShipEvent::RollbackDone | ShipEvent::RedeployFailed { .. } => "a redeploy result",
        ShipEvent::Resumed => "a restart",
    }
}
