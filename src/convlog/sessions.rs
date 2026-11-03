//! One file per conversation.
//!
//! A conversation's v4 lines (see the parent module) go to a file of its own,
//! so it is kept, downloaded, reviewed and deleted **whole**:
//!
//! ```text
//! sessions/2026-11-03/alice__a3f19c.jsonl.zst   sealed lines: concatenated zstd frames
//! sessions/2026-11-03/alice__a3f19c.jsonl       open tail: lines being appended
//! sessions/2026-11-03/alice__a3f19c.meta.json   summary written at each seal
//! sessions/2026-11-03/alice__oneshot.jsonl…     one client's session-less requests that day
//! sessions/deleted.json                         conversations deleted by an operator
//! ```
//!
//! The day is that of the conversation's first turn; the name is the client
//! (sanitized) and the session tag. A file is a **unit**: usually one
//! conversation, the client's one-shot requests of a day, or (by a name
//! collision) a few — every reader filters lines by key, and the meta names
//! the keys, so nothing ever parses a key back out of a file name. A unit that
//! grows past [`PART_MAX_RAW`] closes and the conversation continues in
//! `….p2`, each part self-contained.
//!
//! A tail is **sealed** once it has been quiet for [`SEAL_IDLE`] (or grows past
//! [`SEAL_TAIL_BYTES`]): compressed into one more frame of the `.zst` and
//! removed. Deltas never refer outside their file, and a sealed frame stays in
//! the same file, so a conversation keeps one delta history from its first
//! turn to its last.
//!
//! **Retention** keeps or drops whole units: past `max_days`, then oldest
//! first until the total is within `max_total_bytes`. An open tail or a
//! conversation the hot store still holds is never touched.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{Conv, DiskIndex, Find, TurnSum, TurnSums, codec, fold_head, fold_line, parse_line, turn_sums};
use crate::lock::LockExt;

/// Quiet time after which an open tail is compressed into its unit's `.zst`.
pub const SEAL_IDLE: Duration = Duration::from_secs(3600);
/// A tail this large is sealed at once, whatever its pace.
pub const SEAL_TAIL_BYTES: u64 = 8 << 20;
/// Uncompressed size past which a unit closes and its conversation continues
/// in a new part.
pub const PART_MAX_RAW: u64 = 256 << 20;
/// How often retention runs.
pub const RETENTION_EVERY: Duration = Duration::from_secs(600);
/// Directory under the log folder that holds the units.
pub const SESSIONS_DIR: &str = "sessions";
/// The deleted-conversation list, inside [`SESSIONS_DIR`].
const DELETED_FILE: &str = "deleted.json";
const DAY_MS: u64 = 86_400_000;

/// How much history is kept.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Retention {
    /// Bytes on disk across every unit and any legacy chain file.
    pub max_total_bytes: u64,
    /// Days a conversation is kept after its last turn; 0 = no age limit.
    pub max_days: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self { max_total_bytes: 1 << 30, max_days: 30 }
    }
}

fn with_ext(base: &Path, ext: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(ext);
    PathBuf::from(s)
}

fn zst_of(base: &Path) -> PathBuf {
    with_ext(base, ".jsonl.zst")
}

fn tail_of(base: &Path) -> PathBuf {
    with_ext(base, ".jsonl")
}

fn meta_of(base: &Path) -> PathBuf {
    with_ext(base, ".meta.json")
}

/// A unit's base path from one of its file names, `None` for anything else.
pub fn base_of(file: &Path) -> Option<PathBuf> {
    let name = file.file_name()?.to_str()?;
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))
        .or_else(|| name.strip_suffix(".meta.json"))?;
    Some(file.with_file_name(stem))
}

/// A file-name-safe rendering of a client or session tag.
pub fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .take(64)
        .collect();
    // Never empty, never a hidden file or a relative path element.
    match out.trim_start_matches('.') {
        "" => "_".to_string(),
        t => t.to_string(),
    }
}

/// `YYYY-MM-DD` (UTC) of a unix-ms instant.
pub fn day_of(ms: u64) -> String {
    // Days since the epoch to a civil date (Howard Hinnant's algorithm).
    let z = (ms / DAY_MS) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// One turn's row facts as the meta stores them.
#[derive(Serialize, Deserialize)]
struct MetaTurn(u64, u64, bool, #[serde(default)] Vec<crate::audit::Flag>);

#[derive(Serialize, Deserialize)]
struct MetaConv {
    head: Conv,
    turns: Vec<MetaTurn>,
}

/// The sidecar a seal writes: enough to list and find the unit's
/// conversations at boot without decompressing it.
#[derive(Serialize, Deserialize)]
struct Meta {
    v: u32,
    raw_bytes: u64,
    convs: Vec<MetaConv>,
}

/// One file unit as the index knows it.
struct Unit {
    /// Identity of this unit for the writer: deltas never cross it.
    id: u64,
    /// Per conversation key: its header and turns as far as this unit goes.
    convs: HashMap<String, (Conv, TurnSums)>,
    zst_bytes: u64,
    tail_bytes: u64,
    meta_bytes: u64,
    /// Uncompressed bytes of every line in the unit.
    raw_bytes: u64,
    last_write_ms: u64,
    /// Past [`PART_MAX_RAW`]: the next turn opens a new part.
    full: bool,
}

impl Unit {
    fn new(id: u64) -> Self {
        Self {
            id,
            convs: HashMap::new(),
            zst_bytes: 0,
            tail_bytes: 0,
            meta_bytes: 0,
            raw_bytes: 0,
            last_write_ms: 0,
            full: false,
        }
    }

    fn end_ms(&self) -> u64 {
        self.convs.values().map(|(h, _)| h.end_ms).max().unwrap_or(self.last_write_ms)
    }

    fn disk_bytes(&self) -> u64 {
        self.zst_bytes + self.tail_bytes + self.meta_bytes
    }

    /// Fold one line of the unit into what the index knows.
    fn absorb(&mut self, line: &Conv) {
        let e = self
            .convs
            .entry(line.key.clone())
            .or_insert_with(|| (line.head(), TurnSums::new()));
        fold_head(&mut e.0, line);
        e.1.extend(turn_sums(&line.turns, 0));
    }

    fn first_at(&self, key: &str) -> u64 {
        self.convs
            .get(key)
            .and_then(|(_, t)| t.values().map(|t| t.at_ms).min())
            .unwrap_or(u64::MAX)
    }
}

#[derive(Default)]
struct Index {
    units: HashMap<PathBuf, Unit>,
    /// Per key, the units holding its lines, oldest part first.
    by_key: HashMap<String, Vec<PathBuf>>,
    next_id: u64,
}

impl Index {
    fn add_unit(&mut self, base: PathBuf, mut unit: Unit) {
        self.next_id += 1;
        unit.id = self.next_id;
        for key in unit.convs.keys() {
            let list = self.by_key.entry(key.clone()).or_default();
            if !list.contains(&base) {
                list.push(base.clone());
            }
        }
        self.units.insert(base, unit);
    }

    fn sort_keys(&mut self) {
        let units = &self.units;
        for (key, list) in self.by_key.iter_mut() {
            list.sort_by_key(|b| units.get(b).map_or(u64::MAX, |u| u.first_at(key)));
        }
    }

    fn remove_unit(&mut self, base: &Path) -> Option<Unit> {
        let unit = self.units.remove(base)?;
        for key in unit.convs.keys() {
            if let Some(list) = self.by_key.get_mut(key) {
                list.retain(|b| b != base);
                if list.is_empty() {
                    self.by_key.remove(key);
                }
            }
        }
        Some(unit)
    }

    /// Header and turns of one key across all of its units.
    fn row(&self, key: &str) -> Option<(Conv, TurnSums)> {
        let mut out: Option<(Conv, TurnSums)> = None;
        for base in self.by_key.get(key)? {
            let Some((head, turns)) = self.units.get(base).and_then(|u| u.convs.get(key)) else {
                continue;
            };
            match out.as_mut() {
                Some((h, t)) => {
                    fold_head(h, head);
                    t.extend(turns.iter().map(|(k, v)| (*k, v.clone())));
                }
                None => out = Some((head.clone(), turns.clone())),
            }
        }
        out
    }
}

/// An operator-deleted conversation: lines of `key` up to `upto_rid` are
/// hidden wherever they still sit (a unit shared with other conversations,
/// the legacy chain), until those files are gone too.
#[derive(Serialize, Deserialize, Clone)]
struct Tomb {
    upto_rid: u64,
    at_ms: u64,
    /// Units (relative to the sessions dir) that still hold its lines.
    #[serde(default)]
    files: Vec<PathBuf>,
    /// The legacy chain held lines of it when it was deleted.
    #[serde(default)]
    legacy: bool,
}

/// The deleted-conversation list, shared by the session store and the legacy
/// chain's index so both hide the same lines.
pub struct Tombs {
    path: PathBuf,
    map: Mutex<HashMap<String, Tomb>>,
}

impl Tombs {
    fn load(path: PathBuf) -> Self {
        let map = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, map: Mutex::new(map) }
    }

    /// For tests: no deletions, nothing persisted.
    #[cfg(test)]
    pub fn none() -> Self {
        Self { path: PathBuf::new(), map: Mutex::new(HashMap::new()) }
    }

    /// Strip a line of what was deleted; `false` when nothing is left of it.
    pub fn mask(&self, line: &mut Conv) -> bool {
        let map = self.map.lock_or_recover();
        let Some(t) = map.get(&line.key) else { return true };
        line.turns.retain(|x| x.rid > t.upto_rid);
        !line.turns.is_empty()
    }

    fn save(&self, map: &HashMap<String, Tomb>) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let tmp = with_ext(&self.path, ".tmp");
        let ok = serde_json::to_vec(map)
            .ok()
            .is_some_and(|b| std::fs::write(&tmp, b).is_ok() && std::fs::rename(&tmp, &self.path).is_ok());
        if !ok {
            warn!("conversation log: could not save {}", self.path.display());
        }
    }

    /// Load the list that sits in a sessions directory, for offline readers.
    pub fn read_dir(sessions_dir: &Path) -> Self {
        let mut t = Self::load(sessions_dir.join(DELETED_FILE));
        t.path = PathBuf::new(); // read-only
        t
    }
}

/// What a unit read hands the fold: its lines, oldest first.
fn unit_bytes(base: &Path) -> Vec<u8> {
    let mut out = codec::read_slot_file(&zst_of(base)).unwrap_or_default();
    if let Ok(tail) = std::fs::read(tail_of(base)) {
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend(tail);
    }
    out
}

fn mtime_ms(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// The per-conversation store: index, file layout, sealing, retention,
/// deletion. Shared by the writer task (appends, seals, sweeps) and the web
/// API (reads, deletes).
pub struct SessionStore {
    root: PathBuf,
    index: Mutex<Index>,
    pub tombs: Arc<Tombs>,
    pub retention: Retention,
    /// The last conversation read back, keyed by key and the sizes of its
    /// files: opening one turn after another reads its files once.
    cache: Mutex<Option<(String, u64, Arc<Conv>)>>,
    scan_done: AtomicBool,
    /// [`PART_MAX_RAW`], a field so a test can split a part without 256 MiB.
    part_max: u64,
}

impl SessionStore {
    /// Open (creating) the sessions directory and load the deleted list.
    pub fn open(root: PathBuf, retention: Retention) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        // Prove the directory takes writes now, not at the first flush.
        let probe = root.join(".probe");
        std::fs::write(&probe, b"")?;
        let _ = std::fs::remove_file(&probe);
        let tombs = Arc::new(Tombs::load(root.join(DELETED_FILE)));
        Ok(Self {
            root,
            index: Mutex::new(Index::default()),
            tombs,
            retention,
            cache: Mutex::new(None),
            scan_done: AtomicBool::new(false),
            part_max: PART_MAX_RAW,
        })
    }

    #[cfg(test)]
    pub fn set_part_max(&mut self, n: u64) {
        self.part_max = n;
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn scanned(&self) -> bool {
        self.scan_done.load(Ordering::Relaxed)
    }

    /// Every unit base under the root.
    fn unit_bases(&self) -> Vec<PathBuf> {
        let mut out: HashSet<PathBuf> = HashSet::new();
        let Ok(days) = std::fs::read_dir(&self.root) else { return Vec::new() };
        for day in days.flatten() {
            let Ok(files) = std::fs::read_dir(day.path()) else { continue };
            for f in files.flatten() {
                if let Some(b) = base_of(&f.path()) {
                    out.insert(b);
                }
            }
        }
        let mut v: Vec<PathBuf> = out.into_iter().collect();
        v.sort();
        v
    }

    /// Index one unit from its meta (sealed, unchanged since) or its lines.
    fn scan_unit(&self, base: &Path) -> Option<Unit> {
        let (zst, tail, meta) = (zst_of(base), tail_of(base), meta_of(base));
        let mut unit = Unit::new(0);
        unit.zst_bytes = file_len(&zst);
        unit.tail_bytes = file_len(&tail);
        unit.meta_bytes = file_len(&meta);
        unit.last_write_ms = mtime_ms(&tail).or_else(|| mtime_ms(&zst)).unwrap_or(0);
        let from_meta = (unit.tail_bytes == 0)
            .then(|| std::fs::read(&meta).ok())
            .flatten()
            .and_then(|b| serde_json::from_slice::<Meta>(&b).ok());
        match from_meta {
            Some(m) => {
                unit.raw_bytes = m.raw_bytes;
                let tombs = self.tombs.map.lock_or_recover();
                for c in m.convs {
                    let upto = tombs.get(&c.head.key).map_or(0, |t| t.upto_rid);
                    let turns: TurnSums = c
                        .turns
                        .into_iter()
                        .filter(|t| t.0 > upto)
                        .map(|t| (t.0, TurnSum { slot: 0, at_ms: t.1, open: t.2, flags: t.3 }))
                        .collect();
                    if !turns.is_empty() {
                        unit.convs.insert(c.head.key.clone(), (c.head, turns));
                    }
                }
            }
            None => {
                let data = unit_bytes(base);
                unit.raw_bytes = data.len() as u64;
                for raw in data.split(|b| *b == b'\n') {
                    if let Some(mut line) = parse_line(raw)
                        && self.tombs.mask(&mut line)
                    {
                        unit.absorb(&line);
                    }
                }
            }
        }
        unit.full = unit.raw_bytes > self.part_max;
        (unit.disk_bytes() > 0).then_some(unit)
    }

    /// Index every unit on disk. Metas make this cheap: only open tails (the
    /// last hour's conversations) are parsed.
    pub fn scan(&self) {
        let mut found = Vec::new();
        for base in self.unit_bases() {
            if let Some(u) = self.scan_unit(&base) {
                found.push((base, u));
            }
        }
        let mut ix = self.index.lock_or_recover();
        for (base, u) in found {
            ix.add_unit(base, u);
        }
        ix.sort_keys();
        drop(ix);
        self.scan_done.store(true, Ordering::Relaxed);
    }

    /// The largest rid the units hold.
    pub fn max_rid(&self) -> u64 {
        let ix = self.index.lock_or_recover();
        ix.units
            .values()
            .flat_map(|u| u.convs.values())
            .filter_map(|(_, t)| t.last_key_value().map(|(r, _)| *r))
            .max()
            .unwrap_or(0)
    }

    /// The unit a line of `key` goes to, registered and with its directory
    /// made: the key's current part, else a new one named after the
    /// conversation. Returns the unit's identity and its tail path.
    pub fn unit_for(&self, key: &str, head: &Conv, at_ms: u64) -> std::io::Result<(u64, PathBuf)> {
        let mut ix = self.index.lock_or_recover();
        if let Some(base) = ix.by_key.get(key).and_then(|l| l.last())
            && let Some(u) = ix.units.get(base)
            && !u.full
        {
            return Ok((u.id, tail_of(base)));
        }
        let user = sanitize(&head.user);
        let (tag, day_ms) = match head.session.as_deref() {
            Some(s) => (sanitize(s), if head.start_ms > 0 { head.start_ms } else { at_ms }),
            None => ("oneshot".to_string(), at_ms),
        };
        let dir = self.root.join(day_of(day_ms));
        let mut base = dir.join(format!("{user}__{tag}"));
        let mut part = 1;
        while ix.units.get(&base).is_some_and(|u| u.full) {
            part += 1;
            base = dir.join(format!("{user}__{tag}.p{part}"));
        }
        std::fs::create_dir_all(&dir)?;
        if !ix.units.contains_key(&base) {
            // Files left by an earlier run that the index lost track of
            // (deleted, then a write raced in): pick them up, masked.
            let unit = self.scan_unit(&base).unwrap_or_else(|| Unit::new(0));
            ix.add_unit(base.clone(), unit);
        }
        let list = ix.by_key.entry(key.to_string()).or_default();
        if !list.contains(&base) {
            list.push(base.clone());
        }
        let id = ix.units.get(&base).map_or(0, |u| u.id);
        Ok((id, tail_of(&base)))
    }

    /// A line of `n` bytes reached unit `id`'s tail. `false` when the unit
    /// went away meanwhile (deleted): the caller removes what it wrote.
    pub fn record(&self, id: u64, tail: &Path, n: u64, line: &Conv) -> bool {
        let Some(base) = base_of(tail) else { return false };
        let mut ix = self.index.lock_or_recover();
        let Some(u) = ix.units.get_mut(&base).filter(|u| u.id == id) else {
            return false;
        };
        u.tail_bytes += n;
        u.raw_bytes += n;
        u.last_write_ms = super::now_unix_millis();
        u.absorb(line);
        u.full = u.raw_bytes > self.part_max;
        let list = ix.by_key.entry(line.key.clone()).or_default();
        if !list.contains(&base) {
            list.push(base);
        }
        drop(ix);
        self.cache.lock_or_recover().take();
        true
    }

    /// Whether unit `id` is still registered at the tail's base.
    pub fn holds(&self, id: u64, tail: &Path) -> bool {
        base_of(tail).is_some_and(|b| self.index.lock_or_recover().units.get(&b).is_some_and(|u| u.id == id))
    }

    /// Units whose open tail is due for sealing.
    pub fn due_for_seal(&self, now: u64) -> Vec<(u64, PathBuf)> {
        let idle = SEAL_IDLE.as_millis() as u64;
        let ix = self.index.lock_or_recover();
        ix.units
            .iter()
            .filter(|(_, u)| {
                u.tail_bytes > 0
                    && (now.saturating_sub(u.last_write_ms) >= idle || u.tail_bytes >= SEAL_TAIL_BYTES)
            })
            .map(|(b, u)| (u.id, b.clone()))
            .collect()
    }

    /// Compress unit `id`'s tail into one more frame of its `.zst`, remove
    /// the tail and rewrite the meta. The writer closes its handle first;
    /// blocking file work, run off the async threads.
    pub fn seal(&self, id: u64, base: &Path) -> std::io::Result<()> {
        let tail = tail_of(base);
        let data = std::fs::read(&tail)?;
        let zst = zst_of(base);
        let mut out = std::fs::read(&zst).unwrap_or_default();
        if !data.is_empty() {
            out.extend(zstd::bulk::compress(&data, 9)?);
        }
        let tmp = with_ext(&zst, ".tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&out)?;
            f.sync_all()?;
        }
        // Under the index lock: a delete cannot interleave with the swap.
        let mut ix = self.index.lock_or_recover();
        let Some(u) = ix.units.get_mut(base).filter(|u| u.id == id) else {
            let _ = std::fs::remove_file(&tmp);
            return Ok(());
        };
        std::fs::rename(&tmp, &zst)?;
        std::fs::remove_file(&tail)?;
        u.zst_bytes = out.len() as u64;
        u.tail_bytes = 0;
        let meta = Meta {
            v: 1,
            raw_bytes: u.raw_bytes,
            convs: u
                .convs
                .values()
                .map(|(h, t)| MetaConv {
                    head: h.clone(),
                    turns: t
                        .iter()
                        .map(|(rid, s)| MetaTurn(*rid, s.at_ms, s.open, s.flags.clone()))
                        .collect(),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec(&meta).map_err(std::io::Error::other)?;
        let mtmp = with_ext(&meta_of(base), ".tmp");
        std::fs::write(&mtmp, &bytes)?;
        std::fs::rename(&mtmp, meta_of(base))?;
        u.meta_bytes = bytes.len() as u64;
        Ok(())
    }

    /// Every conversation's header and turn facts.
    pub fn rows(&self) -> Vec<(Conv, TurnSums)> {
        let ix = self.index.lock_or_recover();
        ix.by_key.keys().filter_map(|k| ix.row(k)).collect()
    }

    /// The key `q` names and its `end_ms`, newest match wins.
    pub fn find(&self, q: &Find) -> Option<(String, u64)> {
        let ix = self.index.lock_or_recover();
        if let Some(k) = q.key {
            return ix.row(k).map(|(h, _)| (k.to_string(), h.end_ms));
        }
        let mut best: Option<(String, u64)> = None;
        for key in ix.by_key.keys() {
            let Some((h, t)) = ix.row(key) else { continue };
            let hit = match q {
                Find { rid: Some(rid), session, user, .. } => {
                    t.contains_key(rid)
                        && session.is_none_or(|x| h.session.as_deref() == Some(x))
                        && user.is_none_or(|x| h.user == x)
                }
                Find { session: Some(x), user, .. } => {
                    h.session.as_deref() == Some(x) && user.is_none_or(|u| h.user == *u)
                }
                Find { user: Some(u), session: None, rid: None, .. } => h.user == *u,
                _ => false,
            };
            if hit && best.as_ref().is_none_or(|(_, ms)| h.end_ms > *ms) {
                best = Some((key.clone(), h.end_ms));
            }
        }
        best
    }

    /// One conversation read back from its units, requests as stored.
    pub fn read(&self, key: &str) -> Option<Conv> {
        let bases: Vec<(PathBuf, u64)> = {
            let ix = self.index.lock_or_recover();
            ix.by_key
                .get(key)?
                .iter()
                .filter_map(|b| ix.units.get(b).map(|u| (b.clone(), u.disk_bytes())))
                .collect()
        };
        let sig = bases.iter().map(|(_, n)| *n).sum::<u64>() ^ ((bases.len() as u64) << 56);
        if let Some((k, s, c)) = self.cache.lock_or_recover().as_ref()
            && k == key
            && *s == sig
        {
            return Some((**c).clone());
        }
        let mut acc: Option<Conv> = None;
        for (base, _) in &bases {
            for raw in unit_bytes(base).split(|b| *b == b'\n') {
                let Some(mut line) = parse_line(raw).filter(|l| l.key == key) else { continue };
                if !self.tombs.mask(&mut line) {
                    continue;
                }
                match acc.as_mut() {
                    Some(a) => fold_line(a, line),
                    None => {
                        let mut a = line.head();
                        fold_line(&mut a, line);
                        acc = Some(a);
                    }
                }
            }
        }
        let mut conv = acc?;
        conv.turns.sort_by_key(|t| t.rid);
        *self.cache.lock_or_recover() = Some((key.to_string(), sig, Arc::new(conv.clone())));
        Some(conv)
    }

    /// Remove a unit's files (the index lock is held by the caller).
    fn remove_files(base: &Path) {
        for p in [zst_of(base), tail_of(base), meta_of(base)] {
            let _ = std::fs::remove_file(p);
        }
        if let Some(dir) = base.parent() {
            let _ = std::fs::remove_dir(dir); // only when empty
        }
    }

    /// Delete one conversation: units that hold only it are removed; lines it
    /// left in shared units or the legacy chain are hidden by a tombstone
    /// until those go too. Returns whether the store held any of it.
    pub fn delete(&self, key: &str, upto_rid: u64, legacy: bool) -> bool {
        let mut ix = self.index.lock_or_recover();
        let bases = ix.by_key.remove(key).unwrap_or_default();
        let held = !bases.is_empty();
        let mut shared = Vec::new();
        for base in bases {
            let Some(u) = ix.units.get_mut(&base) else { continue };
            u.convs.remove(key);
            if u.convs.is_empty() {
                Self::remove_files(&base);
                ix.units.remove(&base);
            } else if let Ok(rel) = base.strip_prefix(&self.root) {
                shared.push(rel.to_path_buf());
            }
        }
        drop(ix);
        self.cache.lock_or_recover().take();
        let mut map = self.tombs.map.lock_or_recover();
        let at_ms = super::now_unix_millis();
        let t = map.entry(key.to_string()).or_insert(Tomb { upto_rid, at_ms, files: Vec::new(), legacy });
        t.upto_rid = t.upto_rid.max(upto_rid);
        t.at_ms = at_ms;
        t.legacy |= legacy;
        for f in shared {
            if !t.files.contains(&f) {
                t.files.push(f);
            }
        }
        self.tombs.save(&map);
        held
    }

    /// Bytes on disk across the units, and the oldest unit's last activity.
    pub fn usage(&self) -> (u64, Option<u64>) {
        let ix = self.index.lock_or_recover();
        let bytes = ix.units.values().map(Unit::disk_bytes).sum();
        let oldest = ix.units.values().map(Unit::end_ms).min();
        (bytes, oldest)
    }

    /// Apply retention. Candidates are sealed units none of whose
    /// conversations the hot store holds, plus the legacy chain's files:
    /// everything older than `max_days` goes, then the oldest until the total
    /// fits `max_total_bytes`. A unit goes whole.
    pub fn sweep(&self, hot: &HashSet<String>, legacy: Option<&DiskIndex>, now: u64) {
        enum Cand {
            Unit(PathBuf, u64),
            Legacy(usize),
        }
        let mut cands: Vec<(u64, u64, Cand)> = Vec::new();
        let mut total = 0u64;
        {
            let ix = self.index.lock_or_recover();
            for (base, u) in &ix.units {
                total += u.disk_bytes();
                if u.tail_bytes == 0 && !u.convs.keys().any(|k| hot.contains(k)) {
                    cands.push((u.end_ms(), u.disk_bytes(), Cand::Unit(base.clone(), u.id)));
                }
            }
        }
        if let Some(l) = legacy {
            for (slot, len, mtime) in l.slot_files() {
                total += len;
                cands.push((mtime, len, Cand::Legacy(slot)));
            }
        }
        cands.sort_by_key(|c| c.0);
        let cutoff = match self.retention.max_days {
            0 => 0,
            d => now.saturating_sub(d * DAY_MS),
        };
        let (mut units, mut files, mut freed) = (0usize, 0usize, 0u64);
        for (at, len, cand) in cands {
            if at >= cutoff && total <= self.retention.max_total_bytes {
                break;
            }
            match cand {
                Cand::Unit(base, id) => {
                    let mut ix = self.index.lock_or_recover();
                    if ix.units.get(&base).is_none_or(|u| u.id != id || u.tail_bytes > 0) {
                        continue; // written to since the list was taken
                    }
                    ix.remove_unit(&base);
                    Self::remove_files(&base);
                    units += 1;
                }
                Cand::Legacy(slot) => {
                    if let Some(l) = legacy {
                        l.drop_slot(slot);
                    }
                    files += 1;
                }
            }
            total = total.saturating_sub(len);
            freed += len;
        }
        if units + files > 0 {
            self.cache.lock_or_recover().take();
            info!(
                "conversation log: retention removed {units} conversation file(s) and {files} legacy \
                 file(s), {freed} B; {total} B kept"
            );
        }
        // A tombstone is done once nothing that could hold its lines is left.
        let legacy_left = legacy.is_some_and(|l| !l.slot_files().is_empty());
        let mut map = self.tombs.map.lock_or_recover();
        let before = map.len();
        map.retain(|_, t| {
            t.files.retain(|f| {
                let b = self.root.join(f);
                zst_of(&b).exists() || tail_of(&b).exists()
            });
            !t.files.is_empty() || (t.legacy && legacy_left)
        });
        if map.len() != before {
            self.tombs.save(&map);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe_and_days_are_utc() {
        assert_eq!(sanitize("alice"), "alice");
        assert_eq!(sanitize("brinkley/pi"), "brinkley_pi");
        assert_eq!(sanitize("../etc"), "_etc");
        assert_eq!(sanitize(""), "_");
        assert_eq!(sanitize(&"x".repeat(200)).len(), 64);
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(1_793_727_883_508), "2026-11-03");
        assert_eq!(day_of(951_782_400_000), "2000-02-29");
        assert_eq!(base_of(Path::new("/l/s/d/a__b.jsonl.zst")), Some(PathBuf::from("/l/s/d/a__b")));
        assert_eq!(base_of(Path::new("/l/s/d/a__b.p2.meta.json")), Some(PathBuf::from("/l/s/d/a__b.p2")));
        assert_eq!(base_of(Path::new("/l/s/d/a__b.jsonl.zst.tmp")), None);
    }
}
