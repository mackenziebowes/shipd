//! The difference between two built folders, by path and content. Pure over two `Tree`s; reading a folder is the one edge.
//! What a site's change *means* (a phone removed, a kind relabelled) belongs to the site's own audit. This says which files moved,
//! which of them change how the site is served, and which pages vanished without a redirect.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// A file's place in the folder, always with `/` and never leading, as a static host would serve it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelPath(String);

impl RelPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a file is for, decided by its path alone. `Routing` and `Code` change how the site is served or what it runs, so a change
/// in either needs a person's explicit word at approval (see `DiffReport::flagged`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Page,
    Stylesheet,
    Script,
    Data,
    Image,
    /// Redirects, headers, routes, a worker, and the files that tell crawlers what to do (robots, sitemaps, llms.txt).
    Routing,
    /// Files that run: server code, WebAssembly, and SVG (which can carry script).
    Code,
    /// The site's own deploy files (its deploy script, its wrangler config): not in the build output, but they change what production does.
    Deploy,
    Other,
}

impl PathKind {
    pub fn of(path: &RelPath) -> PathKind {
        let p = path.as_str();
        let name = p.rsplit('/').next().unwrap_or(p);
        let lower = name.to_ascii_lowercase();
        if matches!(lower.as_str(), "_redirects" | "_headers" | "_routes.json" | "_worker.js" | "robots.txt" | "llms.txt" | "llms-full.txt" | ".htaccess")
            || (lower.starts_with("sitemap") && lower.ends_with(".xml"))
        {
            return PathKind::Routing;
        }
        match lower.rsplit_once('.').map(|(_, e)| e) {
            Some("html" | "htm") => PathKind::Page,
            Some("css") => PathKind::Stylesheet,
            Some("js" | "mjs") => PathKind::Script,
            Some("php" | "wasm" | "svg") => PathKind::Code,
            Some("json" | "jsonl" | "xml" | "txt" | "csv" | "tsv" | "webmanifest") => PathKind::Data,
            Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "ico" | "woff" | "woff2") => PathKind::Image,
            _ => PathKind::Other,
        }
    }
    pub fn plural(self) -> &'static str {
        match self {
            PathKind::Page => "pages",
            PathKind::Stylesheet => "stylesheets",
            PathKind::Script => "scripts",
            PathKind::Data => "data files",
            PathKind::Image => "images and fonts",
            PathKind::Routing => "routing and crawler files",
            PathKind::Code => "files that run (php, wasm, svg)",
            PathKind::Deploy => "deploy files (not in the build output)",
            PathKind::Other => "other files",
        }
    }
    /// Kinds whose change a person must name when approving.
    pub fn needs_word(self) -> bool {
        matches!(self, PathKind::Routing | PathKind::Code | PathKind::Deploy)
    }
    pub fn parse(text: &str) -> Result<PathKind, String> {
        match text {
            "routing" => Ok(PathKind::Routing),
            "code" => Ok(PathKind::Code),
            "deploy" => Ok(PathKind::Deploy),
            other => Err(format!("{other:?} is not a kind that needs a word; the kinds are routing, code and deploy")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            PathKind::Routing => "routing",
            PathKind::Code => "code",
            PathKind::Deploy => "deploy",
            PathKind::Page => "page",
            PathKind::Stylesheet => "stylesheet",
            PathKind::Script => "script",
            PathKind::Data => "data",
            PathKind::Image => "image",
            PathKind::Other => "other",
        }
    }
}

/// A file's identity: the SHA-256 of its bytes, as lowercase hex.
type Fingerprint = String;

/// A built folder: every file's path and fingerprint, and the text of its `_redirects` (empty when it has none).
#[derive(Clone, Debug, Default)]
pub struct Tree {
    files: BTreeMap<RelPath, Fingerprint>,
    redirects: String,
}

impl Tree {
    /// The SHA-256 of one file's bytes, by its place in the folder (`index.html`, `a/b/index.html`).
    pub fn file_hash(&self, path: &str) -> Option<&str> {
        self.files.get(&RelPath(path.to_string())).map(String::as_str)
    }

    /// SHA-256 over every path and fingerprint: names this exact folder. The deploy is held to it.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        for (path, fp) in &self.files {
            h.update(format!("{}\t{fp}\n", path.as_str()));
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Reads a folder. A symlink is an error that names it (the diff cannot see through one, so it will not skip one), and so is an empty folder.
    pub fn read(dir: &Path) -> Result<Tree, String> {
        let mut files = BTreeMap::new();
        walk(dir, dir, &mut files)?;
        if files.is_empty() {
            return Err(format!("{} has no files; a diff against an empty folder would call everything new", dir.display()));
        }
        let redirects = std::fs::read_to_string(dir.join("_redirects")).unwrap_or_default();
        Ok(Tree { files, redirects })
    }
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<RelPath, Fingerprint>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if kind.is_symlink() {
            return Err(format!("{} is a symlink; the diff cannot tell what it points at. Build real files, or copy what it points at.", path.display()));
        } else if kind.is_dir() {
            walk(root, &path, out)?;
        } else if kind.is_file() {
            let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let hash: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
            let rel = path.strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
            out.insert(RelPath(rel), hash);
        }
    }
    Ok(())
}

/// How one path differs between the two folders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Added,
    Removed,
    Changed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
    pub path: RelPath,
    pub kind: PathKind,
    pub change: Change,
    /// The two fingerprints, when the path exists on that side. They bind an approval to exactly this diff.
    pub fingerprints: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffSummary {
    /// The digest of the candidate's whole folder, not only of what changed.
    pub tree_digest: String,
    pub unchanged: usize,
    pub by_kind: Vec<(PathKind, Counts)>,
    pub changed: Vec<ChangedPath>,
    /// Removed pages that the candidate's `_redirects` does not send anywhere.
    pub gone_without_redirect: Vec<RelPath>,
    /// Files of the site's own deploy set (named by pathspec in the config) that differ in git between the live commit and the candidate.
    pub deploy_changed: Vec<String>,
    /// SHA-256 of the git diff of those files, so an approval is bound to their exact change.
    pub deploy_digest: String,
}

/// `live` is the folder now serving; `candidate` is the one about to.
pub fn diff_trees(live: &Tree, candidate: &Tree) -> DiffSummary {
    let mut changed = Vec::new();
    let mut unchanged = 0;
    for (path, fp) in &candidate.files {
        match live.files.get(path) {
            None => changed.push(ChangedPath { path: path.clone(), kind: PathKind::of(path), change: Change::Added, fingerprints: format!("-:{fp}") }),
            Some(old) if old != fp => changed.push(ChangedPath { path: path.clone(), kind: PathKind::of(path), change: Change::Changed, fingerprints: format!("{old}:{fp}") }),
            Some(_) => unchanged += 1,
        }
    }
    for (path, old) in live.files.iter().filter(|(p, _)| !candidate.files.contains_key(*p)) {
        changed.push(ChangedPath { path: path.clone(), kind: PathKind::of(path), change: Change::Removed, fingerprints: format!("{old}:-") });
    }
    let mut counts: BTreeMap<PathKind, Counts> = BTreeMap::new();
    for c in &changed {
        let n = counts.entry(c.kind).or_default();
        match c.change {
            Change::Added => n.added += 1,
            Change::Removed => n.removed += 1,
            Change::Changed => n.changed += 1,
        }
    }
    let rules = redirect_rules(&candidate.redirects);
    let gone_without_redirect = changed
        .iter()
        .filter(|c| c.change == Change::Removed && c.kind == PathKind::Page && !covered(&rules, &c.path, candidate))
        .map(|c| c.path.clone())
        .collect();
    DiffSummary { tree_digest: candidate.digest(), unchanged, by_kind: counts.into_iter().collect(), changed, gone_without_redirect, deploy_changed: vec![], deploy_digest: String::new() }
}

/// One line of a `_redirects` file: where from, where to, and the status (301 when the line gives none).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    from: String,
    to: String,
    status: u16,
}

/// The rules in a `_redirects` file that really redirect: a 3xx status, and a target that is not the address it redirects from.
/// A 200 rule is a rewrite (it serves a page at the old address and hides that the page is gone), and 404 or 410 says it is gone.
fn redirect_rules(text: &str) -> Vec<Rule> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let (from, to) = (words.next()?, words.next()?);
            let status = words.next().and_then(|w| w.trim_end_matches('!').parse::<u16>().ok()).unwrap_or(301);
            ((300..400).contains(&status) && from != to).then(|| Rule { from: from.into(), to: to.into(), status })
        })
        .collect()
}

/// Whether an address matches a rule's "from": `*` at the end takes the rest, `:name` takes one segment, anything else must be equal.
fn matches_from(pattern: &str, address: &str) -> bool {
    let (pat, addr): (Vec<&str>, Vec<&str>) = (pattern.trim_matches('/').split('/').collect(), address.trim_matches('/').split('/').collect());
    for (i, seg) in pat.iter().enumerate() {
        if *seg == "*" && i == pat.len() - 1 {
            return addr.len() >= i;
        }
        match addr.get(i) {
            None => return false,
            Some(a) if seg.starts_with(':') && !a.is_empty() => {}
            Some(a) if a == seg => {}
            Some(_) => return false,
        }
    }
    pat.len() == addr.len()
}

/// Whether the candidate has something at the rule's target: an absolute URL, a placeholder or splat target (not checkable), or a page or file.
fn target_exists(target: &str, candidate: &Tree) -> bool {
    if target.starts_with("http://") || target.starts_with("https://") || target.contains(':') || target.contains('*') {
        return true;
    }
    let path = target.split(['?', '#']).next().unwrap_or(target).trim_start_matches('/');
    let dir = path.trim_end_matches('/');
    let has = |p: String| candidate.files.contains_key(&RelPath(p));
    dir.is_empty() && has("index.html".into()) || has(dir.to_string()) || has(format!("{dir}/index.html")) || has(format!("{dir}.html"))
}

/// Whether some redirect answers for the address a removed page used to have, and sends it somewhere that exists. `a/b/index.html` was served at
/// `/a/b/`, `/a/b` and `/a/b/index.html`.
fn covered(rules: &[Rule], page: &RelPath, candidate: &Tree) -> bool {
    let p = page.as_str();
    let dir = p.strip_suffix("index.html").unwrap_or(p).trim_end_matches('/');
    let addresses = [format!("/{dir}/"), format!("/{dir}"), format!("/{p}")];
    rules.iter().any(|r| r.status >= 300 && addresses.iter().any(|a| matches_from(&r.from, a)) && target_exists(&r.to, candidate))
}

/// What the release step and the person approving both need: words, a digest that names this exact diff, and what must be said out loud.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffReport {
    pub text: String,
    /// The digest of the candidate folder that was built and shown. What deploys must be this folder, byte for byte.
    pub tree_digest: String,
    /// SHA-256 over every changed path with both fingerprints. The approval carries it, so a different diff cannot be approved by mistake.
    pub digest: String,
    /// Kinds with a change that need a word at approval (routing, code).
    pub flagged: Vec<PathKind>,
    /// Removed pages with no redirect. Any at all refuses the release.
    pub gone_without_redirect: Vec<RelPath>,
}

impl DiffSummary {
    pub fn report(&self, max_paths: usize) -> DiffReport {
        let mut h = Sha256::new();
        for c in &self.changed {
            h.update(format!("{}\t{}\n", c.path.as_str(), c.fingerprints));
        }
        h.update(format!("deploy\t{}\n", self.deploy_digest));
        let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let mut flagged: Vec<PathKind> = self.by_kind.iter().filter(|(k, _)| k.needs_word()).map(|(k, _)| *k).collect();
        if !self.deploy_changed.is_empty() {
            flagged.push(PathKind::Deploy);
        }
        DiffReport { text: self.render(max_paths), tree_digest: self.tree_digest.clone(), digest, flagged, gone_without_redirect: self.gone_without_redirect.clone() }
    }

    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.deploy_changed.is_empty()
    }

    /// Adds the files of the deploy set that differ in git, found by the caller (git is an edge).
    pub fn with_deploy(mut self, changed: Vec<String>, digest: String) -> DiffSummary {
        self.deploy_changed = changed;
        self.deploy_digest = digest;
        self
    }

    /// The summary in words: a line per kind, then up to `max_paths` changed paths, the ones that change serving first and pages last.
    pub fn render(&self, max_paths: usize) -> String {
        if self.is_empty() {
            return format!("nothing changed: all {} files are identical to live\n", self.unchanged);
        }
        let mut out = String::new();
        if !self.deploy_changed.is_empty() {
            out.push_str(&format!("DEPLOY FILES CHANGED (not in the build output; they change what production does): {}  <- needs a word at approval\n", self.deploy_changed.join(", ")));
        }
        out.push_str(&format!("{} files changed, {} unchanged (the build of the commit tagged live/N against the build of the candidate, not against the site being served)\n", self.changed.len(), self.unchanged));
        for (kind, n) in &self.by_kind {
            let flag = if kind.needs_word() { "  <- needs a word at approval" } else { "" };
            out.push_str(&format!("  {}: {} added, {} removed, {} changed{flag}\n", kind.plural(), n.added, n.removed, n.changed));
        }
        if !self.gone_without_redirect.is_empty() {
            out.push_str(&format!("  {} removed pages have no redirect to a page that exists:\n", self.gone_without_redirect.len()));
            for p in self.gone_without_redirect.iter().take(10) {
                out.push_str(&format!("    {}\n", p.as_str()));
            }
        }
        let mut shown: Vec<&ChangedPath> = self.changed.iter().collect();
        shown.sort_by_key(|c| (matches!(c.kind, PathKind::Page), c.kind, c.path.clone()));
        for c in shown.iter().take(max_paths) {
            let mark = match c.change {
                Change::Added => '+',
                Change::Removed => '-',
                Change::Changed => '~',
            };
            out.push_str(&format!("  {mark} {}\n", c.path.as_str()));
        }
        if shown.len() > max_paths {
            out.push_str(&format!("  ... and {} more\n", shown.len() - max_paths));
        }
        out
    }
}
