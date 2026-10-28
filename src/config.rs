//! The edge for shipd.toml: text becomes sites, or an error that names the line's problem. Nothing past this file reads TOML.
use crate::types::*;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Raw {
    site: Vec<RawSite>,
}

#[derive(Deserialize)]
struct RawSite {
    name: String,
    repo: String,
    branch: String,
    /// "none" for a repo with no remote: the local branch is the truth. Leave out for "origin".
    remote: Option<String>,
    mode: String,
    gates: Vec<RawGate>,
    deploy: Vec<String>,
    /// The line of the deploy output that starts with this holds the platform's version id. Leave out if it reports none.
    version_line: Option<String>,
    diff: Option<RawDiff>,
    verify: Vec<RawCheck>,
}

#[derive(Deserialize)]
struct RawDiff {
    /// "show" or "hold".
    mode: String,
    /// Builds the site into $SHIPD_OUT.
    build: Vec<String>,
}

#[derive(Deserialize)]
struct RawGate {
    name: String,
    command: Vec<String>,
}

#[derive(Deserialize)]
struct RawCheck {
    url: String,
    must_not_contain: Vec<String>,
}

pub fn read(path: &Path) -> Result<Vec<Site>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn parse(text: &str) -> Result<Vec<Site>, String> {
    let raw: Raw = toml::from_str(text).map_err(|e| format!("not valid shipd.toml: {}", e.message()))?;
    let mut seen: Vec<&str> = Vec::new();
    let mut sites = Vec::new();
    for s in &raw.site {
        if seen.contains(&s.name.as_str()) {
            return Err(format!("two sites are called \"{}\". Give one of them another name.", s.name));
        }
        seen.push(&s.name);
        sites.push(site(s)?);
    }
    if sites.is_empty() {
        return Err("no [[site]] entries. Add one site.".into());
    }
    Ok(sites)
}

fn site(s: &RawSite) -> Result<Site, String> {
    let mode = match s.mode.as_str() {
        "dry-run" => Mode::DryRun,
        "live" => Mode::Live,
        other => return Err(format!("site \"{}\": mode is \"{other}\", but it must be \"dry-run\" or \"live\"", s.name)),
    };
    if s.deploy.is_empty() {
        return Err(format!("site \"{}\": deploy is an empty command. Give the command as a list of words.", s.name));
    }
    let gates = s
        .gates
        .iter()
        .map(|g| {
            let name = GateName::parse(&g.name).map_err(|e| format!("site \"{}\": {e}", s.name))?;
            if g.command.is_empty() {
                return Err(format!("site \"{}\": gate \"{}\" has no command", s.name, g.name));
            }
            Ok(Gate { name, command: g.command.clone() })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let diff = match &s.diff {
        None => DiffPolicy::Off,
        Some(d) if d.build.is_empty() => return Err(format!("site \"{}\": [site.diff] has an empty build command. It must write the built site to $SHIPD_OUT.", s.name)),
        Some(d) => match d.mode.as_str() {
            "show" => DiffPolicy::Show { build: d.build.clone() },
            "hold" => DiffPolicy::Hold { build: d.build.clone() },
            other => return Err(format!("site \"{}\": [site.diff] mode is \"{other}\", but it must be \"show\" or \"hold\"", s.name)),
        },
    };
    let version = match &s.version_line {
        None => VersionSource::NotReported,
        Some(p) if p.trim().is_empty() => return Err(format!("site \"{}\": version_line is empty; leave it out if the deploy reports no version", s.name)),
        Some(p) => VersionSource::DeployLine { prefix: p.clone() },
    };
    Ok(Site {
        name: s.name.clone(),
        repo: expand_home(&s.repo),
        branch: s.branch.clone(),
        source: match s.remote.as_deref() {
            None => BranchSource::Remote { name: "origin".into() },
            Some("none") => BranchSource::Local,
            Some(name) => BranchSource::Remote { name: name.into() },
        },
        mode,
        gates,
        deploy: s.deploy.clone(),
        version,
        diff,
        verify: s.verify.iter().map(|v| Check { url: v.url.clone(), must_not_contain: v.must_not_contain.clone() }).collect(),
    })
}

/// `~/...` becomes the home directory. Anything else is taken as written.
pub fn expand_home(text: &str) -> PathBuf {
    match text.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(text),
    }
}
