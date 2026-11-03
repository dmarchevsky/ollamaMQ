//! Read, migrate and check the conversation log outside the proxy.
//!
//! ```text
//! cargo run --release --example convlog_tool -- export  <log files…>  > all.jsonl
//! cargo run --release --example convlog_tool -- compact <out.jsonl> <log files…>
//! cargo run --release --example convlog_tool -- check   <log files…>
//! ```
//!
//! The proxy writes each conversation as a series of v4 lines that carry only
//! what changed, with request deltas and compressed rotated slots (see
//! `src/convlog.rs`). That keeps days of agent traffic in the space minutes used
//! to take, but a line on disk is no longer a whole conversation. These commands
//! give scripts the whole view back and move older logs forward:
//!
//! - **export** prints one line per conversation with every turn's request
//!   rebuilt whole — the shape `jq`, DuckDB or a Python script wants. Pass every
//!   file of the chain (`'/srv/logs/ollamamq-requests.jsonl'*`): a conversation's
//!   lines can span several files.
//! - **compact** rewrites a chain (any schema, e.g. v3 snapshots from before the
//!   compact format) into v4 lines, one per turn, to stand in for the live file
//!   of a stopped proxy or to archive. It writes plain JSONL; compress it with
//!   `zstd --long=27` if it is for keeps.
//! - **check** compacts in memory, reads the result back, and proves every
//!   request and reply survived: each rebuilt body must equal the original, and
//!   each reply must read the same to the audit. It prints the sizes.
//!
//! Exit status: `0` on success, `1` when `check` found a difference, `2` on bad
//! arguments or unreadable input.

use std::io::Write;
use std::path::PathBuf;

use ollamamq::audit;
use ollamamq::convlog::{self, Conv};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => usage(),
    };
    match cmd {
        "export" => export(&files(rest)),
        "compact" => {
            let Some((out, inputs)) = rest.split_first() else { usage() };
            compact(PathBuf::from(out), &files(inputs));
        }
        "check" => check(&files(rest)),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: convlog_tool export <log files…>");
    eprintln!("       convlog_tool compact <out.jsonl> <log files…>");
    eprintln!("       convlog_tool check <log files…>");
    std::process::exit(2);
}

fn files(args: &[String]) -> Vec<PathBuf> {
    if args.is_empty() {
        usage();
    }
    args.iter().map(PathBuf::from).collect()
}

fn load(files: &[PathBuf]) -> Vec<Conv> {
    match convlog::load_conversations(files) {
        Ok((convs, skipped)) => {
            if skipped > 0 {
                eprintln!("convlog_tool: {skipped} line(s) were not conversation records (skipped)");
            }
            convs
        }
        Err(e) => {
            eprintln!("convlog_tool: cannot read the log: {e}");
            std::process::exit(2);
        }
    }
}

fn export(files: &[PathBuf]) {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for conv in load(files) {
        match serde_json::to_string(&conv) {
            Ok(line) => {
                if writeln!(out, "{line}").is_err() {
                    return; // a closed pipe (`| head`) is not an error
                }
            }
            Err(e) => eprintln!("convlog_tool: {}: {e}", conv.key),
        }
    }
}

fn compact(out: PathBuf, files: &[PathBuf]) {
    let convs = load(files);
    let mut lines: Vec<(u64, String)> = Vec::new();
    for conv in &convs {
        // One line per turn, ordered by when the turn arrived — the order a
        // live writer would have appended them in.
        for (t, line) in conv.turns.iter().zip(convlog::compact_lines(conv)) {
            lines.push((t.at_ms, line));
        }
    }
    lines.sort_by_key(|(at, _)| *at);
    let mut f = match std::fs::File::create(&out) {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            eprintln!("convlog_tool: {}: {e}", out.display());
            std::process::exit(2);
        }
    };
    let mut bytes = 0usize;
    for (_, line) in &lines {
        bytes += line.len() + 1;
        if writeln!(f, "{line}").is_err() {
            eprintln!("convlog_tool: write to {} failed", out.display());
            std::process::exit(2);
        }
    }
    if f.flush().is_err() {
        std::process::exit(2);
    }
    eprintln!(
        "convlog_tool: {} conversations, {} lines, {} bytes -> {}",
        convs.len(),
        lines.len(),
        bytes,
        out.display()
    );
}

fn check(files: &[PathBuf]) {
    let input: u64 = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
    let originals = load(files);
    let mut text = String::new();
    for conv in &originals {
        for line in convlog::compact_lines(conv) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let tmp = std::env::temp_dir().join(format!("convlog_tool_check_{}.jsonl", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, &text) {
        eprintln!("convlog_tool: {}: {e}", tmp.display());
        std::process::exit(2);
    }
    let back = load(std::slice::from_ref(&tmp));
    let _ = std::fs::remove_file(&tmp);
    let zst = zstd::bulk::compress(text.as_bytes(), 9).map(|z| z.len()).unwrap_or(0);

    let mut turns = 0usize;
    let mut bad = 0usize;
    let mut folded = 0usize;
    let by_key: std::collections::HashMap<&str, &Conv> =
        back.iter().map(|c| (c.key.as_str(), c)).collect();
    for orig in &originals {
        let Some(got) = by_key.get(orig.key.as_str()) else {
            eprintln!("missing conversation {}", orig.key);
            bad += 1;
            continue;
        };
        for t in &orig.turns {
            turns += 1;
            let Some(g) = got.turns.iter().find(|x| x.rid == t.rid) else {
                eprintln!("{} rid {}: turn missing", orig.key, t.rid);
                bad += 1;
                continue;
            };
            let (oc, gc) = (t.content.as_ref(), g.content.as_ref());
            let req = |c: Option<&convlog::TurnContent>| c.and_then(|c| c.req.as_deref().cloned());
            let resp = |c: Option<&convlog::TurnContent>| c.and_then(|c| c.resp.as_deref().cloned());
            if req(oc) != req(gc) {
                eprintln!("{} rid {}: request differs", orig.key, t.rid);
                bad += 1;
            }
            match (resp(oc), resp(gc)) {
                (Some(a), Some(b)) => {
                    if a != b {
                        folded += 1;
                    }
                    if audit::reply_text(&a) != audit::reply_text(&b) {
                        eprintln!("{} rid {}: reply reads differently", orig.key, t.rid);
                        bad += 1;
                    }
                }
                (None, None) => {}
                _ => {
                    eprintln!("{} rid {}: reply missing", orig.key, t.rid);
                    bad += 1;
                }
            }
            if t.msgs != g.msgs {
                eprintln!("{} rid {}: msgs differ", orig.key, t.rid);
                bad += 1;
            }
        }
    }
    let mib = |b: u64| b as f64 / (1u64 << 20) as f64;
    println!("input files        {:>10.1} MiB", mib(input));
    println!("compact (v4)       {:>10.1} MiB", mib(text.len() as u64));
    println!("compact, zstd -9   {:>10.1} MiB", mib(zst as u64));
    println!("conversations      {:>10}", originals.len());
    println!("turns checked      {:>10}", turns);
    println!("replies folded     {:>10}", folded);
    println!("differences        {:>10}", bad);
    if bad > 0 {
        std::process::exit(1);
    }
}
