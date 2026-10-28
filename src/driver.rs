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

/// Runs a command in `dir`, with SHIPD_TARGET set to the site's shared build directory, SHIPD_RELEASE set (a site's own deploy script
/// refuses to deploy to production without it) and any `extra` variables. Returns success and the output.
fn run_in(cmd: &[String], dir: &Path, target: &Path, extra: &[(&str, &str)], limit: std::time::Duration) -> (bool, String) {
    use std::io::Read;
    use std::process::Stdio;
    let Some((prog, rest)) = cmd.split_first() else {
        return (false, "the command is empty".into());
    };
    let mut command = Command::new(prog);
    // Its own process group, so a timeout stops the whole tree (sh, bun, node, wrangler) and not only its first process.
    use std::os::unix::process::CommandExt;
    command.args(rest).current_dir(dir).env("SHIPD_TARGET", target).env("SHIPD_RELEASE", "1").stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    for (name, value) in extra {
        command.env(name, value);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return (false, format!("cannot run {prog}: {e}")),
    };
    let readers: Vec<std::thread::JoinHandle<String>> = [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)]
        .into_iter()
        .flatten()
        .map(|mut stream| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = stream.read_to_end(&mut bytes);
                String::from_utf8_lossy(&bytes).to_string()
            })
        })
        .collect();
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() > limit => {
                let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).status();
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("{prog} ran over {} seconds and was stopped", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => break Err(format!("cannot wait for {prog}: {e}")),
        }
    };
    let text: String = readers.into_iter().map(|r| r.join().unwrap_or_default()).collect();
    match status {
        Ok(status) => (status.success(), text),
        Err(why) => (false, format!("{text}\n{why}")),
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
        // nothing the previous build left behind (untracked or ignored files) may reach this one
        git(&work, &["clean", "-fdxq"])?;
        install(run, &work)?;
    } else {
        std::fs::create_dir_all(run.paths.site_dir(&run.site.name)).map_err(|e| e.to_string())?;
        let work_text = work.to_string_lossy().to_string();
        git(&run.site.repo, &["worktree", "add", "--detach", &work_text, commit.as_str()])?;
        install(run, &work)?;
    }
    Ok(work)
}

/// Builds `commit` with the site's build command into `out` (emptied first), in the site's clean worktree.
fn build_into(run: &Run, build: &[String], commit: &Commit, out: &Path) -> Result<(), String> {
    let work = ensure_work(run, commit)?;
    let _ = std::fs::remove_dir_all(out);
    std::fs::create_dir_all(out).map_err(|e| format!("cannot make {}: {e}", out.display()))?;
    let (ok, text) = run_in(build, &work, &run.paths.target(&run.site.name), &[("SHIPD_OUT", &out.to_string_lossy())], run.site.command_limit);
    if ok {
        Ok(())
    } else {
        Err(format!("the build of {} failed: {}", commit.short(), tail(&text, 20)))
    }
}

/// Builds the live commit and the candidate into two folders beside the worktree (never in /tmp), compares them, and removes both.
fn make_diff(run: &Run, build: &[String], deploy_files: &[String], live: &Commit, candidate: &Commit) -> Result<crate::diff::DiffReport, String> {
    let dir = run.paths.site_dir(&run.site.name);
    let (live_out, next_out) = (dir.join("diff-live"), dir.join("diff-next"));
    let result = (|| {
        build_into(run, build, live, &live_out)?;
        build_into(run, build, candidate, &next_out)?;
        let summary = crate::diff::diff_trees(&crate::diff::Tree::read(&live_out)?, &crate::diff::Tree::read(&next_out)?);
        // The site's own deploy files are not in the build output: ask git what differs in them between the two commits.
        let summary = if deploy_files.is_empty() {
            summary
        } else {
            let mut args = vec!["diff", live.as_str(), candidate.as_str(), "--"];
            args.extend(deploy_files.iter().map(String::as_str));
            let patch = git(&run.site.repo, &args)?;
            let mut names = vec!["diff", "--name-only", live.as_str(), candidate.as_str(), "--"];
            names.extend(deploy_files.iter().map(String::as_str));
            let changed: Vec<String> = git(&run.site.repo, &names)?.lines().map(String::from).collect();
            let digest: String = sha2::Digest::finalize(sha2::Digest::chain_update(sha2::Sha256::default(), patch.as_bytes())).iter().map(|b| format!("{b:02x}")).collect();
            summary.with_deploy(changed, digest)
        };
        Ok(summary.report(40))
    })();
    // The live build is only for the comparison. The candidate's folder is kept: it is what the approval is about, and what the deploy ships.
    let _ = std::fs::remove_dir_all(&live_out);
    let _ = std::fs::remove_file(dir.join("diff-live.urls.txt"));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&next_out);
    }
    result
}

/// The platform's version id from the deploy output: what follows the site's prefix on the first line that starts with it.
fn version_in(source: &VersionSource, output: &str) -> VersionId {
    let VersionSource::DeployLine { prefix } = source else { return VersionId::NotReported };
    output
        .lines()
        .find_map(|l| l.trim().strip_prefix(prefix.as_str()))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| VersionId::Found(id.to_string()))
        .unwrap_or(VersionId::NotReported)
}

/// Commits, by path, the files a release produced in its checkout that belong on the branch (the site's `land` list), so the next agent's
/// `git add -A` does not sweep them into an unrelated commit and nobody has to remember to commit them. Only for a local branch that is checked out in
/// the repo; says so when it cannot. A failure here never undoes the deploy: it is said out loud.
fn land(run: &Run, tag: &LiveTag) -> Option<Commit> {
    let site = run.site;
    if site.land.is_empty() {
        return None;
    }
    let result = (|| -> Result<Option<Commit>, String> {
        if site.source != BranchSource::Local {
            return Err("this site's branch is on a remote; commit the files by hand".into());
        }
        if git(&site.repo, &["rev-parse", "--abbrev-ref", "HEAD"])? != site.branch {
            return Err(format!("the repo is not on {}; commit the files by hand", site.branch));
        }
        let work = run.paths.work(&site.name);
        let mut moved: Vec<&str> = Vec::new();
        for file in &site.land {
            let from = work.join(file);
            if !from.exists() {
                continue;
            }
            let to = site.repo.join(file);
            if std::fs::read(&from).ok() != std::fs::read(&to).ok() {
                std::fs::copy(&from, &to).map_err(|e| format!("cannot copy {file}: {e}"))?;
                moved.push(file);
            }
        }
        if moved.is_empty() {
            return Ok(None);
        }
        let mut add = vec!["add", "--"];
        add.extend(moved.iter().copied());
        git(&site.repo, &add)?;
        let message = format!("shipd: {} from release {} ({})", moved.join(", "), tag.name(), tag.commit.short());
        let mut commit = vec!["commit", "-q", "-m", message.as_str(), "--"];
        commit.extend(moved.iter().copied());
        git(&site.repo, &commit)?;
        say(format!("landed: committed {} on {}", moved.join(", "), site.branch));
        Ok(Some(Commit::parse(&git(&site.repo, &["rev-parse", "HEAD"])?)?))
    })();
    match result {
        Ok(landed) => landed,
        Err(why) => {
            say(format!("NOT LANDED: {why}. The files are in {}", run.paths.work(&site.name).display()));
            None
        }
    }
}

/// The site's install step, run in a fresh checkout. Nothing when the site has none.
fn install(run: &Run, work: &Path) -> Result<(), String> {
    if run.site.install.is_empty() {
        return Ok(());
    }
    let (ok, text) = run_in(&run.site.install, work, &run.paths.target(&run.site.name), &[], run.site.command_limit);
    if ok {
        Ok(())
    } else {
        Err(format!("the install step ({}) failed: {}", run.site.install.join(" "), tail(&text, 20)))
    }
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

/// Brings the site's branch up to date when it lives on a remote, and names the ref that is its head.
fn branch_head(site: &Site) -> Result<(String, Commit), String> {
    let reference = match &site.source {
        BranchSource::Remote { name } => {
            git(&site.repo, &["fetch", name])?;
            format!("{name}/{}", site.branch)
        }
        BranchSource::Local => site.branch.clone(),
    };
    let head = Commit::parse(&git(&site.repo, &["rev-parse", &reference])?)?;
    Ok((reference, head))
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
        let plan = Plan { gates: &run.site.gates, diff: &run.site.diff, highest };
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
        Effect::MakeDiff { live, candidate } => format!("build {} and {} and compare them", live.short(), candidate.short()),
        Effect::RunGate(g) => format!("run gate \"{}\"", g.name.as_str()),
        Effect::Deploy { commit, .. } => format!("deploy {}", commit.short()),
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
        Effect::MakeDiff { live, candidate } => {
            let (DiffPolicy::Show { build, deploy_files } | DiffPolicy::Hold { build, deploy_files }) = &site.diff else {
                return Ok(vec![ShipEvent::DiffFailed { why: "the site has no [site.diff] section but a diff was asked for".into() }]);
            };
            Ok(vec![match make_diff(run, build, deploy_files, &live, &candidate) {
                Ok(report) => ShipEvent::DiffMade { report },
                Err(why) => ShipEvent::DiffFailed { why },
            }])
        }
        Effect::RunGate(gate) => {
            let work = run.paths.work(&site.name);
            let (ok, text) = run_in(&gate.command, &work, &target, &[], site.command_limit);
            if ok {
                Ok(vec![ShipEvent::GateDone { gate: gate.name, outcome: GateOutcome::Passed }])
            } else {
                let report = tail(&text, 40);
                store::append(run.paths, &site.name, "gate_failed", json!({ "gate": gate.name.as_str(), "report": report }))?;
                Ok(vec![ShipEvent::GateDone { gate: gate.name, outcome: GateOutcome::Failed { report } }])
            }
        }
        Effect::Deploy { commit, built } => {
            if run.dry {
                say(format!("would deploy {} to {}", commit.short(), site.name));
                store::append(run.paths, &site.name, "would_have_deployed", json!({ "commit": commit.as_str() }))?;
                return Ok(vec![ShipEvent::Deployed { version: VersionId::NotReported }]);
            }
            let work = ensure_work(run, &commit)?;
            // With a diff step the approved folder was kept. It is hashed again now, and the deploy is handed it: what ships is what a person saw.
            let kept = run.paths.site_dir(&site.name).join("diff-next");
            let (ok, text) = match &built {
                Built::BySiteDeploy => run_in(&site.deploy, &work, &target, &[], site.command_limit),
                Built::KeptFolder { tree_digest } => match crate::diff::Tree::read(&kept) {
                    Ok(tree) if tree.digest() == *tree_digest => run_in(&site.deploy, &work, &target, &[("SHIPD_APPROVED_DIR", &kept.to_string_lossy()), ("SHIPD_APPROVED_DIGEST", tree_digest)], site.command_limit),
                    Ok(_) => (false, format!("the folder that was approved ({}) changed on disk after the diff was made; nothing was deployed. Run the release again.", kept.display())),
                    Err(e) => (false, format!("the folder that was approved is not there to deploy: {e}. Run the release again.")),
                },
            };
            // The deploy must say which folder it shipped. A script that ignored SHIPD_APPROVED_DIR and built for itself prints another digest, or none.
            let (ok, text) = match (&built, ok) {
                (Built::KeptFolder { tree_digest }, true) => match text.lines().find_map(|l| l.trim().strip_prefix("Uploaded digest:")).map(str::trim) {
                    Some(d) if d == tree_digest => (true, text),
                    Some(d) => (false, format!("the deploy ran, but it shipped a folder with digest {d}, not the approved {tree_digest}. Live may now hold a build nobody approved: check it.\n{text}")),
                    None => (false, format!("the deploy ran but did not print an `Uploaded digest:` line, so it cannot be shown to have shipped the approved folder. Live may hold a build nobody approved: check it.\n{text}")),
                },
                _ => (ok, text),
            };
            if matches!(built, Built::KeptFolder { .. }) {
                let _ = std::fs::remove_dir_all(&kept);
                let _ = std::fs::remove_file(run.paths.site_dir(&site.name).join("diff-next.urls.txt"));
            }
            if ok {
                say(format!("deployed {} to {}", commit.short(), site.name));
                Ok(vec![ShipEvent::Deployed { version: version_in(&site.version, &text) }])
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
            // The release's own output is committed first, and the tag names that commit: it is the branch head that is live.
            let landed = land(run, &tag);
            let at = landed.clone().unwrap_or_else(|| tag.commit.clone());
            git(&site.repo, &["tag", &tag.name(), at.as_str()])?;
            say(format!("tagged {} at {}", tag.name(), at.short()));
            Ok(landed.into_iter().map(|commit| ShipEvent::Landed { commit }).collect())
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
            let (ok, text) = run_in(&site.deploy, &work, &target, &[], site.command_limit);
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
    let (_, head) = branch_head(site)?;
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

/// One release of a named commit (or origin's branch head when none is named): the same machine as a watch pass. The commit must be on the
/// site's branch, because only what is on the branch has been reviewed and gated.
pub fn release_site(run: &Run, rev: Option<&str>) -> Result<ShipState, String> {
    let site = run.site;
    let (branch, head) = branch_head(site)?;
    let commit = match rev {
        None => head.clone(),
        Some(rev) => {
            let c = Commit::parse(&git(&site.repo, &["rev-parse", &format!("{rev}^{{commit}}")])?)?;
            git(&site.repo, &["merge-base", "--is-ancestor", c.as_str(), &branch])
                .map_err(|_| format!("{} is not on {branch}. A release comes from a commit that is on the branch; push it there first.", c.short()))?;
            c
        }
    };
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
        settle(run, state, vec![ShipEvent::OriginMoved { commit }])
    })();
    clean_work(run);
    let state = result?;
    resolve(run)?;
    Ok(state)
}

/// A person approves the diff that is waiting: this commit, this digest, and a word for each kind that needed one.
pub fn approve_site(run: &Run, digest: &str, allow: Vec<crate::diff::PathKind>) -> Result<ShipState, String> {
    let state = store::load_state(run.paths, &run.site.name)?
        .ok_or_else(|| format!("{} has no state yet. Run `shipd release {}` first.", run.site.name, run.site.name))?;
    let ShipState::Diffed { candidate, .. } = &state else {
        return Err(format!("nothing is waiting for approval on {}. It is: {}", run.site.name, words(&state)));
    };
    let commit = candidate.clone();
    let result = settle(run, state, vec![ShipEvent::Approved { commit, digest: digest.to_string(), allow }]);
    clean_work(run);
    result
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
