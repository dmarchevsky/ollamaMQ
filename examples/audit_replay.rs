//! Replay recorded turns through the audit's rule scanner.
//!
//! ```text
//! cargo run --example audit_replay -- /srv/logs
//! ```
//!
//! The chain is read the way the viewer reads it ([`convlog::load_conversations`]):
//! every file, compressed slots included, folded into one record per
//! conversation, each turn — one `/v1/chat/completions` the proxy served — with
//! its request body rebuilt whole and its response as stored (`content.req`,
//! `content.resp`; a streamed reply folded into one completion). This walks them, rebuilds
//! what the audit is handed — the same `request_increment` + `reply_block` +
//! `scan_turn` `judge` runs, over the same text, with the same rule that a user
//! block is judged once per conversation — and prints what the rules see now.
//! It is an example and not a test on purpose: the request log is operator data,
//! gitignored, and a test that needed it would be a test nobody can run.
//!
//! ## What this proves, and what it cannot
//!
//! The records on this host were written **before** the truncation was fixed: the
//! proxy used to cut the accumulated SSE at 128 KiB *before* parsing it, so on a
//! long reply the tail — where a streamed tool call lives — is gone from the file,
//! and nothing here can bring it back. A replay of those records therefore proves
//! the **false-positive side only**: that the rules no longer fire on a grep's
//! `|| showStderr`, on a `process.env`, or on the `docker/.env` a document
//! quotes. It cannot show a repaired streamed call being caught, because in those
//! records there is none to catch. Replay a log this build wrote to check that
//! side.
//!
//! The model layer is not replayed at all: d3-nano and the guard need the backend
//! live, and re-asking them about months of traffic would spend real NPU on the
//! past. Only the scanner — free, local, deterministic — runs over the corpus.
//!
//! Exit status: `0` when the replay ran (whatever it found), `2` when nothing was
//! named or nothing named could be read.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use ollamamq::audit::{self, Flag};
use ollamamq::convlog;
use regex::Regex;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: cargo run --example audit_replay -- <request-log> [more logs or globs…]");
        eprintln!("  e.g. cargo run --example audit_replay -- /srv/logs");
        std::process::exit(2);
    }
    let files = expand(&args);
    if files.is_empty() {
        eprintln!("audit_replay: no file matches {}", args.join(" "));
        std::process::exit(2);
    }
    let mut replay = Replay { files: files.len(), ..Default::default() };
    // A conversation's lines can sit in several rotated files, so the chain is
    // folded as a whole before any turn is judged.
    let (convs, unreadable) = match convlog::load_conversations(&files) {
        Ok(read) => read,
        Err(e) => {
            eprintln!("audit_replay: cannot read the log: {e}");
            std::process::exit(2);
        }
    };
    replay.unreadable = unreadable;
    for conv in &convs {
        match serde_json::to_value(conv) {
            Ok(record) => replay_record(&record, &mut replay),
            Err(e) => eprintln!("audit_replay: {}: {e}", conv.key),
        }
    }
    replay.print();
}

/// The files the arguments pick out. `*` and `?` are honoured in the file-name
/// part, because the log rotates to `…jsonl.0`, `…jsonl.1` and a replay wants the
/// whole set; a directory stands for every `*.jsonl*` inside it. An argument that
/// names nothing is reported rather than ignored — a replay of nothing that
/// printed zero flags would be the worst possible result.
fn expand(args: &[String]) -> Vec<PathBuf> {
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
    for arg in args {
        let path = Path::new(arg);
        if path.is_dir() {
            // A logs folder: its legacy chain and every conversation file
            // under `sessions/`, the way the proxy reads it.
            found.extend(convlog::expand_log_paths(&[path.to_path_buf()]));
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => {
                eprintln!("audit_replay: not a log path: {arg}");
                continue;
            }
        };
        if !name.contains(['*', '?']) {
            if path.is_file() {
                found.insert(path.to_path_buf());
            } else {
                eprintln!("audit_replay: no such file: {arg}");
            }
            continue;
        }
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        match dir_entries(dir) {
            Some(entries) => {
                let pattern = glob_pattern(&name);
                found.extend(entries.filter(|n| pattern.is_match(n)).map(|n| dir.join(n)));
            }
            None => eprintln!("audit_replay: cannot read directory {}", dir.display()),
        }
    }
    found.into_iter().collect()
}

fn dir_entries(dir: &Path) -> Option<impl Iterator<Item = String>> {
    let read = std::fs::read_dir(dir).ok()?;
    Some(
        read.filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_string)),
    )
}

/// `*` and `?` as a regex, everything else escaped — enough to name a set of file
/// names, and no new dependency to take a rotated log in one argument.
fn glob_pattern(name: &str) -> Regex {
    let mut re = String::from("^");
    for c in name.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    // Every special character is escaped above, so this only fails on a bug here.
    Regex::new(&re).expect("a glob translated to a regex is always valid")
}

fn replay_record(record: &serde_json::Value, out: &mut Replay) {
    out.conversations += 1;
    let key = record
        .get("key")
        .and_then(|k| k.as_str())
        .unwrap_or("?")
        .to_string();
    // `judge` judges a user block once per conversation. Its ledger lives inside
    // `Audit` and is not exported, so the replay keeps its own for this record —
    // which *is* the conversation, folded across every file it was written to.
    let mut judged: HashSet<u64> = HashSet::new();
    let turns = record.get("turns").and_then(|t| t.as_array());
    for turn in turns.iter().flat_map(|list| list.iter()) {
        out.turns += 1;
        let rid = turn.get("rid").and_then(|r| r.as_u64()).unwrap_or(0);
        let Some(content) = turn.get("content") else {
            // Nothing stored: a redacted turn, or content logging off. The rules
            // cannot be replayed on text that was never kept.
            out.unstored += 1;
            continue;
        };
        let req = content.get("req").and_then(|v| v.as_str()).unwrap_or("");
        let resp = content.get("resp").and_then(|v| v.as_str());

        let mut text = audit::request_increment(req, audit::QUESTION_CHARS * 4);
        let mut fresh: Vec<String> = Vec::new();
        for block in text.user.drain(..) {
            if judged.insert(digest(&block)) {
                fresh.push(block);
            }
        }
        text.user = fresh;
        text.reply = resp.and_then(audit::reply_block);
        if text.is_empty() {
            out.empty += 1;
            continue;
        }
        out.judged += 1;
        let flags = audit::scan_turn(&text.scans(), &[]);
        if flags.is_empty() {
            continue;
        }
        out.flagged_turns += 1;
        println!("{key} rid {rid}");
        for flag in &flags {
            print_flag(flag);
            out.tally(flag);
        }
    }
}

fn print_flag(flag: &Flag) {
    let sev = if flag.bad { "confirmed" } else { "watch" };
    let prob = flag.prob.map(|p| format!(" p{p:.2}")).unwrap_or_default();
    let detail = flag
        .detail
        .as_deref()
        .map(|d| format!(" · {d}"))
        .unwrap_or_default();
    let category = flag.category.as_deref().unwrap_or("");
    println!(
        "  {:<6} {:<5} {:<24} {sev}{prob} {category}{detail}",
        flag.kind.as_str(),
        flag.src.as_str(),
        flag.by,
    );
}

/// The same thing the audit remembers per conversation: a keyed digest, so the
/// replay holds no text of its own either.
fn digest(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// What the run saw, kept as counts: verdicts are printed, traffic never is.
#[derive(Default)]
struct Replay {
    files: usize,
    conversations: usize,
    turns: usize,
    /// Turns with no stored text: redacted, or content logging off.
    unstored: usize,
    /// Turns whose stored text held nothing new to judge.
    empty: usize,
    judged: usize,
    flagged_turns: usize,
    unreadable: usize,
    by_rule: BTreeMap<String, usize>,
    by_kind: BTreeMap<String, usize>,
    confirmed: usize,
    watches: usize,
}

impl Replay {
    fn tally(&mut self, flag: &Flag) {
        *self.by_rule.entry(flag.by.clone()).or_insert(0) += 1;
        *self.by_kind.entry(flag.kind.as_str().to_string()).or_insert(0) += 1;
        if flag.bad {
            self.confirmed += 1;
        } else {
            self.watches += 1;
        }
    }

    fn print(&self) {
        println!();
        println!("=== audit replay ===");
        println!("files             {}", self.files);
        println!("conversations     {}", self.conversations);
        println!("turns             {}", self.turns);
        println!("  judged          {}", self.judged);
        println!("  nothing stored  {} (redacted, or content logging off)", self.unstored);
        println!("  nothing new     {} (already judged in this conversation)", self.empty);
        println!("unreadable lines  {}", self.unreadable);
        println!("turns flagged     {}", self.flagged_turns);
        println!("flags             {} confirmed · {} watch", self.confirmed, self.watches);
        if self.by_kind.is_empty() {
            println!("kinds             none");
        } else {
            let kinds: Vec<String> = self
                .by_kind
                .iter()
                .map(|(k, n)| format!("{k} {n}"))
                .collect();
            println!("kinds             {}", kinds.join(" · "));
        }
        if self.by_rule.is_empty() {
            println!("no rule fired.");
            return;
        }
        println!("by detector:");
        for (rule, count) in &self.by_rule {
            println!("  {count:>4}  {rule}");
        }
    }
}
