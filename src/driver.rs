//! The adapter: runs what `step` asks for, and turns each result back into an event. Git, gates, the deploy command and the
//! live checks happen here, never in `step`. In dry-run the Deploy, Tag and Redeploy effects are only printed, the live
//! checks run only the status check, and the machine proceeds as if they passed. In live mode everything is real.
use crate::step::{step, Plan};
use crate::store::{self, Paths};
use crate::types::*;
use crate::words::describe;
use serde_json::json;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Run<'a> {
    pub site: &'a Site,
    pub paths: &'a Paths,
    pub dry: bool,
}

/// Whether this run deploys for real: the config says live, and the command line did not say --dry-run.
pub fn is_dry(site: &Site, dry_flag: bool) -> bool {
    dry_flag || site.mode == Mode::DryRun
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Runs a command in `dir`, with SHIPD_TARGET set to the site's shared build directory. Returns success and the output.
fn run_in(cmd: &[String], dir: &Path, target: &Path) -> (bool, String) {
    let Some((prog, rest)) = cmd.split_first() else {
        return (false, "the command is empty".into());
    };
    match Command::new(prog).args(rest).current_dir(dir).env("SHIPD_TARGET", target).output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            (out.status.success(), text)
        }
        Err(e) => (false, format!("cannot run {prog}: {e}")),
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The site's clean worktree, checked out at `commit`. Made once with `git worktree add`, then moved with checkout.
fn ensure_work(run: &Run, commit: &Commit) -> Result<PathBuf, String> {
    let work = run.paths.work(&run.site.name);
    if work.exists() {
        git(&work, &["checkout", "--detach", "-f", commit.as_str()])?;
    } else {
        std::fs::create_dir_all(run.paths.site_dir(&run.site.name)).map_err(|e| e.to_string())?;
        let work_text = work.to_string_lossy().to_string();
        git(&run.site.repo, &["worktree", "add", "--detach", &work_text, commit.as_str()])?;
    }
    Ok(work)
}

/// Removes the site's worktree, so nothing is left behind after a pass.
pub fn clean_work(run: &Run) {
    let work = run.paths.work(&run.site.name);
    if work.exists() {
        let work_text = work.to_string_lossy().to_string();
        let _ = git(&run.site.repo, &["worktree", "remove", "--force", &work_text]);
        let _ = std::fs::remove_dir_all(&work);
        let _ = git(&run.site.repo, &["worktree", "prune"]);
    }
}

/// The highest live/<n> in git, or 0. Tags are never deleted, so this only grows.
fn highest_tag_in_git(site: &Site) -> Result<u32, String> {
    let names = git(&site.repo, &["tag", "--list", "live/*"])?;
    Ok(names.lines().filter_map(|l| l.strip_prefix("live/")?.parse::<u32>().ok()).max().unwrap_or(0))
}

/// Feeds events through `step` until none are left, running each effect and recording it. Saves the state after each event.
/// The highest number is read from git and the state once per call, so a number that was used is never handed out again.
pub fn settle(run: &Run, state: ShipState, events: Vec<ShipEvent>) -> Result<ShipState, String> {
    let name = &run.site.name;
    let highest = highest_tag_in_git(run.site)?.max(state.highest_number());
    let mut queue: VecDeque<ShipEvent> = events.into();
    let mut state = state;
    while let Some(event) = queue.pop_front() {
        store::append(run.paths, name, "event", json!({ "event": event }))?;
        let plan = Plan { gates: &run.site.gates, highest };
        let (next, effects) = step(state, event, &plan);
        state = next;
        store::save_state(run.paths, name, &state)?;
        for effect in effects {
            store::append(run.paths, name, "effect", json!({ "effect": effect_words(&effect) }))?;
            for follow in execute(run, effect)? {
                queue.push_back(follow);
            }
        }
    }
    Ok(state)
}

fn effect_words(effect: &Effect) -> String {
    match effect {
        Effect::FetchWorktree(c) => format!("fetch a worktree at {}", c.short()),
        Effect::RunGate(g) => format!("run gate \"{}\"", g.name.as_str()),
        Effect::Deploy(c) => format!("deploy {}", c.short()),
        Effect::Tag(t) => format!("tag {}", t.name()),
        Effect::VerifyLive => "check the live site".into(),
        Effect::Redeploy(t) => format!("redeploy {}", t.name()),
        Effect::Report(_, text) => format!("report: {text}"),
    }
}

/// Does the effect for real (or, in dry-run, as the print says), and returns the events its result gives back.
fn execute(run: &Run, effect: Effect) -> Result<Vec<ShipEvent>, String> {
    let site = run.site;
    let target = run.paths.target(&site.name);
    match effect {
        Effect::FetchWorktree(commit) => {
            ensure_work(run, &commit)?;
            Ok(vec![])
        }
        Effect::RunGate(gate) => {
            let work = run.paths.work(&site.name);
            let (ok, text) = run_in(&gate.command, &work, &target);
            if ok {
                Ok(vec![ShipEvent::GateDone { gate: gate.name, outcome: GateOutcome::Passed }])
            } else {
                let report = tail(&text, 40);
                store::append(run.paths, &site.name, "gate_failed", json!({ "gate": gate.name.as_str(), "report": report }))?;
                Ok(vec![ShipEvent::GateDone { gate: gate.name, outcome: GateOutcome::Failed { report } }])
            }
        }
        Effect::Deploy(commit) => {
            if run.dry {
                say(format!("would deploy {} to {}", commit.short(), site.name));
                store::append(run.paths, &site.name, "would_have_deployed", json!({ "commit": commit.as_str() }))?;
                return Ok(vec![ShipEvent::Deployed]);
            }
            let work = ensure_work(run, &commit)?;
            let (ok, text) = run_in(&site.deploy, &work, &target);
            if ok {
                say(format!("deployed {} to {}", commit.short(), site.name));
                Ok(vec![ShipEvent::Deployed])
            } else {
                Ok(vec![ShipEvent::DeployFailed { why: tail(&text, 20) }])
            }
        }
        Effect::Tag(tag) => {
            if run.dry {
                say(format!("would tag {} at {}: that would be live", tag.name(), tag.commit.short()));
                return Ok(vec![]);
            }
            // a tag that exists is never moved: a rolled-back commit keeps its number
            if !git(&site.repo, &["tag", "--list", &tag.name()])?.is_empty() {
                return Err(format!("{} already exists; tags are never moved, so the number is not reused", tag.name()));
            }
            git(&site.repo, &["tag", &tag.name(), tag.commit.as_str()])?;
            say(format!("tagged {} at {}", tag.name(), tag.commit.short()));
            Ok(vec![])
        }
        Effect::VerifyLive => Ok(match verify(site, run.dry) {
            None => vec![ShipEvent::Verified],
            Some(why) => vec![ShipEvent::VerifyFailed { why }],
        }),
        Effect::Redeploy(tag) => {
            if run.dry {
                say(format!("would redeploy {} ({}) to {}", tag.name(), tag.commit.short(), site.name));
                return Ok(vec![ShipEvent::RollbackDone]);
            }
            let work = ensure_work(run, &tag.commit)?;
            let (ok, text) = run_in(&site.deploy, &work, &target);
            if ok {
                say(format!("redeployed {} ({}) to {}", tag.name(), tag.commit.short(), site.name));
                Ok(vec![ShipEvent::RollbackDone])
            } else {
                Ok(vec![ShipEvent::RedeployFailed { why: tail(&text, 20) }])
            }
        }
        Effect::Report(_, text) => {
            say(text);
            Ok(vec![])
        }
    }
}

fn say(text: String) {
    println!("{text}");
}

/// The live URLs. Each must answer 200. In live mode each must also not contain any string the change removed; in dry-run
/// that check is only printed, because the live site still has those strings until a real deploy. None means they pass.
fn verify(site: &Site, dry: bool) -> Option<String> {
    for check in &site.verify {
        if dry {
            let out = match Command::new("curl").args(["-sS", "--max-time", "20", "-o", "/dev/null", "-w", "%{http_code}", &check.url]).output() {
                Ok(out) => out,
                Err(e) => return Some(format!("cannot run curl for {}: {e}", check.url)),
            };
            if !out.status.success() {
                return Some(format!("{} could not be reached: {}", check.url, String::from_utf8_lossy(&out.stderr).trim()));
            }
            let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if code != "200" {
                return Some(format!("{} answered {}, not 200", check.url, code));
            }
            for s in &check.must_not_contain {
                say(format!("would check the live page does not contain \"{s}\""));
            }
            continue;
        }
        let out = match Command::new("curl").args(["-sS", "--max-time", "20", "-o", "-", "-w", "\n%{http_code}", &check.url]).output() {
            Ok(out) => out,
            Err(e) => return Some(format!("cannot run curl for {}: {e}", check.url)),
        };
        if !out.status.success() {
            return Some(format!("{} could not be reached: {}", check.url, String::from_utf8_lossy(&out.stderr).trim()));
        }
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
        if code.trim() != "200" {
            return Some(format!("{} answered {}, not 200", check.url, code.trim()));
        }
        if let Some(found) = check.must_not_contain.iter().find(|s| body.contains(s.as_str())) {
            return Some(format!("{} still contains \"{}\"", check.url, found));
        }
    }
    None
}

/// The first state for a site with no state file: live is the newest live/* tag, or origin's commit when there is none.
pub fn initial_state(site: &Site, head: &Commit) -> Result<ShipState, String> {
    let names = git(&site.repo, &["tag", "--list", "live/*"])?;
    let mut tags: Vec<LiveTag> = Vec::new();
    for name in names.lines().filter(|l| !l.is_empty()) {
        let Some(n) = name.strip_prefix("live/").and_then(|x| x.parse::<u32>().ok()) else { continue };
        let commit = Commit::parse(&git(&site.repo, &["rev-parse", &format!("{name}^{{commit}}")])?)?;
        tags.push(LiveTag { n, commit });
    }
    tags.sort_by_key(|t| t.n);
    Ok(match tags.len() {
        0 => {
            let live = LiveTag { n: 0, commit: head.clone() };
            ShipState::Idle { live: live.clone(), previous: live }
        }
        1 => {
            let live = tags.remove(0);
            ShipState::Idle { live: live.clone(), previous: live }
        }
        _ => {
            let live = tags.pop().expect("two or more tags");
            let previous = tags.pop().expect("two or more tags");
            ShipState::Idle { live, previous }
        }
    })
}

/// One pass for one site. A state killed mid-effect is resumed first, so its effect runs again; then origin's branch head
/// is sent. The dry-run counter is resolved from the history at the end.
pub fn watch_site(run: &Run) -> Result<ShipState, String> {
    let site = run.site;
    git(&site.repo, &["fetch", "origin"])?;
    let head = Commit::parse(&git(&site.repo, &["rev-parse", &format!("origin/{}", site.branch)])?)?;
    let state = match store::load_state(run.paths, &site.name)? {
        Some(state) => state,
        None => {
            let state = initial_state(site, &head)?;
            store::save_state(run.paths, &site.name, &state)?;
            state
        }
    };
    let result = (|| {
        let state = if state.is_mid_flight() { settle(run, state, vec![ShipEvent::Resumed])? } else { state };
        settle(run, state, vec![ShipEvent::OriginMoved { commit: head }])
    })();
    clean_work(run);
    let state = result?;
    resolve(run)?;
    Ok(state)
}

/// Sends a rollback request through the machine, the same way a watch pass does.
pub fn rollback_site(run: &Run) -> Result<ShipState, String> {
    let state = store::load_state(run.paths, &run.site.name)?
        .ok_or_else(|| format!("{} has no state yet. Run `shipd watch --once` first.", run.site.name))?;
    let result = settle(run, state, vec![ShipEvent::RollbackRequested]);
    clean_work(run);
    result
}

/// Records a commit a person deployed by hand, so the dry-run counter can compare it with what shipd would have done.
pub fn mark_live(paths: &Paths, site: &Site, text: &str) -> Result<String, String> {
    let commit = Commit::parse(&git(&site.repo, &["rev-parse", "--verify", &format!("{text}^{{commit}}")])?)?;
    store::append(paths, &site.name, "human_deployed", json!({ "commit": commit.as_str() }))?;
    resolve_with(paths, site)?;
    Ok(format!("recorded: {} was deployed by hand on {}", commit.short(), site.name))
}

fn resolve(run: &Run) -> Result<(), String> {
    resolve_with(run.paths, run.site)
}

/// Works out which dry-run pushes were right: a would-have-deployed commit that a person later deployed, or that carries a
/// live/* tag, is agreed; a different commit deployed by hand is disagreed. Each outcome is written once.
pub fn resolve_with(paths: &Paths, site: &Site) -> Result<(), String> {
    let name = &site.name;
    let history = store::read_history(paths, name);
    let mut pending: Option<String> = None;
    for (i, rec) in history.iter().enumerate() {
        match rec["kind"].as_str() {
            Some("would_have_deployed") => pending = rec["commit"].as_str().map(String::from),
            Some("human_deployed") => {
                // a human_deployed that already has its agreed or disagreed record after it is written up: only the pending push goes
                let written = history[i + 1..].iter().any(|r| matches!(r["kind"].as_str(), Some("agreed") | Some("disagreed")) && r["commit"] == rec["commit"]);
                if let (Some(p), Some(commit), false) = (pending.take(), rec["commit"].as_str(), written) {
                    let kind = if p == commit { "agreed" } else { "disagreed" };
                    store::append(paths, name, kind, json!({ "commit": p }))?;
                }
            }
            Some("agreed") | Some("disagreed") => pending = None,
            _ => {}
        }
    }
    if let Some(p) = pending {
        let tags = git(&site.repo, &["tag", "--points-at", &p]).unwrap_or_default();
        if tags.lines().any(|t| t.starts_with("live/")) {
            store::append(paths, name, "agreed", json!({ "commit": p }))?;
        }
    }
    Ok(())
}

/// How many dry-run pushes in a row would have been right: the run of agreed records since the last disagreed one.
pub fn streak(paths: &Paths, site: &str) -> usize {
    store::read_history(paths, site).iter().fold(0, |n, rec| match rec["kind"].as_str() {
        Some("agreed") => n + 1,
        Some("disagreed") => 0,
        _ => n,
    })
}

/// The last gate failure in the history, as its gate and report.
pub fn last_gate_failure(paths: &Paths, site: &str) -> Option<(String, String)> {
    store::read_history(paths, site).iter().rev().find(|r| r["kind"] == "gate_failed").map(|r| {
        (r["gate"].as_str().unwrap_or("").to_string(), r["report"].as_str().unwrap_or("").to_string())
    })
}

pub fn words(state: &ShipState) -> String {
    describe(state)
}
