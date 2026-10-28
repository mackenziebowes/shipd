//! The difference between two built folders, by path and content. Pure over two `Tree`s; reading a folder is the one edge.
//! What a site's change *means* (a phone removed, a kind relabelled) belongs to the site's own audit. This says which files moved.
use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// A file's place in the folder, always with `/` and never leading, as a static host would serve it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RelPath(String);

impl RelPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a file is for, decided by its path alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathKind {
    Page,
    Stylesheet,
    Script,
    Data,
    Image,
    Redirects,
    Other,
}

impl PathKind {
    pub fn of(path: &RelPath) -> PathKind {
        let p = path.as_str();
        let name = p.rsplit('/').next().unwrap_or(p);
        if name == "_redirects" || name == "_headers" {
            return PathKind::Redirects;
        }
        match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
            Some("html" | "htm") => PathKind::Page,
            Some("css") => PathKind::Stylesheet,
            Some("js" | "mjs") => PathKind::Script,
            Some("json" | "jsonl" | "xml" | "txt" | "csv" | "tsv" | "webmanifest") => PathKind::Data,
            Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" | "ico" | "woff" | "woff2") => PathKind::Image,
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
            PathKind::Redirects => "redirect and header files",
            PathKind::Other => "other files",
        }
    }
}

/// One file's identity: its size and a hash of its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    len: u64,
    hash: u64,
}

/// A built folder: every file's path and fingerprint.
#[derive(Clone, Debug, Default)]
pub struct Tree(BTreeMap<RelPath, Fingerprint>);

impl Tree {
    pub fn read(dir: &Path) -> Result<Tree, String> {
        let mut files = BTreeMap::new();
        walk(dir, dir, &mut files)?;
        if files.is_empty() {
            return Err(format!("{} has no files; a diff against an empty folder would call everything new", dir.display()));
        }
        Ok(Tree(files))
    }
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<RelPath, Fingerprint>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if kind.is_dir() {
            walk(root, &path, out)?;
        } else if kind.is_file() {
            let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let mut h = DefaultHasher::new();
            bytes.hash(&mut h);
            let rel = path.strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
            out.insert(RelPath(rel), Fingerprint { len: bytes.len() as u64, hash: h.finish() });
        }
    }
    Ok(())
}

/// How one path differs between the two folders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

/// `live` is the folder now serving; `candidate` is the one about to.
pub fn diff_trees(live: &Tree, candidate: &Tree) -> DiffSummary {
    let mut changed = Vec::new();
    let mut unchanged = 0;
    for (path, fp) in &candidate.0 {
        match live.0.get(path) {
            None => changed.push(ChangedPath { path: path.clone(), kind: PathKind::of(path), change: Change::Added }),
            Some(old) if old != fp => changed.push(ChangedPath { path: path.clone(), kind: PathKind::of(path), change: Change::Changed }),
            Some(_) => unchanged += 1,
        }
    }
    for path in live.0.keys().filter(|p| !candidate.0.contains_key(*p)) {
        changed.push(ChangedPath { path: path.clone(), kind: PathKind::of(path), change: Change::Removed });
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
    DiffSummary { unchanged, by_kind: counts.into_iter().collect(), changed }
}

impl DiffSummary {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }

    /// The summary in words: a line per kind, then up to `max_paths` changed paths, the stylesheet and script ones first.
    pub fn render(&self, max_paths: usize) -> String {
        if self.is_empty() {
            return format!("nothing changed: all {} files are identical to live", self.unchanged);
        }
        let mut out = format!("{} files changed, {} unchanged\n", self.changed.len(), self.unchanged);
        for (kind, n) in &self.by_kind {
            out.push_str(&format!("  {}: {} added, {} removed, {} changed\n", kind.plural(), n.added, n.removed, n.changed));
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
