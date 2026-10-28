//! The whole transition table for one site, as one match, read top to bottom. Each arm is one row of the design's table.
//! Pure: no clock, no IO. The `Plan` is the site's ordered gates, and the highest live/<n> number git and the state already
//! use, both passed in because "the next gate" and "the next number" need them. An event a state does not expect comes back
//! as a Report with words; nothing panics and nothing is dropped.
use crate::types::*;
use crate::words::describe;

type Step = (ShipState, Vec<Effect>);

pub struct Plan<'a> {
    pub gates: &'a [Gate],
    /// What the site does between its gates and its deploy.
    pub diff: &'a DiffPolicy,
    /// The highest live/<n> in git or in the state. The next deploy's tag is one more; a number is never reused.
    pub highest: u32,
}

pub fn step(state: ShipState, event: ShipEvent, plan: &Plan) -> Step {
    use ShipEvent as E;
    use ShipState as S;
        match (state, event) {
        // Idle: origin's main moving ahead starts the gates. The live commit itself changes nothing.
        (S::Idle { live, previous }, E::OriginMoved { commit }) => {
            if commit == live.commit {
                (S::Idle { live, previous }, vec![])
            } else {
                start(live, commit, plan)
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
            if plan.gates.get(done.len()).map(|g| &g.name) != Some(&gate) {
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
                    match plan.gates.get(done.len()) {
                        Some(following) => (S::Gating { live, candidate, done }, vec![Effect::RunGate(following.clone())]),
                        None => after_gates(live, candidate, plan),
                    }
                }
            }
        }

        // Diffing: the diff decides between going on, waiting for a person, and blocking. A page that vanished with no redirect always blocks.
        (S::Diffing { live, candidate }, E::DiffMade { report }) => {
            if !report.gone_without_redirect.is_empty() {
                let text = format!("{} would remove {} page(s) with no redirect, so nothing is deployed. {}", candidate.short(), report.gone_without_redirect.len(), report.text);
                return (
                    S::Blocked { live, candidate, failed: diff_gate(), report: report.text },
                    vec![Effect::Report(Where::Console, text)],
                );
            }
            // Show goes straight on, unless the diff touches routing or code: those need a person's word, so they wait like Hold.
            let waits = matches!(plan.diff, DiffPolicy::Hold { .. }) || !report.flagged.is_empty();
            if waits {
                let words = describe(&S::Diffed { live: live.clone(), candidate: candidate.clone(), report: report.clone() });
                (
                    S::Diffed { live, candidate, report: report.clone() },
                    vec![Effect::Report(Where::Console, format!("{}\n{}", report.text, words))],
                )
            } else {
                (
                    S::Deploying { previous: live, candidate: candidate.clone() },
                    vec![Effect::Report(Where::Console, report.text), Effect::Deploy(candidate)],
                )
            }
        }
        (S::Diffing { live, candidate }, E::DiffFailed { why }) => {
            let text = format!("the diff for {} could not be made, so nothing is deployed: {}", candidate.short(), why);
            (S::Blocked { live, candidate, failed: diff_gate(), report: why }, vec![Effect::Report(Where::Console, text)])
        }

        // Diffed: only an approval of this commit and this exact diff, with a word for every kind that needs one, deploys.
        (S::Diffed { live, candidate, report }, E::Approved { commit, digest, allow }) => {
            let refusal = if commit != candidate {
                Some(format!("that approval is for {}, but the diff waiting is for {}", commit.short(), candidate.short()))
            } else if digest != report.digest && !(digest.len() >= 12 && report.digest.starts_with(&digest)) {
                Some("that digest is not the digest of the diff waiting; read the diff again".to_string())
            } else if let Some(k) = report.flagged.iter().find(|k| !allow.contains(k)) {
                Some(format!("the diff changes {} files; approve with --allow {} to say that is meant", k.name(), k.name()))
            } else {
                None
            };
            match refusal {
                Some(why) => say(S::Diffed { live, candidate, report }, format!("not approved: {why}")),
                None => (
                    S::Deploying { previous: live, candidate: candidate.clone() },
                    vec![Effect::Report(Where::Console, format!("approved {} with diff {}", candidate.short(), report.digest.get(..12).unwrap_or(&report.digest))), Effect::Deploy(candidate)],
                ),
            }
        }
        (S::Diffed { live, candidate, report }, E::OriginMoved { commit }) => {
            let kept = S::Diffed { live: live.clone(), candidate: candidate.clone(), report };
            waiting(kept, live, candidate, commit, plan)
        }

        // Blocked: nothing deploys until origin moves to a newer commit. The same candidate changes nothing.
        (S::Blocked { live, candidate, failed, report }, E::OriginMoved { commit }) => {
            let kept = S::Blocked { live: live.clone(), candidate: candidate.clone(), failed, report };
            waiting(kept, live, candidate, commit, plan)
        }

        // Deploying: the deploy command's result decides between Verifying and Failed. The tag waits for a verified deploy.
        (S::Deploying { previous, candidate }, E::Deployed { version }) => {
            let now = LiveTag { n: plan.highest + 1, commit: candidate.clone() };
            (S::Verifying { previous, now, version }, vec![Effect::VerifyLive])
        }
        (S::Deploying { previous, candidate }, E::DeployFailed { why }) => {
            let text = format!("deploy of {} failed: {}. Live is still {}", candidate.short(), why, previous.name());
            (S::Failed { live: previous, candidate, why }, vec![Effect::Report(Where::Console, text)])
        }

        // Failed: as Blocked. Live stays where it was, and only a newer commit on origin moves it on.
        (S::Failed { live, candidate, why }, E::OriginMoved { commit }) => {
            let kept = S::Failed { live: live.clone(), candidate: candidate.clone(), why };
            waiting(kept, live, candidate, commit, plan)
        }

        // Verifying: the live checks decide. Passing tags the new live/<n> and makes it live; failing rolls back, with no tag.
        (S::Verifying { previous, now, version }, E::Verified) => {
            let text = format!(
                "{} at {} passed the live checks ({}). To undo: shipd rollback SITE, which redeploys {}",
                now.name(),
                now.commit.short(),
                version.words(),
                previous.name()
            );
            (
                S::Idle { live: now.clone(), previous },
                vec![Effect::Tag(now), Effect::Report(Where::Console, text)],
            )
        }
        (S::Verifying { previous, now, .. }, E::VerifyFailed { why }) => {
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
            waiting(kept, live, rejected.commit, commit, plan)
        }

        // RollbackFailed: reported again on every pass, until a newer commit leaves it for Gating, as Held does.
        (S::RollbackFailed { live, rejected, why }, E::OriginMoved { commit }) => {
            if commit == rejected.commit {
                let text = format!("still not fixed: {} may be live, and the rollback to {} failed: {}", rejected.commit.short(), live.name(), why);
                say(S::RollbackFailed { live, rejected, why }, text)
            } else {
                let kept = S::RollbackFailed { live: live.clone(), rejected: rejected.clone(), why };
                waiting(kept, live, rejected.commit, commit, plan)
            }
        }

        // Resumed: a state that was killed mid-effect runs that effect again. Deploys are idempotent by commit.
        (S::Gating { live, candidate, .. }, E::Resumed) => start(live, candidate, plan),
        (S::Deploying { previous, candidate }, E::Resumed) => (
            S::Deploying { previous: previous.clone(), candidate: candidate.clone() },
            vec![Effect::Deploy(candidate)],
        ),
        (S::Diffing { live, candidate }, E::Resumed) => (
            S::Diffing { live: live.clone(), candidate: candidate.clone() },
            vec![Effect::MakeDiff { live: live.commit, candidate }],
        ),
        (S::Verifying { previous, now, version }, E::Resumed) => (S::Verifying { previous, now, version }, vec![Effect::VerifyLive]),
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

/// Starts the gates on a new candidate, or goes on to the diff step when the site has no gates.
fn start(live: LiveTag, candidate: Commit, plan: &Plan) -> Step {
    match plan.gates.first() {
        Some(first) => (
            ShipState::Gating { live, candidate: candidate.clone(), done: vec![] },
            vec![Effect::FetchWorktree(candidate), Effect::RunGate(first.clone())],
        ),
        None => {
            let (state, mut effects) = after_gates(live, candidate.clone(), plan);
            effects.insert(0, Effect::FetchWorktree(candidate));
            (state, effects)
        }
    }
}

/// Every gate has passed: build both sides and compare when the site has a diff step, or deploy.
fn after_gates(live: LiveTag, candidate: Commit, plan: &Plan) -> Step {
    match plan.diff {
        DiffPolicy::Off => (ShipState::Deploying { previous: live, candidate: candidate.clone() }, vec![Effect::Deploy(candidate)]),
        DiffPolicy::Show { .. } | DiffPolicy::Hold { .. } => {
            (ShipState::Diffing { live: live.clone(), candidate: candidate.clone() }, vec![Effect::MakeDiff { live: live.commit, candidate }])
        }
    }
}

/// The name a diff that blocks goes under, where a gate's name would go.
fn diff_gate() -> GateName {
    GateName::parse("diff").expect("diff is a valid gate name")
}

/// A state that waits on `held_on` (a blocked candidate, or a rejected commit): origin at that commit changes nothing,
/// origin back at live goes to Idle, and any other commit starts the gates.
fn waiting(kept: ShipState, live: LiveTag, held_on: Commit, commit: Commit, plan: &Plan) -> Step {
    if commit == held_on {
        (kept, vec![])
    } else if commit == live.commit {
        (ShipState::Idle { live: live.clone(), previous: live }, vec![])
    } else {
        start(live, commit, plan)
    }
}

fn say(state: ShipState, text: String) -> Step {
    (state, vec![Effect::Report(Where::Console, text)])
}

fn event_name(event: &ShipEvent) -> &'static str {
    match event {
        ShipEvent::OriginMoved { .. } => "a new commit on origin",
        ShipEvent::GateDone { .. } => "a gate result",
        ShipEvent::DiffMade { .. } | ShipEvent::DiffFailed { .. } => "a diff result",
        ShipEvent::Approved { .. } => "an approval",
        ShipEvent::Deployed { .. } | ShipEvent::DeployFailed { .. } => "a deploy result",
        ShipEvent::Verified | ShipEvent::VerifyFailed { .. } => "a live check result",
        ShipEvent::RollbackRequested => "a rollback request",
        ShipEvent::RollbackDone | ShipEvent::RedeployFailed { .. } => "a redeploy result",
        ShipEvent::Resumed => "a restart",
    }
}
