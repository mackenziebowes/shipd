//! shipd: a local watcher that deploys a site's main when every gate passes, and rolls back when the live check fails.
//! This file is the adapter for the command line: it turns each command into one event or one query, and decides nothing.
mod config;
mod driver;
mod step;
mod store;
mod types;
mod words;

use driver::Run;
use std::path::PathBuf;
use std::time::Duration;
use store::Paths;
use types::Site;

const USAGE: &str = "shipd: deploys a site's main when every gate passes.

Usage: shipd [--config FILE] <command> [args]

Commands:
  status [SITE]               the state of each site, in words, and the dry-run count
  watch [--once] [--dry-run]  check origin for each site; every 60 seconds unless --once
  rollback SITE [--dry-run]   redeploy the previous live/<n> tag
  mark-live SITE COMMIT       record a commit that a person deployed by hand

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
        match driver::last_gate_failure(paths, &site.name) {
            Some((gate, report)) => println!("  last gate failure: \"{gate}\": {}", report.lines().last().unwrap_or("")),
            None => println!("  last gate failure: none"),
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
        for site in &sites {
            let dry = driver::is_dry(site, has(args, "--dry-run"));
            println!("{}: {}", site.name, if dry { "dry-run pass" } else { "live pass" });
            let run = Run { site, paths, dry };
            match driver::watch_site(&run) {
                Ok(state) => println!("  {}: {}", site.name, driver::words(&state)),
                Err(e) => println!("  {}: this pass failed: {e}", site.name),
            }
        }
        if once {
            return Ok(());
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
        _ => USAGE.into(),
    }
}
