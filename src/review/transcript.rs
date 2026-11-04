//! A conversation as plain text for a reviewing model.
//!
//! The log keeps every request whole, and an agent's requests re-send the
//! whole history each turn, so printing them as stored would hand the model the
//! same messages dozens of times. Each turn here shows only what it added to
//! the turn before (the viewer's incremental mode) and then the reply. A turn
//! that starts over (a compaction, a sub-agent sharing the session) prints its
//! new context in full and says so.
//!
//! The text is bounded twice: each message is cut to `max_message` (head and
//! tail kept), and the whole transcript to `max_total` by dropping turns from
//! the middle, because a review is most about how a conversation started and
//! how it ended.

use serde_json::Value;

use crate::convlog::{Conv, Turn};

/// What a review is handed.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// Turns rendered (before any were dropped from the middle).
    pub turns: usize,
    /// Something was cut: a message, or whole turns.
    pub truncated: bool,
}

/// Render `conv`'s turns up to and including `rid_to`.
pub fn build(conv: &Conv, rid_to: u64, max_total: usize, max_message: usize) -> Transcript {
    let mut blocks: Vec<String> = Vec::new();
    let mut cut = false;
    let mut prev: Vec<String> = Vec::new();
    let mut prev_tools: Vec<String> = Vec::new();
    let mut last_reply: Option<String> = None;
    let start = conv.turns.first().map_or(0, |t| t.at_ms);
    let turns: Vec<&Turn> = conv.turns.iter().filter(|t| t.rid <= rid_to).collect();
    for (i, turn) in turns.iter().enumerate() {
        let mut out = turn_header(i + 1, turn, start);
        let content = turn.content.as_ref();
        let req = content.and_then(|c| c.req.as_deref());
        let resp = content.and_then(|c| c.resp.as_deref());
        if req.is_none() && resp.is_none() {
            out.push_str(if turn.redacted {
                "[this turn's text was not recorded (redacted)]\n"
            } else {
                "[this turn's text was not captured]\n"
            });
            blocks.push(out);
            continue;
        }
        if let Some(req) = req {
            let body: Value = serde_json::from_str(req).unwrap_or(Value::Null);
            let tools = tool_names(&body);
            if !tools.is_empty() && tools != prev_tools {
                out.push_str(&format!("Tools available: {}\n\n", cap(&tools.join(", "), max_message, &mut cut)));
                prev_tools = tools;
            }
            let msgs = request_messages(&body, req);
            let keys: Vec<String> = msgs.iter().map(msg_key).collect();
            let common = keys.iter().zip(prev.iter()).take_while(|(a, b)| a == b).count();
            if i > 0 && common == 0 && !prev.is_empty() {
                out.push_str("(the context starts over in this turn)\n\n");
            }
            let mut fresh = &msgs[common.min(msgs.len())..];
            // The reply printed under the previous turn comes back as the first
            // new message of this one: do not print it twice.
            if let (Some(first), Some(reply)) = (fresh.first(), last_reply.as_deref())
                && role_of(first) == "assistant"
                && text_of(first.get("content")).trim() == reply.trim()
            {
                fresh = &fresh[1..];
            }
            for m in fresh {
                out.push_str(&render_message(m, max_message, &mut cut));
            }
            prev = keys;
        }
        match resp {
            Some(resp) => {
                let reply = parse_reply(resp);
                out.push_str(&render_reply(&reply, max_message, &mut cut));
                last_reply = Some(reply.content);
            }
            None => {
                out.push_str(&format!(
                    "[no reply recorded{}]\n",
                    turn.outcome.as_deref().map(|o| format!(": {o}")).unwrap_or_default()
                ));
                last_reply = None;
            }
        }
        blocks.push(out);
    }
    let n = blocks.len();
    let mut text = fit(blocks, max_total, &mut cut);
    if conv.dropped_turns > 0 {
        text.insert_str(
            0,
            &format!("[{} early turns were dropped by the log before this review]\n\n", conv.dropped_turns),
        );
        cut = true;
    }
    Transcript { text, turns: n, truncated: cut }
}

fn turn_header(n: usize, turn: &Turn, start: u64) -> String {
    let secs = turn.at_ms.saturating_sub(start) / 1000;
    let mut h = format!("=== Turn {n} · +{}:{:02}", secs / 60, secs % 60);
    if let Some(m) = &turn.model {
        h.push_str(&format!(" · {m}"));
    }
    if let Some(k) = &turn.kind {
        h.push_str(&format!(" · {k}"));
    }
    match turn.outcome.as_deref() {
        Some("ok") | None => {}
        Some(o) => h.push_str(&format!(" · {o}")),
    }
    h.push_str(" ===\n");
    h
}

/// The messages of a request body: `messages` (OpenAI, Ollama, with a top-level
/// `system` folded in front), else a completion `prompt`, else the raw text.
fn request_messages(body: &Value, raw: &str) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(sys) = body.get("system").filter(|s| !s.is_null()) {
        out.push(serde_json::json!({ "role": "system", "content": sys }));
    }
    if let Some(list) = body.get("messages").and_then(Value::as_array) {
        out.extend(list.iter().cloned());
    } else if let Some(p) = body.get("prompt").filter(|p| !p.is_null()) {
        out.push(serde_json::json!({ "role": "user", "content": p }));
    } else if body.is_null() {
        out.push(serde_json::json!({ "role": "request", "content": raw }));
    }
    out
}

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|t| {
                    t.pointer("/function/name")
                        .or_else(|| t.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn role_of(m: &Value) -> &str {
    m.get("role").and_then(Value::as_str).unwrap_or("message")
}

/// What makes two messages "the same" across turns: role, text and the tools
/// called. Clients re-send history with small marks of their own (cache hints,
/// ids), which must not make every turn look new.
fn msg_key(m: &Value) -> String {
    let calls: Vec<String> = calls_of(m).into_iter().map(|(n, _)| n).collect();
    format!("{}\u{1}{}\u{1}{}", role_of(m), text_of(m.get("content")), calls.join(","))
}

/// Text of a `content` value: a string, or the text parts of a list.
fn text_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| match p {
                Value::String(s) => s.clone(),
                _ => match p.get("type").and_then(Value::as_str) {
                    Some("image_url" | "image" | "input_image") => "[image]".to_string(),
                    Some("tool_result") => text_of(p.get("content")),
                    Some("tool_use") => format!(
                        "[call {}]",
                        p.get("name").and_then(Value::as_str).unwrap_or("?")
                    ),
                    _ => p
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_default(),
                },
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
    }
}

/// Tool calls a message makes, as (name, arguments).
fn calls_of(m: &Value) -> Vec<(String, String)> {
    m.get("tool_calls")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|c| {
                    let f = c.get("function").unwrap_or(c);
                    let name = f.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
                    let args = match f.get("arguments") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Null) | None => String::new(),
                        Some(v) => v.to_string(),
                    };
                    (name, args)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn render_message(m: &Value, max: usize, cut: &mut bool) -> String {
    let role = role_of(m);
    let mut head = format!("### {role}");
    if let Some(name) = m.get("name").or_else(|| m.get("tool_name")).and_then(Value::as_str) {
        head.push_str(&format!(" ({name})"));
    }
    let mut out = format!("{head}\n");
    let text = text_of(m.get("content"));
    if !text.trim().is_empty() {
        out.push_str(&cap(text.trim(), max, cut));
        out.push('\n');
    }
    for (name, args) in calls_of(m) {
        out.push_str(&format!("→ call {name}({})\n", cap(&args, max / 4, cut)));
    }
    out.push('\n');
    out
}

/// A reply reduced to what a reviewer reads.
#[derive(Debug, Default, PartialEq)]
struct Reply {
    content: String,
    reasoning: String,
    calls: Vec<(String, String)>,
    error: Option<String>,
}

/// Read a stored reply: an OpenAI completion (streams are folded into one by
/// the log), an Ollama `/api/chat` or `/api/generate` answer, or the NDJSON
/// lines of an Ollama stream. Anything else is passed on as text.
fn parse_reply(text: &str) -> Reply {
    let mut r = Reply::default();
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        absorb(&mut r, &v);
        return r;
    }
    let mut any = false;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let line = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            absorb(&mut r, &v);
            any = true;
        }
    }
    if !any {
        r.content = text.to_string();
    }
    r
}

fn absorb(r: &mut Reply, v: &Value) {
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        r.error = Some(match e {
            Value::String(s) => s.clone(),
            other => other.get("message").and_then(Value::as_str).map_or_else(|| other.to_string(), str::to_string),
        });
        return;
    }
    let msg = v
        .pointer("/choices/0/message")
        .or_else(|| v.pointer("/choices/0/delta"))
        .or_else(|| v.get("message"));
    if let Some(m) = msg {
        r.content.push_str(&text_of(m.get("content")));
        for key in ["reasoning_content", "reasoning", "thinking"] {
            if let Some(s) = m.get(key).and_then(Value::as_str) {
                r.reasoning.push_str(s);
            }
        }
        r.calls.extend(calls_of(m));
    } else if let Some(s) = v.pointer("/choices/0/text").or_else(|| v.get("response")).and_then(Value::as_str) {
        r.content.push_str(s);
    }
}

fn render_reply(r: &Reply, max: usize, cut: &mut bool) -> String {
    if let Some(e) = &r.error {
        return format!("### assistant\n[error: {}]\n\n", cap(e, max, cut));
    }
    let mut out = String::from("### assistant\n");
    if !r.reasoning.trim().is_empty() {
        out.push_str(&format!("(reasoning) {}\n", cap(r.reasoning.trim(), max / 2, cut)));
    }
    if !r.content.trim().is_empty() {
        out.push_str(&cap(r.content.trim(), max, cut));
        out.push('\n');
    }
    for (name, args) in &r.calls {
        out.push_str(&format!("→ call {name}({})\n", cap(args, max / 4, cut)));
    }
    out.push('\n');
    out
}

/// `s` cut to at most `max` characters: two thirds from the head, one from the
/// tail, and a marker saying how much went.
fn cap(s: &str, max: usize, cut: &mut bool) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    *cut = true;
    let head = max * 2 / 3;
    let tail = max - head;
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(n - tail).collect();
    format!("{h}\n[… {} characters cut …]\n{t}", n - head - tail)
}

/// Join the turn blocks, dropping turns from the middle until the text fits
/// `max`: a third of the budget for the opening turns, the rest for the end.
fn fit(blocks: Vec<String>, max: usize, cut: &mut bool) -> String {
    let len = |s: &String| s.chars().count();
    let total: usize = blocks.iter().map(len).sum();
    if total <= max {
        return blocks.concat();
    }
    *cut = true;
    let mut head_n = 0;
    let mut head_len = 0;
    for b in &blocks {
        if head_len + len(b) > max / 3 {
            break;
        }
        head_len += len(b);
        head_n += 1;
    }
    let mut tail_n = 0;
    let mut tail_len = 0;
    for b in blocks[head_n..].iter().rev() {
        if head_len + tail_len + len(b) > max {
            break;
        }
        tail_len += len(b);
        tail_n += 1;
    }
    let mut out = blocks[..head_n].concat();
    let omitted = blocks.len() - head_n - tail_n;
    if tail_n == 0 {
        // Not even the last turn fits whole: keep its end.
        let last = blocks.last().map(String::as_str).unwrap_or("");
        out.push_str(&format!("[… {} turns omitted to fit the review budget …]\n\n", omitted - 1));
        let room = max.saturating_sub(head_len).max(200);
        let mut c = false;
        out.push_str(&cap(last, room, &mut c));
        return out;
    }
    if omitted > 0 {
        out.push_str(&format!("[… {omitted} turns omitted to fit the review budget …]\n\n"));
    }
    out.push_str(&blocks[blocks.len() - tail_n..].concat());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convlog::TurnContent;
    use std::sync::Arc;

    fn turn(rid: u64, req: Option<&str>, resp: Option<&str>) -> Turn {
        serde_json::from_value::<Turn>(serde_json::json!({
            "rid": rid, "at_ms": 1_000 + rid * 1000, "path": "/v1/chat/completions",
            "model": "m", "outcome": "ok",
        }))
        .map(|mut t| {
            t.content = Some(TurnContent {
                req: req.map(|s| Arc::new(s.to_string())),
                req_d: None,
                resp: resp.map(|s| Arc::new(s.to_string())),
            });
            t
        })
        .unwrap()
    }

    fn conv(turns: Vec<Turn>) -> Conv {
        serde_json::from_value::<Conv>(serde_json::json!({
            "v": 4, "key": "u#abc123", "user": "u", "start_ms": 0, "end_ms": 0,
        }))
        .map(|mut c| {
            c.turns = turns;
            c
        })
        .unwrap()
    }

    fn reply(text: &str) -> String {
        serde_json::json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
    }

    fn req(msgs: &[(&str, &str)]) -> String {
        let m: Vec<Value> = msgs.iter().map(|(r, c)| serde_json::json!({ "role": r, "content": c })).collect();
        serde_json::json!({ "model": "m", "messages": m }).to_string()
    }

    #[test]
    fn each_turn_shows_only_what_it_added() {
        let t1 = req(&[("system", "be brief"), ("user", "hello")]);
        let t2 = req(&[("system", "be brief"), ("user", "hello"), ("assistant", "hi there"), ("user", "bye")]);
        let c = conv(vec![turn(1, Some(&t1), Some(&reply("hi there"))), turn(2, Some(&t2), Some(&reply("ciao")))]);
        let t = build(&c, 99, 100_000, 4000);
        assert_eq!(t.turns, 2);
        assert!(!t.truncated);
        assert_eq!(t.text.matches("be brief").count(), 1, "{}", t.text);
        assert_eq!(t.text.matches("hello").count(), 1);
        // The reply is printed once, under turn 1, not again as turn 2's history.
        assert_eq!(t.text.matches("hi there").count(), 1, "{}", t.text);
        assert!(t.text.contains("bye") && t.text.contains("ciao"));
        assert!(t.text.contains("=== Turn 2"));
    }

    #[test]
    fn rid_to_fixes_the_slice() {
        let c = conv(vec![
            turn(1, Some(&req(&[("user", "one")])), Some(&reply("a"))),
            turn(2, Some(&req(&[("user", "one"), ("assistant", "a"), ("user", "two")])), Some(&reply("b"))),
        ]);
        let t = build(&c, 1, 100_000, 4000);
        assert_eq!(t.turns, 1);
        assert!(!t.text.contains("two"));
    }

    #[test]
    fn redacted_turns_and_restarts_are_said() {
        let mut r = turn(2, None, None);
        r.content = None;
        r.redacted = true;
        let c = conv(vec![
            turn(1, Some(&req(&[("user", "one")])), Some(&reply("a"))),
            r,
            turn(3, Some(&req(&[("user", "summary of it all")])), Some(&reply("ok"))),
        ]);
        let t = build(&c, 99, 100_000, 4000);
        assert!(t.text.contains("not recorded (redacted)"));
        assert!(t.text.contains("context starts over"));
    }

    #[test]
    fn long_messages_and_long_conversations_are_cut() {
        let big = "x".repeat(10_000);
        let c = conv(vec![turn(1, Some(&req(&[("user", &big)])), Some(&reply("ok")))]);
        let t = build(&c, 99, 100_000, 1000);
        assert!(t.truncated);
        assert!(t.text.contains("characters cut"));
        assert!(t.text.chars().count() < 1500);

        let mut turns = Vec::new();
        let mut hist: Vec<(String, String)> = Vec::new();
        for i in 1..=50u64 {
            hist.push(("user".into(), format!("question {i} {}", "y".repeat(500))));
            let msgs: Vec<(&str, &str)> = hist.iter().map(|(r, c)| (r.as_str(), c.as_str())).collect();
            turns.push(turn(i, Some(&req(&msgs)), Some(&reply(&format!("answer {i}")))));
            hist.push(("assistant".into(), format!("answer {i}")));
        }
        let t = build(&conv(turns), 999, 6000, 4000);
        assert!(t.truncated);
        assert!(t.text.contains("turns omitted"));
        assert!(t.text.contains("question 1 ") && t.text.contains("answer 50"));
        assert!(t.text.chars().count() <= 6100, "{}", t.text.chars().count());
    }

    #[test]
    fn replies_of_every_shape_are_read() {
        let ollama = r#"{"model":"m","message":{"role":"assistant","content":"from ollama","tool_calls":[{"function":{"name":"read","arguments":{"path":"a"}}}]},"done":true}"#;
        let r = parse_reply(ollama);
        assert_eq!(r.content, "from ollama");
        assert_eq!(r.calls, vec![("read".to_string(), r#"{"path":"a"}"#.to_string())]);
        let ndjson = "{\"message\":{\"content\":\"a\"}}\n{\"message\":{\"content\":\"b\"},\"done\":true}\n";
        assert_eq!(parse_reply(ndjson).content, "ab");
        assert_eq!(parse_reply(r#"{"response":"gen"}"#).content, "gen");
        assert_eq!(parse_reply(r#"{"error":{"message":"boom"}}"#).error.as_deref(), Some("boom"));
        assert_eq!(parse_reply("plain words").content, "plain words");
    }
}
