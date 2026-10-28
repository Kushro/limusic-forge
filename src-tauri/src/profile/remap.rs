//! Absolute paths in an imported profile: find them, group them by root, rewrite them.
//!
//! A profile made on `E:\Música` is useless on Linux until every reference to that folder is
//! rewritten: `local_tracks.path`, the `LOCAL:<path>` ids in history, playlists, queue and
//! shortcuts, the download rows, the folder settings, the font files of a custom theme. This module
//! does that in two steps, both pure string logic over a SQLite connection (no filesystem access
//! except the injected `exists` probe used for suggestions):
//!
//! - [`detect_roots`] lists every absolute path outside the source data dir, grouped by root, with
//!   a suggested destination for the import dialog.
//! - [`apply`] rewrites them according to the user's [`RootDecision`]s, and maps everything under
//!   the source data dir (covers, …) to the destination data dir without asking.
//!
//! ## Root heuristic
//!
//! 1. Explicit folders are roots: every `local_folders` entry, `downloads.dir`,
//!    `monitor.backups_dir`.
//! 2. Any other path under one of those roots belongs to the longest one that contains it.
//! 3. The rest is grouped by a "user folder" when it lives in one (`C:\Users\<u>\Music`,
//!    `/home/<u>/Música`, `Downloads`/`Descargas`, `Documents`, `Videos`, `Pictures`), otherwise by
//!    its parent directory (a directory reference such as `downloads.dest_dir` is its own root).
//! 4. Those groups are folded: a group under another group (with at least one path component, so
//!    `E:\` never swallows everything on the drive) merges into it. `E:\Stuff\A\x.mp3` and
//!    `E:\Stuff\z.mp3` end up as the single root `E:\Stuff`.
//!
//! Paths are matched by component, never by string prefix, so `E:\Mus` does not contain
//! `E:\Music\a.mp3`; comparison is case-insensitive when the source is Windows.
//!
//! JSON values (queue, song blobs, shortcuts, custom theme) are rewritten generically: every
//! string that is an absolute path, or `LOCAL:` plus one, is a reference. When a reference is
//! dropped the enclosing array element is removed (a queue item, a pinned shortcut).
//!
//! Not rewritten: `LOCALALBUM:` / `LOCALARTIST:` ids (they hold keys, not paths) and the journals
//! (`playlist_ops`), which are opaque undo data.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use super::{PathRoot, RootAction, RootDecision, SourceOs, WebStorage};
use crate::profile::Result;

const LOCAL_PREFIX: &str = crate::local::SONG_PREFIX;
const FOLDERS_KEY: &str = "local_folders";
const DOWNLOADS_DIR_KEY: &str = "downloads.dir";
const BACKUPS_DIR_KEY: &str = "monitor.backups_dir";
const QUEUE_KEY: &str = "queue_json";
const COVER_KEY_LIKE: &str = "playlist_cover:%";

/// Where the profile came from and where it is going.
#[derive(Debug, Clone)]
pub struct RemapContext {
    pub source_os: SourceOs,
    /// The source's data dir as it printed there ([`super::Manifest::source_data_dir`]).
    pub source_data_dir: String,
    pub dest_data_dir: PathBuf,
    /// `SourceOs::current()` at the call sites.
    pub dest_os: SourceOs,
}

/// What [`apply`] did, in values (a column, a setting, a JSON string), not rows.
#[derive(Debug, Default, Clone, Serialize)]
pub struct RemapReport {
    pub rewritten: u64,
    pub kept: u64,
    pub dropped: u64,
}

// --- path syntax --------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Prefix {
    /// `/…`
    Root,
    /// `E:\…` (upper-cased).
    Drive(char),
    /// `\\server\share\…`
    Unc(String, String),
}

#[derive(Debug, Clone)]
struct Parsed {
    prefix: Prefix,
    comps: Vec<String>,
}

fn split(s: &str, seps: &[char]) -> Vec<String> {
    s.split(|c: char| seps.contains(&c)).filter(|x| !x.is_empty()).map(String::from).collect()
}

/// An absolute path in either syntax, or `None`. Rejects URLs and anything with a newline.
fn parse(path: &str) -> Option<Parsed> {
    if path.is_empty() || path.contains(['\n', '\0']) {
        return None;
    }
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let b = path.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        return Some(Parsed {
            prefix: Prefix::Drive((b[0] as char).to_ascii_uppercase()),
            comps: split(&path[3..], &['\\', '/']),
        });
    }
    if let Some(rest) = path.strip_prefix(r"\\") {
        let mut parts = split(rest, &['\\', '/']);
        if parts.len() < 2 {
            return None;
        }
        let server = parts.remove(0);
        let share = parts.remove(0);
        return Some(Parsed { prefix: Prefix::Unc(server, share), comps: parts });
    }
    if path.starts_with('/') {
        if path.contains("://") {
            return None;
        }
        return Some(Parsed { prefix: Prefix::Root, comps: split(path, &['/']) });
    }
    None
}

/// Something that can be rewritten: not the bare `/`.
fn usable(p: &Parsed) -> bool {
    !(p.prefix == Prefix::Root && p.comps.is_empty())
}

/// Sure enough to be a file path for grouping and for the "kept" count: a `/word` string is not
/// (it could be a route fragment in some JSON blob).
fn strict(p: &Parsed) -> bool {
    p.prefix != Prefix::Root || p.comps.len() >= 2
}

fn render(p: &Parsed) -> String {
    match &p.prefix {
        Prefix::Drive(c) => format!("{c}:\\{}", p.comps.join("\\")),
        Prefix::Unc(a, b) => {
            let mut s = format!(r"\\{a}\{b}");
            for c in &p.comps {
                s.push('\\');
                s.push_str(c);
            }
            s
        }
        Prefix::Root => format!("/{}", p.comps.join("/")),
    }
}

fn comp_eq(a: &str, b: &str, ci: bool) -> bool {
    a == b || (ci && a.to_lowercase() == b.to_lowercase())
}

fn prefix_eq(a: &Prefix, b: &Prefix, ci: bool) -> bool {
    match (a, b) {
        (Prefix::Unc(s1, h1), Prefix::Unc(s2, h2)) => {
            comp_eq(s1, s2, true) && comp_eq(h1, h2, true)
        }
        _ => {
            let _ = ci;
            a == b
        }
    }
}

/// The components of `p` below `root`, or `None` when `p` is not under it (component boundaries).
fn strip_root<'a>(p: &'a Parsed, root: &Parsed, ci: bool) -> Option<&'a [String]> {
    if !prefix_eq(&p.prefix, &root.prefix, ci) || root.comps.len() > p.comps.len() {
        return None;
    }
    root.comps
        .iter()
        .zip(&p.comps)
        .all(|(a, b)| comp_eq(a, b, ci))
        .then(|| &p.comps[root.comps.len()..])
}

/// `to` plus `tail`, in the destination's syntax. `None` when `to` is empty.
fn join_dest(to: &str, tail: &[String], dest_os: SourceOs) -> Option<String> {
    if to.trim().is_empty() {
        return None;
    }
    let (sep, win) = if dest_os == SourceOs::Windows { ('\\', true) } else { ('/', false) };
    let mut base = if win { to.replace('/', "\\") } else { to.to_string() };
    while base.ends_with(sep) {
        base.pop();
    }
    if tail.is_empty() {
        if base.is_empty() || base.ends_with(':') {
            base.push(sep);
        }
        return Some(base);
    }
    base.push(sep);
    base.push_str(&tail.join(&sep.to_string()));
    Some(base)
}

/// Translate one path from under `root` to under `to`, converting separators and drive syntax for
/// `dest_os`. The prefix match is by path component and case-insensitive when `source_os` is
/// Windows. `None` when `path` is not under `root`.
pub fn translate(
    path: &str,
    root: &str,
    to: &str,
    source_os: SourceOs,
    dest_os: SourceOs,
) -> Option<String> {
    let p = parse(path)?;
    let r = parse(root)?;
    let tail = strip_root(&p, &r, source_os == SourceOs::Windows)?;
    join_dest(to, tail, dest_os)
}

// --- the mapper: one string in, one outcome out -------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum StrOutcome {
    /// Not a path, or one no decision speaks about: leave it, count nothing.
    Untouched,
    /// A path that stays as it is.
    Keep,
    Map(String),
    Drop,
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    rewritten: u64,
    kept: u64,
    dropped: u64,
}

impl Tally {
    fn moved(&self) -> u64 {
        self.rewritten + self.dropped
    }
    fn add(&mut self, o: Tally) {
        self.rewritten += o.rewritten;
        self.kept += o.kept;
        self.dropped += o.dropped;
    }
}

struct Rule {
    root: Parsed,
    action: RootAction,
}

struct Mapper {
    ci: bool,
    dest_os: SourceOs,
    /// Source data dir → destination data dir, applied before any decision.
    data: Option<(Parsed, String)>,
    /// Most specific root first.
    rules: Vec<Rule>,
}

impl Mapper {
    fn new(ctx: &RemapContext, decisions: &[RootDecision]) -> Self {
        let data = parse(&ctx.source_data_dir)
            .map(|p| (p, ctx.dest_data_dir.to_string_lossy().into_owned()));
        let mut rules: Vec<Rule> = decisions
            .iter()
            .filter_map(|d| Some(Rule { root: parse(&d.root)?, action: d.action.clone() }))
            .collect();
        rules.sort_by_key(|r| Reverse(r.root.comps.len()));
        Mapper { ci: ctx.source_os == SourceOs::Windows, dest_os: ctx.dest_os, data, rules }
    }

    /// What happens to one string: a path, or `LOCAL:` plus a path.
    fn outcome(&self, s: &str) -> StrOutcome {
        let (lead, rest) = match s.strip_prefix(LOCAL_PREFIX) {
            Some(r) => (LOCAL_PREFIX, r),
            None => ("", s),
        };
        let Some(p) = parse(rest) else { return StrOutcome::Untouched };
        if !usable(&p) {
            return StrOutcome::Untouched;
        }
        if let Some((root, to)) = &self.data {
            if let Some(tail) = strip_root(&p, root, self.ci) {
                return match join_dest(to, tail, self.dest_os) {
                    Some(new) if new != rest => StrOutcome::Map(format!("{lead}{new}")),
                    _ => StrOutcome::Keep,
                };
            }
        }
        for rule in &self.rules {
            let Some(tail) = strip_root(&p, &rule.root, self.ci) else { continue };
            return match &rule.action {
                RootAction::Keep => StrOutcome::Keep,
                RootAction::Drop => StrOutcome::Drop,
                RootAction::Map { to } => match join_dest(to, tail, self.dest_os) {
                    Some(new) if new != rest => StrOutcome::Map(format!("{lead}{new}")),
                    _ => StrOutcome::Keep,
                },
            };
        }
        if strict(&p) {
            StrOutcome::Keep
        } else {
            StrOutcome::Untouched
        }
    }

    /// Rewrite every path string in `v`. Returns true when `v` itself should be removed by its
    /// parent array (it is, or directly holds, a dropped reference).
    fn walk(&self, v: &mut Value, t: &mut Tally) -> bool {
        match v {
            Value::String(s) => match self.outcome(s) {
                StrOutcome::Map(n) => {
                    *s = n;
                    t.rewritten += 1;
                    false
                }
                StrOutcome::Keep => {
                    t.kept += 1;
                    false
                }
                StrOutcome::Drop => true,
                StrOutcome::Untouched => false,
            },
            Value::Array(a) => {
                a.retain_mut(|el| {
                    let gone = self.walk(el, t);
                    if gone {
                        t.dropped += 1;
                    }
                    !gone
                });
                false
            }
            Value::Object(o) => {
                let mut poisoned = false;
                for x in o.values_mut() {
                    if self.walk(x, t) {
                        poisoned = true;
                    }
                }
                poisoned
            }
            _ => false,
        }
    }

    /// `text` as JSON, rewritten. `Some(new text)` only when something moved.
    fn rewrite_json_text(&self, text: &str, t: &mut Tally) -> Option<String> {
        let mut v: Value = serde_json::from_str(text).ok()?;
        let mut local = Tally::default();
        self.walk(&mut v, &mut local);
        t.add(local);
        if local.moved() == 0 {
            return None;
        }
        serde_json::to_string(&v).ok()
    }
}

/// The new value of a path column after counting it. `Drop` must be handled by the caller.
fn tally_value(o: StrOutcome, t: &mut Tally) -> Option<String> {
    match o {
        StrOutcome::Map(n) => {
            t.rewritten += 1;
            Some(n)
        }
        StrOutcome::Keep => {
            t.kept += 1;
            None
        }
        _ => None,
    }
}

// --- reading the database -----------------------------------------------------------------------

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [name], |_| Ok(()))
        .optional()?
        .is_some())
}

fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get::<_, String>(0))
        .optional()?)
}

fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute("UPDATE settings SET value=?1 WHERE key=?2", params![value, key])?;
    Ok(())
}

fn del_setting(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM settings WHERE key=?1", [key])?;
    Ok(())
}

fn settings_like(conn: &Connection, like: &str) -> Result<Vec<(String, String)>> {
    let mut st = conn.prepare("SELECT key, value FROM settings WHERE key LIKE ?1")?;
    let rows = st
        .query_map([like], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
    Ok(rows)
}

fn local_ids(conn: &Connection, sql: &str) -> Result<Vec<(i64, String, String)>> {
    let mut st = conn.prepare(sql)?;
    let rows = st
        .query_map([LOCAL_PREFIX], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
    Ok(rows)
}

// --- detect -------------------------------------------------------------------------------------

struct Ref {
    p: Parsed,
    kind: &'static str,
    counted: bool,
    dir: bool,
    hint: bool,
}

struct Collector {
    ci: bool,
    data: Option<Parsed>,
    refs: Vec<Ref>,
}

impl Collector {
    fn new(ctx: &RemapContext) -> Self {
        Collector {
            ci: ctx.source_os == SourceOs::Windows,
            data: parse(&ctx.source_data_dir),
            refs: Vec::new(),
        }
    }

    fn add(&mut self, s: &str, kind: &'static str, counted: bool, dir: bool, hint: bool) {
        let rest = s.strip_prefix(LOCAL_PREFIX).unwrap_or(s);
        let Some(p) = parse(rest) else { return };
        if !usable(&p) || (!hint && !strict(&p)) {
            return;
        }
        if let Some(d) = &self.data {
            if strip_root(&p, d, self.ci).is_some() {
                return;
            }
        }
        self.refs.push(Ref { p, kind, counted, dir, hint });
    }

    fn add_json(&mut self, v: &Value, kind: &'static str, counted: bool) {
        match v {
            Value::String(s) => self.add(s, kind, counted, false, false),
            Value::Array(a) => a.iter().for_each(|x| self.add_json(x, kind, counted)),
            Value::Object(o) => o.values().for_each(|x| self.add_json(x, kind, counted)),
            _ => {}
        }
    }

    fn add_json_text(&mut self, text: &str, kind: &'static str, counted: bool) {
        if let Ok(v) = serde_json::from_str::<Value>(text) {
            self.add_json(&v, kind, counted);
        }
    }
}

fn collect(conn: &Connection, web: &WebStorage, ctx: &RemapContext) -> Result<Vec<Ref>> {
    let mut c = Collector::new(ctx);
    if has_table(conn, "settings")? {
        if let Some(raw) = get_setting(conn, FOLDERS_KEY)? {
            if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&raw) {
                for s in a.iter().filter_map(Value::as_str) {
                    c.add(s, "local_folders", false, true, true);
                }
            }
        }
        if let Some(v) = get_setting(conn, DOWNLOADS_DIR_KEY)? {
            c.add(&v, "downloads_dir", false, true, true);
        }
        if let Some(v) = get_setting(conn, BACKUPS_DIR_KEY)? {
            c.add(&v, "backups_dir", false, true, true);
        }
        for (_, v) in settings_like(conn, COVER_KEY_LIKE)? {
            c.add(&v, "playlist_covers", true, false, false);
        }
        if let Some(raw) = get_setting(conn, QUEUE_KEY)? {
            c.add_json_text(&raw, "queue", false);
        }
    }
    if has_table(conn, "local_tracks")? {
        let mut st = conn.prepare("SELECT path, cover FROM local_tracks")?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?
            .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
        for (path, cover) in rows {
            c.add(&path, "local_tracks", true, false, false);
            if let Some(cv) = cover {
                c.add(&cv, "local_covers", true, false, false);
            }
        }
    }
    if has_table(conn, "downloads")? {
        let mut st = conn.prepare("SELECT dest_dir, file_path FROM downloads")?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?
            .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
        for (dest, file) in rows {
            match file.filter(|f| !f.trim().is_empty()) {
                Some(f) => {
                    c.add(&f, "downloads", true, false, false);
                    c.add(&dest, "downloads", false, true, false);
                }
                None => c.add(&dest, "downloads", true, true, false),
            }
        }
    }
    if has_table(conn, "listens")? {
        let mut st = conn.prepare("SELECT video_id FROM listens WHERE video_id LIKE ?1 || '%'")?;
        let ids = st
            .query_map([LOCAL_PREFIX], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
        for id in ids {
            c.add(&id, "listens", true, false, false);
        }
    }
    for (table, kind) in [("listen_songs", "listens"), ("local_playlist_tracks", "local_playlists")]
    {
        if !has_table(conn, table)? {
            continue;
        }
        let rows = local_ids(
            conn,
            &format!(
                "SELECT rowid, video_id, song_json FROM {table} WHERE video_id LIKE ?1 || '%'"
            ),
        )?;
        for (_, id, json) in rows {
            c.add(&id, kind, true, false, false);
            c.add_json_text(&json, kind, false);
        }
    }
    for (k, v) in web {
        let kind = match k.as_str() {
            "limusic:personal" => "shortcuts",
            "custom-theme" => "fonts",
            _ => "webstorage",
        };
        c.add_json_text(v, kind, true);
    }
    Ok(c.refs)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UserFolder {
    Music,
    Downloads,
    Documents,
    Videos,
    Pictures,
}

fn user_folder_named(name: &str) -> Option<UserFolder> {
    match name.to_lowercase().as_str() {
        "music" | "música" | "musica" => Some(UserFolder::Music),
        "downloads" | "descargas" => Some(UserFolder::Downloads),
        "documents" | "documentos" => Some(UserFolder::Documents),
        "videos" | "vídeos" | "movies" => Some(UserFolder::Videos),
        "pictures" | "imágenes" | "imagenes" => Some(UserFolder::Pictures),
        _ => None,
    }
}

/// `C:\Users\<u>\Music…`, `/home/<u>/Música…`, `/Users/<u>/Music…` → which folder, and its root.
fn known_user_folder(p: &Parsed) -> Option<(UserFolder, Parsed)> {
    if p.comps.len() < 3 {
        return None;
    }
    let users_dir = match p.prefix {
        Prefix::Drive(_) => p.comps[0].eq_ignore_ascii_case("users"),
        Prefix::Root => p.comps[0] == "home" || p.comps[0] == "Users",
        Prefix::Unc(..) => false,
    };
    if !users_dir {
        return None;
    }
    let folder = user_folder_named(&p.comps[2])?;
    Some((folder, Parsed { prefix: p.prefix.clone(), comps: p.comps[..3].to_vec() }))
}

/// The directory a loose reference is grouped under (heuristic steps 3).
fn group_root(p: &Parsed, dir: bool) -> Parsed {
    if let Some((_, folder)) = known_user_folder(p) {
        return folder;
    }
    if dir || p.comps.is_empty() {
        return p.clone();
    }
    Parsed { prefix: p.prefix.clone(), comps: p.comps[..p.comps.len() - 1].to_vec() }
}

struct Group {
    root: Parsed,
    kinds: Vec<&'static str>,
    count: u64,
}

impl Group {
    fn absorb(&mut self, kind: &'static str, counted: bool) {
        if !self.kinds.contains(&kind) {
            self.kinds.push(kind);
        }
        if counted {
            self.count += 1;
        }
    }
}

fn group_key(p: &Parsed, ci: bool) -> String {
    let s = render(p);
    if ci {
        s.to_lowercase()
    } else {
        s
    }
}

/// Every reference grouped by root (heuristic above).
fn group_refs(refs: &[Ref], ci: bool) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    for r in refs.iter().filter(|r| r.hint) {
        let k = group_key(&r.p, ci);
        let i = *index.entry(k).or_insert_with(|| {
            groups.push(Group { root: r.p.clone(), kinds: Vec::new(), count: 0 });
            groups.len() - 1
        });
        groups[i].absorb(r.kind, r.counted);
    }
    let hint_groups = groups.len();

    let mut loose: Vec<(Parsed, &'static str, bool)> = Vec::new();
    for r in refs.iter().filter(|r| !r.hint) {
        let best = (0..hint_groups)
            .filter(|&i| strip_root(&r.p, &groups[i].root, ci).is_some())
            .max_by_key(|&i| groups[i].root.comps.len());
        match best {
            Some(i) => groups[i].absorb(r.kind, r.counted),
            None => loose.push((group_root(&r.p, r.dir), r.kind, r.counted)),
        }
    }

    // Fold: shallow roots first so a deeper one finds its ancestor already accepted.
    loose.sort_by_key(|(root, _, _)| root.comps.len());
    let mut accepted: Vec<usize> = Vec::new();
    for (root, kind, counted) in loose {
        let k = group_key(&root, ci);
        if let Some(&i) = index.get(&k) {
            groups[i].absorb(kind, counted);
            continue;
        }
        let ancestor = accepted.iter().copied().find(|&i| {
            !groups[i].root.comps.is_empty() && strip_root(&root, &groups[i].root, ci).is_some()
        });
        match ancestor {
            Some(i) => groups[i].absorb(kind, counted),
            None => {
                index.insert(k, groups.len());
                accepted.push(groups.len());
                let mut g = Group { root, kinds: Vec::new(), count: 0 };
                g.absorb(kind, counted);
                groups.push(g);
            }
        }
    }
    groups
}

// --- suggestions --------------------------------------------------------------------------------

#[derive(Default)]
struct UserDirs {
    music: Option<String>,
    downloads: Option<String>,
    documents: Option<String>,
    videos: Option<String>,
    pictures: Option<String>,
}

impl UserDirs {
    fn get(&self, f: UserFolder) -> Option<&str> {
        let slot = match f {
            UserFolder::Music => &self.music,
            UserFolder::Downloads => &self.downloads,
            UserFolder::Documents => &self.documents,
            UserFolder::Videos => &self.videos,
            UserFolder::Pictures => &self.pictures,
        };
        slot.as_deref()
    }
}

/// A mounted volume on the destination: its directory name (`e`, a label) and its path.
struct Mount {
    name: String,
    path: String,
}

/// Everything suggestions need from the machine, injectable for tests.
struct Env {
    exists: Box<dyn Fn(&str) -> bool>,
    user_dirs: UserDirs,
    mounts: Vec<Mount>,
}

fn lossy(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

impl Env {
    fn system(dest_os: SourceOs) -> Self {
        Env {
            exists: Box::new(|p| std::path::Path::new(p).exists()),
            user_dirs: UserDirs {
                music: dirs::audio_dir().map(lossy),
                downloads: dirs::download_dir().map(lossy),
                documents: dirs::document_dir().map(lossy),
                videos: dirs::video_dir().map(lossy),
                pictures: dirs::picture_dir().map(lossy),
            },
            mounts: if dest_os == SourceOs::Windows { Vec::new() } else { system_mounts() },
        }
    }
}

/// `/mnt/*`, `/media/<user>/*`, `/run/media/<user>/*`.
fn system_mounts() -> Vec<Mount> {
    fn children(dir: &std::path::Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
            .unwrap_or_default()
    }
    let mut dirs_: Vec<PathBuf> = children(std::path::Path::new("/mnt"));
    for base in ["/media", "/run/media"] {
        for user in children(std::path::Path::new(base)) {
            dirs_.extend(children(&user));
        }
    }
    dirs_
        .into_iter()
        .map(|p| Mount {
            name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            path: lossy(p),
        })
        .collect()
}

/// The proposed destination for `root` (design §5.2): as-is when it exists here, a known user
/// folder, a mounted Windows drive, or nothing.
fn suggest(root: &Parsed, ctx: &RemapContext, env: &Env) -> Option<String> {
    let root_str = render(root);
    let dest_win = ctx.dest_os == SourceOs::Windows;
    let root_win = root.prefix != Prefix::Root;

    if root_win == dest_win && (env.exists)(&root_str) {
        return Some(root_str);
    }

    if let Some((folder, base)) = known_user_folder(root) {
        if let Some(dest) = env.user_dirs.get(folder) {
            return join_dest(dest, &root.comps[base.comps.len()..], ctx.dest_os);
        }
    }

    if let (Prefix::Drive(letter), false) = (&root.prefix, dest_win) {
        let name = letter.to_ascii_lowercase().to_string();
        let is_letter = |m: &&Mount| m.name.eq_ignore_ascii_case(&name);
        let by_letter = env.mounts.iter().filter(is_letter);
        // Another mount only counts when the folder is actually in it (a labelled volume).
        let by_content = env.mounts.iter().filter(|m| !is_letter(m) && !root.comps.is_empty());
        for m in by_letter.clone().chain(by_content) {
            if let Some(c) = join_dest(&m.path, &root.comps, ctx.dest_os) {
                if (env.exists)(&c) {
                    return Some(c);
                }
            }
        }
        if let Some(m) = by_letter.clone().next() {
            return join_dest(&m.path, &root.comps, ctx.dest_os);
        }
        return None;
    }

    // WSL-style `/mnt/e/…` onto a Windows machine.
    if dest_win && root.prefix == Prefix::Root && root.comps.len() >= 2 && root.comps[0] == "mnt" {
        let l = &root.comps[1];
        if l.len() == 1 && l.chars().all(|c| c.is_ascii_alphabetic()) {
            let c = join_dest(
                &format!("{}:\\", l.to_ascii_uppercase()),
                &root.comps[2..],
                ctx.dest_os,
            )?;
            if (env.exists)(&c) {
                return Some(c);
            }
        }
    }
    None
}

fn detect_roots_with(
    conn: &Connection,
    web: &WebStorage,
    ctx: &RemapContext,
    env: &Env,
) -> Result<Vec<PathRoot>> {
    let ci = ctx.source_os == SourceOs::Windows;
    let refs = collect(conn, web, ctx)?;
    let mut out: Vec<PathRoot> = group_refs(&refs, ci)
        .into_iter()
        .map(|g| PathRoot {
            suggestion: suggest(&g.root, ctx, env),
            root: render(&g.root),
            kinds: g.kinds.iter().map(|k| k.to_string()).collect(),
            count: g.count,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.root.cmp(&b.root)));
    Ok(out)
}

/// Every absolute path in the snapshot and the webstorage, grouped by root, except what is under
/// `source_data_dir` (that is remapped automatically). Each root carries a suggested destination
/// where one could be found (see the module docs).
pub fn detect_roots(
    conn: &Connection,
    web: &WebStorage,
    ctx: &RemapContext,
) -> Result<Vec<PathRoot>> {
    detect_roots_with(conn, web, ctx, &Env::system(ctx.dest_os))
}

// --- apply --------------------------------------------------------------------------------------

/// Apply the user's decisions plus the automatic data-dir mapping to the snapshot and to the
/// webstorage. A root with no decision is kept. All database changes are one transaction. A
/// `local_tracks` row whose new path already exists is left as it was (counted as kept).
pub fn apply(
    conn: &Connection,
    web: &mut WebStorage,
    ctx: &RemapContext,
    decisions: &[RootDecision],
) -> Result<RemapReport> {
    let mapper = Mapper::new(ctx, decisions);
    let mut t = Tally::default();
    let tx = conn.unchecked_transaction()?;
    apply_db(&tx, &mapper, &mut t)?;
    tx.commit()?;
    for v in web.values_mut() {
        if let Some(new) = mapper.rewrite_json_text(v, &mut t) {
            *v = new;
        }
    }
    Ok(RemapReport { rewritten: t.rewritten, kept: t.kept, dropped: t.dropped })
}

fn apply_db(conn: &Connection, m: &Mapper, t: &mut Tally) -> Result<()> {
    if has_table(conn, "settings")? {
        apply_settings(conn, m, t)?;
    }
    if has_table(conn, "local_tracks")? {
        apply_local_tracks(conn, m, t)?;
    }
    if has_table(conn, "downloads")? {
        apply_downloads(conn, m, t)?;
    }
    if has_table(conn, "listens")? {
        apply_listens(conn, m, t)?;
    }
    if has_table(conn, "listen_songs")? {
        apply_song_rows(conn, m, t, "listen_songs", "rowid")?;
    }
    if has_table(conn, "local_playlist_tracks")? {
        apply_song_rows(conn, m, t, "local_playlist_tracks", "id")?;
    }
    Ok(())
}

/// A setting that holds one path: remapped, or removed on drop (the app falls back to its default).
fn apply_plain_setting(conn: &Connection, m: &Mapper, t: &mut Tally, key: &str) -> Result<()> {
    let Some(val) = get_setting(conn, key)? else { return Ok(()) };
    match m.outcome(&val) {
        StrOutcome::Drop => {
            del_setting(conn, key)?;
            t.dropped += 1;
        }
        o => {
            if let Some(new) = tally_value(o, t) {
                set_setting(conn, key, &new)?;
            }
        }
    }
    Ok(())
}

fn apply_settings(conn: &Connection, m: &Mapper, t: &mut Tally) -> Result<()> {
    if let Some(raw) = get_setting(conn, FOLDERS_KEY)? {
        if let Ok(mut v) = serde_json::from_str::<Value>(&raw) {
            let mut local = Tally::default();
            m.walk(&mut v, &mut local);
            t.add(local);
            if let Value::Array(a) = &mut v {
                // Two folders can map to the same place.
                let mut seen = HashSet::new();
                a.retain(|x| match x.as_str() {
                    Some(s) => seen.insert(s.to_string()),
                    None => true,
                });
            }
            if local.moved() > 0 {
                set_setting(conn, FOLDERS_KEY, &v.to_string())?;
            }
        }
    }
    apply_plain_setting(conn, m, t, DOWNLOADS_DIR_KEY)?;
    apply_plain_setting(conn, m, t, BACKUPS_DIR_KEY)?;
    for (key, _) in settings_like(conn, COVER_KEY_LIKE)? {
        apply_plain_setting(conn, m, t, &key)?;
    }
    apply_queue(conn, m, t)
}

/// `queue_json`: items under a dropped root are removed and `current` follows.
fn apply_queue(conn: &Connection, m: &Mapper, t: &mut Tally) -> Result<()> {
    let Some(raw) = get_setting(conn, QUEUE_KEY)? else { return Ok(()) };
    let Ok(mut v) = serde_json::from_str::<Value>(&raw) else { return Ok(()) };
    let mut local = Tally::default();
    if let Some(obj) = v.as_object_mut() {
        let cur = obj.get("current").and_then(Value::as_u64).unwrap_or(0) as usize;
        let mut removed_before = 0usize;
        let mut new_len = None;
        if let Some(Value::Array(items)) = obj.get_mut("items") {
            let mut idx = 0usize;
            items.retain_mut(|el| {
                let gone = m.walk(el, &mut local);
                if gone {
                    local.dropped += 1;
                    if idx < cur {
                        removed_before += 1;
                    }
                }
                idx += 1;
                !gone
            });
            new_len = Some(items.len());
        }
        for (k, val) in obj.iter_mut() {
            if k != "items" {
                m.walk(val, &mut local);
            }
        }
        if let (Some(len), true) = (new_len, removed_before > 0) {
            let current = (cur - removed_before).min(len.saturating_sub(1));
            obj.insert("current".into(), Value::from(current as u64));
        }
    }
    t.add(local);
    if local.moved() > 0 {
        set_setting(conn, QUEUE_KEY, &v.to_string())?;
    }
    Ok(())
}

fn apply_local_tracks(conn: &Connection, m: &Mapper, t: &mut Tally) -> Result<()> {
    let mut st = conn.prepare("SELECT rowid, path, cover FROM local_tracks")?;
    let rows = st
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
    drop(st);
    for (rid, path, cover) in rows {
        match m.outcome(&path) {
            StrOutcome::Drop => {
                conn.execute("DELETE FROM local_tracks WHERE rowid=?1", [rid])?;
                t.dropped += 1;
                continue;
            }
            StrOutcome::Map(new) => {
                // `path` is the primary key: a row already at the target wins, this one stays.
                let n = conn.execute(
                    "UPDATE OR IGNORE local_tracks SET path=?1 WHERE rowid=?2",
                    params![new, rid],
                )?;
                if n > 0 {
                    t.rewritten += 1;
                } else {
                    t.kept += 1;
                }
            }
            StrOutcome::Keep => t.kept += 1,
            StrOutcome::Untouched => {}
        }
        if let Some(cv) = cover {
            match m.outcome(&cv) {
                StrOutcome::Drop => {
                    conn.execute("UPDATE local_tracks SET cover=NULL WHERE rowid=?1", [rid])?;
                    t.dropped += 1;
                }
                o => {
                    if let Some(new) = tally_value(o, t) {
                        conn.execute(
                            "UPDATE local_tracks SET cover=?1 WHERE rowid=?2",
                            params![new, rid],
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn apply_downloads(conn: &Connection, m: &Mapper, t: &mut Tally) -> Result<()> {
    let mut st = conn.prepare("SELECT rowid, dest_dir, file_path FROM downloads")?;
    let rows = st
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
    drop(st);
    for (rid, dest, file) in rows {
        let d = m.outcome(&dest);
        let f = file.as_deref().map(|p| m.outcome(p));
        if d == StrOutcome::Drop || f == Some(StrOutcome::Drop) {
            conn.execute("DELETE FROM downloads WHERE rowid=?1", [rid])?;
            t.dropped += 1;
            continue;
        }
        let nd = tally_value(d, t);
        let nf = f.and_then(|o| tally_value(o, t));
        if nd.is_some() || nf.is_some() {
            conn.execute(
                "UPDATE downloads SET dest_dir=COALESCE(?1, dest_dir), \
                 file_path=COALESCE(?2, file_path) WHERE rowid=?3",
                params![nd, nf, rid],
            )?;
        }
    }
    Ok(())
}

fn apply_listens(conn: &Connection, m: &Mapper, t: &mut Tally) -> Result<()> {
    let mut st = conn.prepare("SELECT id, video_id FROM listens WHERE video_id LIKE ?1 || '%'")?;
    let rows = st
        .query_map([LOCAL_PREFIX], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
    drop(st);
    for (id, vid) in rows {
        match m.outcome(&vid) {
            StrOutcome::Drop => {
                conn.execute("DELETE FROM listens WHERE id=?1", [id])?;
                t.dropped += 1;
            }
            o => {
                if let Some(new) = tally_value(o, t) {
                    conn.execute("UPDATE listens SET video_id=?1 WHERE id=?2", params![new, id])?;
                }
            }
        }
    }
    Ok(())
}

/// `listen_songs` / `local_playlist_tracks`: the local id column and the song blob next to it.
/// A new id that collides with an existing row leaves this one as it was (kept).
fn apply_song_rows(
    conn: &Connection,
    m: &Mapper,
    t: &mut Tally,
    table: &str,
    key: &str,
) -> Result<()> {
    let rows = local_ids(
        conn,
        &format!("SELECT {key}, video_id, song_json FROM {table} WHERE video_id LIKE ?1 || '%'"),
    )?;
    for (rid, vid, json) in rows {
        let o = m.outcome(&vid);
        if o == StrOutcome::Drop {
            conn.execute(&format!("DELETE FROM {table} WHERE {key}=?1"), [rid])?;
            t.dropped += 1;
            continue;
        }
        let mut local = Tally::default();
        let new_json = m.rewrite_json_text(&json, &mut local);
        let new_id = match &o {
            StrOutcome::Map(n) => Some(n.clone()),
            _ => None,
        };
        if new_id.is_none() && new_json.is_none() {
            if o == StrOutcome::Keep {
                t.kept += 1;
            }
            t.add(local);
            continue;
        }
        let n = conn.execute(
            &format!(
                "UPDATE OR IGNORE {table} SET video_id=COALESCE(?1, video_id), \
                 song_json=COALESCE(?2, song_json) WHERE {key}=?3"
            ),
            params![new_id, new_json, rid],
        )?;
        if n > 0 {
            t.add(local);
            t.rewritten += u64::from(new_id.is_some());
        } else {
            t.kept += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use SourceOs::{Linux, Windows};

    const DATA: &str = r"C:\Users\Octavio\AppData\Roaming\com.limusic.forge";
    const DEST_DATA: &str = "/home/o/.local/share/com.limusic.forge";
    const COVER: &str = r"C:\Users\Octavio\AppData\Roaming\com.limusic.forge\covers\x.jpg";
    const MUSIC: &str = r"E:\Música";
    const DL: &str = r"C:\Users\Octavio\Music\LiMusic Forge";

    fn ctx() -> RemapContext {
        RemapContext {
            source_os: Windows,
            source_data_dir: DATA.into(),
            dest_data_dir: PathBuf::from(DEST_DATA),
            dest_os: Linux,
        }
    }

    fn env() -> Env {
        Env {
            exists: Box::new(|_| false),
            user_dirs: UserDirs {
                music: Some("/home/o/Music".into()),
                downloads: Some("/home/o/Downloads".into()),
                ..UserDirs::default()
            },
            mounts: vec![Mount { name: "e".into(), path: "/mnt/e".into() }],
        }
    }

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE local_tracks(path TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', cover TEXT);
             CREATE TABLE downloads(video_id TEXT NOT NULL, format TEXT NOT NULL,
                 dest_dir TEXT NOT NULL, file_path TEXT, PRIMARY KEY(video_id, format));
             CREATE TABLE listens(id INTEGER PRIMARY KEY, video_id TEXT NOT NULL);
             CREATE TABLE listen_songs(video_id TEXT PRIMARY KEY, song_json TEXT NOT NULL);
             CREATE TABLE local_playlist_tracks(id INTEGER PRIMARY KEY AUTOINCREMENT,
                 playlist_id INTEGER NOT NULL, video_id TEXT NOT NULL, song_json TEXT NOT NULL,
                 UNIQUE(playlist_id, video_id));",
        )
        .unwrap();
        c
    }

    fn set(c: &Connection, k: &str, v: &str) {
        c.execute("INSERT OR REPLACE INTO settings(key, value) VALUES (?1, ?2)", [k, v]).unwrap();
    }

    fn get(c: &Connection, k: &str) -> Option<String> {
        get_setting(c, k).unwrap()
    }

    fn strings(c: &Connection, sql: &str) -> Vec<String> {
        let mut st = c.prepare(sql).unwrap();
        let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn count(c: &Connection, table: &str) -> i64 {
        c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    fn seed(c: &Connection) {
        set(c, "local_folders", &json!([MUSIC]).to_string());
        set(c, "downloads.dir", DL);
        set(c, "monitor.backups_dir", &format!(r"{DATA}\backups"));
        set(c, "playlist_cover:PL1", &format!(r"{DATA}\covers\playlists\PL1.jpg"));
        set(
            c,
            "queue_json",
            &json!({
                "items": [
                    {"id": format!(r"LOCAL:{MUSIC}\Rock\a.mp3"), "title": "a"},
                    {"id": "ytid", "title": "yt"},
                    {"id": format!(r"LOCAL:{MUSIC}\b.mp3"), "title": "b"},
                ],
                "current": 2,
                "repeat": "off",
            })
            .to_string(),
        );
        let t = "INSERT INTO local_tracks(path, cover) VALUES (?1, ?2)";
        c.execute(t, [format!(r"{MUSIC}\Rock\a.mp3"), COVER.to_string()]).unwrap();
        c.execute(t, rusqlite::params![format!(r"{MUSIC}\b.mp3"), None::<String>]).unwrap();
        c.execute(
            "INSERT INTO downloads VALUES ('v1','audio',?1,?2)",
            [DL.to_string(), format!(r"{DL}\v1.m4a")],
        )
        .unwrap();
        c.execute("INSERT INTO downloads VALUES ('v2','audio',?1,NULL)", [DL.to_string()]).unwrap();
        c.execute("INSERT INTO listens VALUES (1, ?1)", [format!(r"LOCAL:{MUSIC}\Rock\a.mp3")])
            .unwrap();
        c.execute("INSERT INTO listens VALUES (2, 'abc')", []).unwrap();
        c.execute(
            "INSERT INTO listen_songs VALUES (?1, ?2)",
            [
                format!(r"LOCAL:{MUSIC}\Rock\a.mp3"),
                json!({"id": format!(r"LOCAL:{MUSIC}\Rock\a.mp3"), "thumbnail": COVER}).to_string(),
            ],
        )
        .unwrap();
        c.execute("INSERT INTO listen_songs VALUES ('abc', '{\"id\":\"abc\"}')", []).unwrap();
        c.execute(
            "INSERT INTO local_playlist_tracks(playlist_id, video_id, song_json) VALUES (1, ?1, ?2)",
            [
                format!(r"LOCAL:{MUSIC}\b.mp3"),
                json!({"id": format!(r"LOCAL:{MUSIC}\b.mp3"), "thumbnail": null}).to_string(),
            ],
        )
        .unwrap();
    }

    fn seed_web() -> WebStorage {
        let mut w = WebStorage::new();
        w.insert(
            "limusic:personal".into(),
            json!({
                "shortcuts": [{"id": format!(r"LOCAL:{MUSIC}\b.mp3"), "title": "b"}, {"id": "yt1"}],
                "pins": [format!(r"LOCAL:{MUSIC}\Rock\a.mp3")],
            })
            .to_string(),
        );
        w.insert(
            "custom-theme".into(),
            json!({"fontFiles": [r"E:\Fonts\Inter.ttf"], "fontSans": null}).to_string(),
        );
        w.insert("zoom".into(), "1.1".into());
        w
    }

    fn map(root: &str, to: &str) -> RootDecision {
        RootDecision { root: root.into(), action: RootAction::Map { to: to.into() } }
    }

    fn action(root: &str, a: RootAction) -> RootDecision {
        RootDecision { root: root.into(), action: a }
    }

    fn find<'a>(v: &'a [PathRoot], r: &str) -> &'a PathRoot {
        v.iter().find(|x| x.root == r).unwrap_or_else(|| panic!("no root {r}: {v:?}"))
    }

    // --- translate ---

    #[test]
    fn profile_remap_translate_table() {
        use SourceOs::Macos;
        // (path, root, to, source_os, dest_os, expected)
        let cases: Vec<(&str, &str, &str, SourceOs, SourceOs, Option<&str>)> = vec![
            (
                r"E:\Música\Rock\a.mp3",
                r"E:\Música",
                "/mnt/e/Música",
                Windows,
                Linux,
                Some("/mnt/e/Música/Rock/a.mp3"),
            ),
            (
                r"e:\música\Rock\a.mp3",
                r"E:\Música",
                "/mnt/e/Música",
                Windows,
                Linux,
                Some("/mnt/e/Música/Rock/a.mp3"),
            ),
            (
                r"E:/Música/a.mp3",
                r"E:\Música",
                "/mnt/e/Música",
                Windows,
                Linux,
                Some("/mnt/e/Música/a.mp3"),
            ),
            (r"E:\Music\a.mp3", r"E:\Mus", "/x", Windows, Linux, None),
            (r"E:\Música", r"E:\Música", "/mnt/e/Música", Windows, Linux, Some("/mnt/e/Música")),
            (r"E:\Música\", r"E:\Música", "/mnt/e/Música/", Windows, Linux, Some("/mnt/e/Música")),
            (r"E:\a\b.mp3", r"E:\", "/mnt/e", Windows, Linux, Some("/mnt/e/a/b.mp3")),
            (r"F:\a\b.mp3", r"E:\", "/mnt/e", Windows, Linux, None),
            (r"E:\a\b.mp3", r"E:\a", "/mnt/e/", Windows, Linux, Some("/mnt/e/b.mp3")),
            (
                r"\\nas\music\a\b.mp3",
                r"\\NAS\Music",
                "/mnt/nas",
                Windows,
                Linux,
                Some("/mnt/nas/a/b.mp3"),
            ),
            (r"\\nas\other\a.mp3", r"\\nas\music", "/mnt/nas", Windows, Linux, None),
            (r"E:\Old\x\a.mp3", r"E:\Old", r"F:\New", Windows, Windows, Some(r"F:\New\x\a.mp3")),
            (r"E:\Old\a.mp3", r"E:\Old", "F:/New/", Windows, Windows, Some(r"F:\New\a.mp3")),
            (
                "/mnt/e/Música/a.mp3",
                "/mnt/e/Música",
                r"D:\Música",
                Linux,
                Windows,
                Some(r"D:\Música\a.mp3"),
            ),
            (
                "/home/u/Music/a/b.mp3",
                "/home/u/Music",
                r"C:\Users\Octavio\Music",
                Linux,
                Windows,
                Some(r"C:\Users\Octavio\Music\a\b.mp3"),
            ),
            ("/home/u/music/a.mp3", "/home/u/Music", "/x", Linux, Linux, None),
            (
                "/home/u/Music/a.mp3",
                "/home/u/Music",
                "/home/v/Music",
                Linux,
                Linux,
                Some("/home/v/Music/a.mp3"),
            ),
            (
                "/Users/u/Music/a.mp3",
                "/Users/u/Music",
                "/home/u/Music",
                Macos,
                Linux,
                Some("/home/u/Music/a.mp3"),
            ),
            ("/home/ab/x.mp3", "/home/a", "/y", Linux, Linux, None),
            ("relative/a.mp3", "/home", "/y", Linux, Linux, None),
            ("/a/b", "relative", "/y", Linux, Linux, None),
            ("/a/b", "/a", "", Linux, Linux, None),
        ];
        for (path, root, to, src, dst, want) in cases {
            assert_eq!(
                translate(path, root, to, src, dst).as_deref(),
                want,
                "translate({path:?}, {root:?}, {to:?}, {src:?}, {dst:?})"
            );
        }
    }

    // --- detect ---

    #[test]
    fn profile_remap_detect_groups_by_root_and_skips_data_dir() {
        let c = db();
        seed(&c);
        let roots = detect_roots_with(&c, &seed_web(), &ctx(), &env()).unwrap();
        assert_eq!(roots.len(), 3, "{roots:?}");

        let m = find(&roots, MUSIC);
        // 2 tracks + 1 listen + 1 listen_song + 1 playlist track; the cover in the data dir and
        // the queue / shortcut copies do not add.
        assert_eq!(m.count, 5 + 2, "{m:?}"); // + 2 webstorage refs (shortcut + pin)
        for k in
            ["local_folders", "local_tracks", "listens", "local_playlists", "queue", "shortcuts"]
        {
            assert!(m.kinds.iter().any(|x| x == k), "{k} in {:?}", m.kinds);
        }
        assert_eq!(m.suggestion.as_deref(), Some("/mnt/e/Música"));

        let d = find(&roots, DL);
        assert_eq!(d.count, 2, "{d:?}");
        assert!(d.kinds.contains(&"downloads_dir".to_string()));
        assert!(d.kinds.contains(&"downloads".to_string()));
        assert_eq!(d.suggestion.as_deref(), Some("/home/o/Music/LiMusic Forge"));

        let f = find(&roots, r"E:\Fonts");
        assert_eq!(f.kinds, vec!["fonts".to_string()]);
        assert_eq!(f.suggestion.as_deref(), Some("/mnt/e/Fonts"));

        assert!(roots.iter().all(|r| !r.root.contains("AppData")), "data dir leaked: {roots:?}");
    }

    #[test]
    fn profile_remap_detect_folds_loose_parents() {
        let c = db();
        for p in [
            r"D:\Stuff\A\x.mp3",
            r"D:\Stuff\B\y.mp3",
            r"D:\Stuff\z.mp3",
            r"C:\Users\Ana\Música\Rock\a.mp3",
            r"C:\Users\Ana\Música\Pop\b.mp3",
            r"G:\solo.mp3",
        ] {
            c.execute("INSERT INTO local_tracks(path) VALUES (?1)", [p]).unwrap();
        }
        let roots = detect_roots_with(&c, &WebStorage::new(), &ctx(), &env()).unwrap();
        assert_eq!(roots.len(), 3, "{roots:?}");
        assert_eq!(find(&roots, r"D:\Stuff").count, 3);
        let u = find(&roots, r"C:\Users\Ana\Música");
        assert_eq!(u.count, 2);
        assert_eq!(u.suggestion.as_deref(), Some("/home/o/Music"));
        // A drive root with a single file: its own root, nothing mounted at /mnt/g.
        let g = find(&roots, r"G:\");
        assert_eq!(g.count, 1);
        assert_eq!(g.suggestion, None);
    }

    #[test]
    fn profile_remap_detect_nested_hint_roots_pick_the_longest() {
        let c = db();
        set(&c, "local_folders", &json!([r"E:\Music", r"E:\Music\Live"]).to_string());
        c.execute("INSERT INTO local_tracks(path) VALUES (?1)", [r"E:\Music\Live\a.mp3"]).unwrap();
        c.execute("INSERT INTO local_tracks(path) VALUES (?1)", [r"E:\Music\b.mp3"]).unwrap();
        let roots = detect_roots_with(&c, &WebStorage::new(), &ctx(), &env()).unwrap();
        assert_eq!(find(&roots, r"E:\Music\Live").count, 1);
        assert_eq!(find(&roots, r"E:\Music").count, 1);
    }

    #[test]
    fn profile_remap_detect_ignores_non_paths() {
        let c = db();
        set(&c, "queue_json", &json!({"items": [{"id": "x", "thumbnail": "https://i.ytimg.com/a.jpg", "route": "/watch"}]}).to_string());
        let roots = detect_roots_with(&c, &WebStorage::new(), &ctx(), &env()).unwrap();
        assert!(roots.is_empty(), "{roots:?}");
    }

    #[test]
    fn profile_remap_suggest_table() {
        // (root, existing paths, expected)
        let cases: Vec<(&str, Vec<&'static str>, Option<&str>)> = vec![
            ("/music/lib", vec!["/music/lib"], Some("/music/lib")),
            ("/music/lib", vec![], None),
            (r"C:\Users\Octavio\Downloads\x", vec![], Some("/home/o/Downloads/x")),
            (r"C:\Users\Octavio\Descargas", vec![], Some("/home/o/Downloads")),
            (r"C:\Users\Octavio\Documents", vec![], None),
            (r"E:\Música", vec![], Some("/mnt/e/Música")),
            (r"E:\", vec![], Some("/mnt/e")),
            (r"F:\Música", vec![], None),
            // A labelled volume that has the folder.
            (r"F:\Música", vec!["/run/media/o/Datos/Música"], Some("/run/media/o/Datos/Música")),
            (r"\\nas\music", vec![], None),
        ];
        for (root, existing, want) in cases {
            let mut e = env();
            e.mounts.push(Mount { name: "Datos".into(), path: "/run/media/o/Datos".into() });
            e.exists = Box::new(move |p| existing.iter().any(|x| *x == p));
            let p = parse(root).unwrap();
            assert_eq!(suggest(&p, &ctx(), &e).as_deref(), want, "root {root}");
        }
    }

    #[test]
    fn profile_remap_suggest_linux_to_windows() {
        let mut c = ctx();
        c.source_os = Linux;
        c.dest_os = Windows;
        let mut e = env();
        e.user_dirs.music = Some(r"C:\Users\Octavio\Music".into());
        let p = parse("/home/ana/Música/Rock").unwrap();
        assert_eq!(suggest(&p, &c, &e).as_deref(), Some(r"C:\Users\Octavio\Music\Rock"));
        let w = parse("/mnt/e/Música").unwrap();
        assert_eq!(suggest(&w, &c, &e), None);
        e.exists = Box::new(|p| p == r"E:\Música");
        assert_eq!(suggest(&w, &c, &e).as_deref(), Some(r"E:\Música"));
    }

    // --- apply ---

    #[test]
    fn profile_remap_apply_map_rewrites_everywhere() {
        let c = db();
        seed(&c);
        let mut web = seed_web();
        let d = [
            map(MUSIC, "/mnt/e/Música"),
            map(DL, "/home/o/Music/LiMusic Forge"),
            map(r"E:\Fonts", "/home/o/fonts"),
        ];
        let r = apply(&c, &mut web, &ctx(), &d).unwrap();
        assert_eq!(r.dropped, 0);
        assert!(r.rewritten >= 15, "{r:?}");

        assert_eq!(get(&c, "local_folders").unwrap(), json!(["/mnt/e/Música"]).to_string());
        assert_eq!(get(&c, "downloads.dir").unwrap(), "/home/o/Music/LiMusic Forge");
        assert_eq!(get(&c, "monitor.backups_dir").unwrap(), format!("{DEST_DATA}/backups"));
        assert_eq!(
            get(&c, "playlist_cover:PL1").unwrap(),
            format!("{DEST_DATA}/covers/playlists/PL1.jpg")
        );

        let q: Value = serde_json::from_str(&get(&c, "queue_json").unwrap()).unwrap();
        assert_eq!(q["items"][0]["id"], "LOCAL:/mnt/e/Música/Rock/a.mp3");
        assert_eq!(q["items"][1]["id"], "ytid");
        assert_eq!(q["items"][2]["id"], "LOCAL:/mnt/e/Música/b.mp3");
        assert_eq!(q["current"], 2);

        assert_eq!(
            strings(&c, "SELECT path FROM local_tracks ORDER BY path"),
            vec!["/mnt/e/Música/Rock/a.mp3", "/mnt/e/Música/b.mp3"]
        );
        assert_eq!(
            strings(&c, "SELECT cover FROM local_tracks WHERE cover IS NOT NULL"),
            vec![format!("{DEST_DATA}/covers/x.jpg")]
        );
        assert_eq!(
            strings(&c, "SELECT dest_dir || '|' || COALESCE(file_path,'-') FROM downloads ORDER BY video_id"),
            vec![
                "/home/o/Music/LiMusic Forge|/home/o/Music/LiMusic Forge/v1.m4a",
                "/home/o/Music/LiMusic Forge|-"
            ]
        );
        assert_eq!(
            strings(&c, "SELECT video_id FROM listens ORDER BY id"),
            vec!["LOCAL:/mnt/e/Música/Rock/a.mp3", "abc"]
        );
        let song: Value = serde_json::from_str(
            &strings(&c, "SELECT song_json FROM listen_songs WHERE video_id LIKE 'LOCAL:%'")[0],
        )
        .unwrap();
        assert_eq!(song["id"], "LOCAL:/mnt/e/Música/Rock/a.mp3");
        assert_eq!(song["thumbnail"], format!("{DEST_DATA}/covers/x.jpg"));
        assert_eq!(
            strings(&c, "SELECT video_id FROM local_playlist_tracks"),
            vec!["LOCAL:/mnt/e/Música/b.mp3"]
        );

        let personal: Value = serde_json::from_str(&web["limusic:personal"]).unwrap();
        assert_eq!(personal["shortcuts"][0]["id"], "LOCAL:/mnt/e/Música/b.mp3");
        assert_eq!(personal["shortcuts"][1]["id"], "yt1");
        assert_eq!(personal["pins"][0], "LOCAL:/mnt/e/Música/Rock/a.mp3");
        let theme: Value = serde_json::from_str(&web["custom-theme"]).unwrap();
        assert_eq!(theme["fontFiles"][0], "/home/o/fonts/Inter.ttf");
        assert_eq!(web["zoom"], "1.1");
    }

    #[test]
    fn profile_remap_apply_drop_removes_what_points_there() {
        let c = db();
        seed(&c);
        let mut web = seed_web();
        let d = [action(MUSIC, RootAction::Drop), action(DL, RootAction::Drop)];
        let r = apply(&c, &mut web, &ctx(), &d).unwrap();
        assert!(r.dropped >= 8, "{r:?}");

        assert_eq!(get(&c, "local_folders").unwrap(), "[]");
        assert_eq!(get(&c, "downloads.dir"), None);
        assert_eq!(count(&c, "local_tracks"), 0);
        assert_eq!(count(&c, "downloads"), 0);
        assert_eq!(strings(&c, "SELECT video_id FROM listens"), vec!["abc"]);
        assert_eq!(strings(&c, "SELECT video_id FROM listen_songs"), vec!["abc"]);
        assert_eq!(count(&c, "local_playlist_tracks"), 0);

        let q: Value = serde_json::from_str(&get(&c, "queue_json").unwrap()).unwrap();
        assert_eq!(q["items"].as_array().unwrap().len(), 1);
        assert_eq!(q["items"][0]["id"], "ytid");
        assert_eq!(q["current"], 0);

        let personal: Value = serde_json::from_str(&web["limusic:personal"]).unwrap();
        assert_eq!(personal["shortcuts"].as_array().unwrap().len(), 1);
        assert_eq!(personal["shortcuts"][0]["id"], "yt1");
        assert_eq!(personal["pins"], json!([]));
        // The fonts root had no decision: kept.
        let theme: Value = serde_json::from_str(&web["custom-theme"]).unwrap();
        assert_eq!(theme["fontFiles"][0], r"E:\Fonts\Inter.ttf");
        // Data-dir settings still follow the data dir.
        assert_eq!(get(&c, "monitor.backups_dir").unwrap(), format!("{DEST_DATA}/backups"));
    }

    #[test]
    fn profile_remap_apply_without_decisions_keeps_but_follows_data_dir() {
        let c = db();
        seed(&c);
        let mut web = seed_web();
        let r = apply(&c, &mut web, &ctx(), &[]).unwrap();
        assert_eq!(r.dropped, 0);
        assert!(r.kept > 0);
        assert_eq!(
            strings(&c, "SELECT path FROM local_tracks ORDER BY path"),
            vec![format!(r"{MUSIC}\Rock\a.mp3"), format!(r"{MUSIC}\b.mp3")]
        );
        assert_eq!(get(&c, "downloads.dir").unwrap(), DL);
        assert_eq!(
            strings(&c, "SELECT cover FROM local_tracks WHERE cover IS NOT NULL"),
            vec![format!("{DEST_DATA}/covers/x.jpg")]
        );
        assert_eq!(web["limusic:personal"], seed_web()["limusic:personal"]);
    }

    #[test]
    fn profile_remap_apply_respects_component_boundaries() {
        let c = db();
        c.execute("INSERT INTO local_tracks(path) VALUES (?1)", [r"E:\Music\a.mp3"]).unwrap();
        c.execute("INSERT INTO local_tracks(path) VALUES (?1)", [r"E:\Mus\b.mp3"]).unwrap();
        let r = apply(&c, &mut WebStorage::new(), &ctx(), &[map(r"E:\Mus", "/m")]).unwrap();
        assert_eq!(r.rewritten, 1);
        assert_eq!(
            strings(&c, "SELECT path FROM local_tracks ORDER BY path"),
            vec!["/m/b.mp3".to_string(), r"E:\Music\a.mp3".to_string()]
        );
    }

    #[test]
    fn profile_remap_apply_most_specific_decision_wins() {
        let c = db();
        c.execute("INSERT INTO local_tracks(path) VALUES (?1)", [r"E:\Music\Live\a.mp3"]).unwrap();
        c.execute("INSERT INTO local_tracks(path) VALUES (?1)", [r"E:\Music\b.mp3"]).unwrap();
        let d = [map(r"E:\Music", "/m"), action(r"E:\Music\Live", RootAction::Drop)];
        apply(&c, &mut WebStorage::new(), &ctx(), &d).unwrap();
        assert_eq!(strings(&c, "SELECT path FROM local_tracks"), vec!["/m/b.mp3"]);
    }

    #[test]
    fn profile_remap_apply_primary_key_conflict_keeps_the_old_row() {
        let c = db();
        c.execute("INSERT INTO local_tracks(path, title) VALUES (?1, 'old')", [r"E:\M\a.mp3"])
            .unwrap();
        c.execute("INSERT INTO local_tracks(path, title) VALUES (?1, 'new')", ["/mnt/e/M/a.mp3"])
            .unwrap();
        let r = apply(&c, &mut WebStorage::new(), &ctx(), &[map(r"E:\M", "/mnt/e/M")]).unwrap();
        assert_eq!(r.rewritten, 0);
        assert!(r.kept >= 1);
        assert_eq!(count(&c, "local_tracks"), 2);
        assert_eq!(
            strings(&c, "SELECT title FROM local_tracks WHERE path = 'E:\\M\\a.mp3'"),
            vec!["old"]
        );
    }

    #[test]
    fn profile_remap_apply_playlist_unique_conflict_keeps_the_old_row() {
        let c = db();
        let sql = "INSERT INTO local_playlist_tracks(playlist_id, video_id, song_json) VALUES (1, ?1, '{}')";
        c.execute(sql, [r"LOCAL:E:\M\a.mp3"]).unwrap();
        c.execute(sql, ["LOCAL:/mnt/e/M/a.mp3"]).unwrap();
        apply(&c, &mut WebStorage::new(), &ctx(), &[map(r"E:\M", "/mnt/e/M")]).unwrap();
        assert_eq!(count(&c, "local_playlist_tracks"), 2);
    }

    #[test]
    fn profile_remap_apply_folders_dedupe_after_mapping() {
        let c = db();
        set(&c, "local_folders", &json!([r"E:\A", r"F:\A", r"G:\Z"]).to_string());
        let d = [map(r"E:\A", "/mnt/x"), map(r"F:\A", "/mnt/x")];
        apply(&c, &mut WebStorage::new(), &ctx(), &d).unwrap();
        assert_eq!(get(&c, "local_folders").unwrap(), json!(["/mnt/x", r"G:\Z"]).to_string());
    }

    #[test]
    fn profile_remap_apply_windows_data_dir_is_case_insensitive() {
        let c = db();
        c.execute(
            "INSERT INTO local_tracks(path, cover) VALUES (?1, ?2)",
            [r"E:\a.mp3", r"c:\users\octavio\appdata\roaming\COM.LIMUSIC.FORGE\covers\y.png"],
        )
        .unwrap();
        apply(&c, &mut WebStorage::new(), &ctx(), &[]).unwrap();
        assert_eq!(
            strings(&c, "SELECT cover FROM local_tracks"),
            vec![format!("{DEST_DATA}/covers/y.png")]
        );
    }

    #[test]
    fn profile_remap_apply_same_machine_is_a_no_op() {
        let c = db();
        seed(&c);
        let mut web = seed_web();
        let same = RemapContext {
            source_os: Windows,
            source_data_dir: DATA.into(),
            dest_data_dir: PathBuf::from(DATA),
            dest_os: Windows,
        };
        let before = strings(&c, "SELECT path FROM local_tracks ORDER BY path");
        let r = apply(&c, &mut web, &same, &[]).unwrap();
        assert_eq!((r.rewritten, r.dropped), (0, 0));
        assert_eq!(strings(&c, "SELECT path FROM local_tracks ORDER BY path"), before);
        assert_eq!(get(&c, "monitor.backups_dir").unwrap(), format!(r"{DATA}\backups"));
    }

    #[test]
    fn profile_remap_apply_linux_to_windows() {
        let c = db();
        c.execute(
            "INSERT INTO local_tracks(path, cover) VALUES (?1, ?2)",
            ["/mnt/e/Música/a.mp3", "/home/o/.local/share/lm/covers/c.jpg"],
        )
        .unwrap();
        c.execute("INSERT INTO listens VALUES (1, 'LOCAL:/mnt/e/Música/a.mp3')", []).unwrap();
        let ctx = RemapContext {
            source_os: Linux,
            source_data_dir: "/home/o/.local/share/lm".into(),
            dest_data_dir: PathBuf::from(r"C:\Users\O\AppData\Roaming\lm"),
            dest_os: Windows,
        };
        apply(&c, &mut WebStorage::new(), &ctx, &[map("/mnt/e/Música", r"D:\Música")]).unwrap();
        assert_eq!(strings(&c, "SELECT path FROM local_tracks"), vec![r"D:\Música\a.mp3"]);
        assert_eq!(
            strings(&c, "SELECT cover FROM local_tracks"),
            vec![r"C:\Users\O\AppData\Roaming\lm\covers\c.jpg"]
        );
        assert_eq!(strings(&c, "SELECT video_id FROM listens"), vec![r"LOCAL:D:\Música\a.mp3"]);
    }

    #[test]
    fn profile_remap_apply_tolerates_missing_tables_and_bad_json() {
        let c = Connection::open_in_memory().unwrap();
        let r = apply(&c, &mut WebStorage::new(), &ctx(), &[]).unwrap();
        assert_eq!((r.rewritten, r.kept, r.dropped), (0, 0, 0));

        let c = db();
        set(&c, "queue_json", "not json");
        set(&c, "local_folders", "{broken");
        let mut web = WebStorage::new();
        web.insert("limusic:personal".into(), "not json".into());
        apply(&c, &mut web, &ctx(), &[action("/x", RootAction::Drop)]).unwrap();
        assert_eq!(get(&c, "queue_json").unwrap(), "not json");
        assert_eq!(web["limusic:personal"], "not json");
    }

    #[test]
    fn profile_remap_apply_queue_current_follows_removed_items() {
        let c = db();
        let q = json!({
            "items": [
                {"id": r"LOCAL:E:\X\a.mp3"}, {"id": "yt1"}, {"id": r"LOCAL:E:\X\b.mp3"}, {"id": "yt2"}
            ],
            "current": 3,
            "shuffleOrig": [{"id": r"LOCAL:E:\X\a.mp3"}, {"id": "yt1"}],
        });
        set(&c, "queue_json", &q.to_string());
        apply(&c, &mut WebStorage::new(), &ctx(), &[action(r"E:\X", RootAction::Drop)]).unwrap();
        let q: Value = serde_json::from_str(&get(&c, "queue_json").unwrap()).unwrap();
        assert_eq!(q["items"], json!([{"id": "yt1"}, {"id": "yt2"}]));
        assert_eq!(q["current"], 1);
        assert_eq!(q["shuffleOrig"], json!([{"id": "yt1"}]));
    }
}
