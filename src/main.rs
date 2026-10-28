//! shipd: a local watcher that deploys a site's main when every gate passes, and rolls back when the live check fails.
//! This file is the adapter for the command line: it turns each command into one event or one query, and decides nothing.
mod config;
mod diff;
mod driver;
mod step;
mod store;
mod types;
mod words;

use driver::Run;
use std::path::PathBuf;
use std::time::Duration;
use store::Paths;
use types::{ShipState, Site};

const USAGE: &str = "shipd: deploys a site's main when every gate passes.

Usage: shipd [--config FILE] <command> [args]

Commands:
  status [SITE]               the state of each site, in words, and the dry-run count
  watch [--once] [--dry-run]  check origin for each site; every 60 seconds unless --once
  rollback SITE [--dry-run]   redeploy the previous live/<n> tag
  mark-live SITE COMMIT       record a commit that a person deployed by hand
  release SITE [--commit REV] [--dry-run]
                              run one commit (default: the branch head) through the gates, the diff and the deploy
  approve SITE DIGEST [--allow routing,code]
                              approve the diff that is waiting, by its digest
  accept-live SITE            after a release was blocked because the served site changed: record the site as it is now
  diff LIVE_DIR NEXT_DIR      say which files differ between two built folders
  digest DIR                  print the SHA-256 that names a built folder (what an approval and a deploy are held to)

The config is shipd.toml: $SHIPD_CONFIG, or ~/.config/shipd/shipd.toml, unless --config says otherwise.
State and history are kept under $SHIPD_HOME, or ~/.local/share/shipd.
Run `shipd <command> --help` for one command.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("shipd: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn run(args: &[String]) -> Result<(), String> {
    let (config_path, rest) = take_flag(args, "--config")?;
    let config_path = config_path.map(PathBuf::from).unwrap_or_else(default_config);
    let Some((command, rest)) = rest.split_first() else {
        println!("{USAGE}");
        return Ok(());
    };
    if rest.iter().any(|a| a == "--help" || a == "-h") || command == "--help" || command == "-h" || command == "help" {
        println!("{}", help(command));
        return Ok(());
    }
    let paths = Paths::from_env();
    match command.as_str() {
        "status" => status(&config_path, &paths, rest),
        "watch" => watch(&config_path, &paths, rest),
        "rollback" => rollback(&config_path, &paths, rest),
        "mark-live" => mark_live(&config_path, &paths, rest),
        "release" => release(&config_path, &paths, rest),
        "approve" => approve(&config_path, &paths, rest),
        "accept-live" => {
            let pos = positional(rest);
            let [name] = pos.as_slice() else { return Err("usage: shipd accept-live SITE".into()) };
            let site = sites_named(&config_path, Some(name.as_str()))?.remove(0);
            let state = driver::accept_live(&Run { site: &site, paths: &paths, dry: false })?;
            println!("{}: {}", site.name, driver::words(&state));
            Ok(())
        }
        "diff" => diff_command(rest),
        "digest" => {
            let pos = positional(rest);
            let [dir] = pos.as_slice() else { return Err("usage: shipd digest DIR".into()) };
            println!("{}", diff::Tree::read(std::path::Path::new(dir.as_str()))?.digest());
            Ok(())
        }
        other => Err(format!("no command called \"{other}\". Try `shipd --help`.")),
    }
}

fn default_config() -> PathBuf {
    match std::env::var("SHIPD_CONFIG") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => config::expand_home("~/.config/shipd/shipd.toml"),
    }
}

/// Removes `--name VALUE` from the arguments, if it is there.
fn take_flag(args: &[String], name: &str) -> Result<(Option<String>, Vec<String>), String> {
    let mut value = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            value = Some(it.next().ok_or_else(|| format!("{name} needs a value"))?.clone());
        } else {
            rest.push(a.clone());
        }
    }
    Ok((value, rest))
}

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn positional(args: &[String]) -> Vec<&String> {
    args.iter().filter(|a| !a.starts_with("--")).collect()
}

fn sites_named(config_path: &PathBuf, name: Option<&str>) -> Result<Vec<Site>, String> {
    let all = config::read(config_path)?;
    match name {
        None => Ok(all),
        Some(n) => {
            let found: Vec<Site> = all.into_iter().filter(|s| s.name == n).collect();
            if found.is_empty() {
                Err(format!("no site called \"{n}\" in {}", config_path.display()))
            } else {
                Ok(found)
            }
        }
    }
}

fn status(config_path: &PathBuf, paths: &Paths, args: &[String]) -> Result<(), String> {
    for site in sites_named(config_path, positional(args).first().map(|s| s.as_str()))? {
        println!("{}", site.name);
        match store::load_state(paths, &site.name)? {
            Some(state) => println!("  state: {}", driver::words(&state)),
            None => println!("  state: nothing yet. Run `shipd watch --once` to start it."),
        }
        // a gate failure is news only while its candidate is still blocked: once the state moves on, it is history
        if let Some(ShipState::Blocked { .. }) = store::load_state(paths, &site.name)? {
            if let Some((gate, report)) = driver::last_gate_failure(paths, &site.name) {
                println!("  last gate failure: \"{gate}\": {}", report.lines().last().unwrap_or(""));
            }
        }
        match site.mode {
            types::Mode::DryRun => println!("  mode: dry-run (config says so; shipd never flips itself)"),
            types::Mode::Live => println!("  mode: live (config says so; deploys are real)"),
        }
        driver::resolve_with(paths, &site)?;
        println!("  dry-run: {} pushes in a row would have been right", driver::streak(paths, &site.name));
    }
    Ok(())
}

fn watch(config_path: &PathBuf, paths: &Paths, args: &[String]) -> Result<(), String> {
    let once = has(args, "--once");
    let sites = config::read(config_path)?;
    loop {
        let mut loud: Vec<String> = Vec::new();
        for site in &sites {
            let dry = driver::is_dry(site, has(args, "--dry-run"));
            println!("{}: {}", site.name, if dry { "dry-run pass" } else { "live pass" });
            let run = Run { site, paths, dry };
            match driver::watch_site(&run) {
                Ok(state) => {
                    println!("  {}: {}", site.name, driver::words(&state));
                    if matches!(state, ShipState::RollbackFailed { .. }) {
                        loud.push(site.name.clone());
                    }
                }
                Err(e) => println!("  {}: this pass failed: {e}", site.name),
            }
        }
        if once {
            // a failed rollback never goes quiet: the pass that ends in one exits non-zero
            return if loud.is_empty() { Ok(()) } else { Err(format!("a rollback failed on {}; see the status line above", loud.join(", "))) };
        }
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn rollback(config_path: &PathBuf, paths: &Paths, args: &[String]) -> Result<(), String> {
    let name = positional(args).first().map(|s| s.to_string()).ok_or("rollback needs a site: shipd rollback SITE")?;
    let site = sites_named(config_path, Some(&name))?.remove(0);
    let dry = driver::is_dry(&site, has(args, "--dry-run"));
    let run = Run { site: &site, paths, dry };
    let state = driver::rollback_site(&run)?;
    println!("{}: {}", site.name, driver::words(&state));
    if matches!(state, ShipState::RollbackFailed { .. }) {
        return Err("the rollback failed, and the rejected commit may still be live".into());
    }
    Ok(())
}

fn mark_live(config_path: &PathBuf, paths: &Paths, args: &[String]) -> Result<(), String> {
    let pos = positional(args);
    let (Some(name), Some(commit)) = (pos.first(), pos.get(1)) else {
        return Err("mark-live needs a site and a commit: shipd mark-live SITE COMMIT".into());
    };
    let site = sites_named(config_path, Some(name))?.remove(0);
    println!("{}", driver::mark_live(paths, &site, commit)?);
    Ok(())
}

fn help(command: &str) -> String {
    match command {
        "status" => "shipd status [SITE]\n\nPrints each site's state in words, its last gate failure, and how many dry-run\npushes in a row would have been right.".into(),
        "watch" => "shipd watch [--once] [--dry-run]\n\nFor each site: git fetch origin, send origin's branch head to the state machine, and run what it\nasks for. Gates always run for real. With --dry-run, Deploy, Tag and Redeploy are only printed.\nWithout --once it repeats every 60 seconds.".into(),
        "rollback" => "shipd rollback SITE [--dry-run]\n\nSends a rollback request: the previous live/<n> tag is redeployed, and the failed commit is held\non main until main moves past it.".into(),
        "mark-live" => "shipd mark-live SITE COMMIT\n\nRecords that a person deployed COMMIT by hand. If shipd would have deployed the same commit in\ndry-run, that is counted as agreed; a different commit is disagreed.".into(),
        "release" => "shipd release SITE [--commit REV] [--dry-run]\n\nSends one commit (REV, which must be on the site's branch; or the branch head) through the same machine as `watch`: the gates in a clean\nworktree, then the diff against live if the site has a [site.diff] section, then the deploy, the live check and the live/<n> tag. With\nmode \"hold\" it stops after the diff and waits for `shipd approve`.".into(),
        "approve" => "shipd approve SITE DIGEST [--allow routing,code]\n\nApproves the diff the site is waiting on. DIGEST is the digest printed with the diff (12 characters are enough). If the diff changes\nrouting or code files, say so with --allow. A different commit, a different digest or a missing word is refused, and nothing deploys.".into(),
        "accept-live" => "shipd accept-live SITE\n\nFor a person who has looked. A release was blocked because the pages the site serves no longer match what they were when the live tag went\nlive (a hand deploy, or a page that is not static). This records the pages as they are now, drops the blocked release and returns the site to idle.".into(),
        "digest" => "shipd digest DIR\n\nPrints the SHA-256 over every path and file hash of a built folder. An approval and a deploy are held to it.".into(),
        "diff" => "shipd diff LIVE_DIR NEXT_DIR\n\nReads two built folders and says which files were added, removed or changed, by kind (pages, stylesheets,\nscripts, data, images, redirects), then lists the changed paths. Exit 0 whether or not they differ.".into(),
        _ => USAGE.into(),
    }
}

/// `shipd diff LIVE_DIR NEXT_DIR`: reads both folders and prints the difference. The same function the release step uses.
fn diff_command(args: &[String]) -> Result<(), String> {
    let pos = positional(args);
    let [live, next] = pos.as_slice() else {
        return Err("usage: shipd diff LIVE_DIR NEXT_DIR".into());
    };
    let summary = diff::diff_trees(&diff::Tree::read(std::path::Path::new(live.as_str()))?, &diff::Tree::read(std::path::Path::new(next.as_str()))?);
    let report = summary.report(40);
    print!("{}", report.text);
    println!("digest {}", report.digest);
    Ok(())
}

/// `shipd release SITE [--commit REV] [--dry-run]`: one commit through the machine.
fn release(config_path: &PathBuf, paths: &Paths, args: &[String]) -> Result<(), String> {
    let (rev, rest) = take_flag(args, "--commit")?;
    let pos = positional(&rest);
    let [name] = pos.as_slice() else {
        return Err("usage: shipd release SITE [--commit REV] [--dry-run]".into());
    };
    let site = sites_named(config_path, Some(name.as_str()))?.remove(0);
    let dry = driver::is_dry(&site, has(&rest, "--dry-run"));
    println!("{}: {}", site.name, if dry { "dry-run release" } else { "live release" });
    let state = driver::release_site(&Run { site: &site, paths, dry }, rev.as_deref())?;
    println!("{}: {}", site.name, driver::words(&state));
    Ok(())
}

/// `shipd approve SITE DIGEST [--allow routing,code]`.
fn approve(config_path: &PathBuf, paths: &Paths, args: &[String]) -> Result<(), String> {
    let (allow_text, rest) = take_flag(args, "--allow")?;
    let pos = positional(&rest);
    let [name, digest] = pos.as_slice() else {
        return Err("usage: shipd approve SITE DIGEST [--allow routing,code]".into());
    };
    let allow = allow_text.as_deref().unwrap_or("").split(',').filter(|k| !k.is_empty()).map(diff::PathKind::parse).collect::<Result<Vec<_>, _>>()?;
    let site = sites_named(config_path, Some(name.as_str()))?.remove(0);
    let dry = driver::is_dry(&site, has(&rest, "--dry-run"));
    let state = driver::approve_site(&Run { site: &site, paths, dry }, digest, allow)?;
    println!("{}: {}", site.name, driver::words(&state));
    Ok(())
}
