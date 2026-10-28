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
            PathKind::Other => "other files",
        }
    }
    /// Kinds whose change a person must name when approving.
    pub fn needs_word(self) -> bool {
        matches!(self, PathKind::Routing | PathKind::Code)
    }
    pub fn parse(text: &str) -> Result<PathKind, String> {
        match text {
            "routing" => Ok(PathKind::Routing),
            "code" => Ok(PathKind::Code),
            other => Err(format!("{other:?} is not a kind that needs a word; the kinds are routing and code")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            PathKind::Routing => "routing",
            PathKind::Code => "code",
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
    pub unchanged: usize,
    pub by_kind: Vec<(PathKind, Counts)>,
    pub changed: Vec<ChangedPath>,
    /// Removed pages that the candidate's `_redirects` does not send anywhere.
    pub gone_without_redirect: Vec<RelPath>,
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
    let sources = redirect_sources(&candidate.redirects);
    let gone_without_redirect = changed
        .iter()
        .filter(|c| c.change == Change::Removed && c.kind == PathKind::Page && !covered(&sources, &c.path))
        .map(|c| c.path.clone())
        .collect();
    DiffSummary { unchanged, by_kind: counts.into_iter().collect(), changed, gone_without_redirect }
}

/// The "from" side of each rule in a `_redirects` file: the first word of each line that is not blank or a comment.
fn redirect_sources(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).filter_map(|l| l.split_whitespace().next()).map(String::from).collect()
}

/// Whether some redirect rule answers for the address a removed page used to have. `a/b/index.html` was served at `/a/b/`, `/a/b` and
/// `/a/b/index.html`; a rule ending in `*` covers every address that starts with what comes before it.
fn covered(sources: &[String], page: &RelPath) -> bool {
    let p = page.as_str();
    let dir = p.strip_suffix("index.html").unwrap_or(p).trim_end_matches('/');
    let addresses = [format!("/{dir}/"), format!("/{dir}"), format!("/{p}")];
    sources.iter().any(|s| match s.strip_suffix('*') {
        Some(prefix) => addresses.iter().any(|a| a.starts_with(prefix)),
        None => addresses.iter().any(|a| a == s),
    })
}

/// What the release step and the person approving both need: words, a digest that names this exact diff, and what must be said out loud.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffReport {
    pub text: String,
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
        let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let flagged = self.by_kind.iter().filter(|(k, _)| k.needs_word()).map(|(k, _)| *k).collect();
        DiffReport { text: self.render(max_paths), digest, flagged, gone_without_redirect: self.gone_without_redirect.clone() }
    }

    pub fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }

    /// The summary in words: a line per kind, then up to `max_paths` changed paths, the ones that change serving first and pages last.
    pub fn render(&self, max_paths: usize) -> String {
        if self.is_empty() {
            return format!("nothing changed: all {} files are identical to live\n", self.unchanged);
        }
        let mut out = format!("{} files changed, {} unchanged\n", self.changed.len(), self.unchanged);
        for (kind, n) in &self.by_kind {
            let flag = if kind.needs_word() { "  <- needs a word at approval" } else { "" };
            out.push_str(&format!("  {}: {} added, {} removed, {} changed{flag}\n", kind.plural(), n.added, n.removed, n.changed));
        }
        if !self.gone_without_redirect.is_empty() {
            out.push_str(&format!("  {} removed pages have no redirect:\n", self.gone_without_redirect.len()));
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
