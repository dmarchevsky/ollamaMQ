//! How a conversation's turns are kept small on disk (schema v4).
//!
//! An agent resends its whole history on every turn, and a streamed reply
//! arrives as hundreds of `data: {…}` chunks that each repeat the same
//! envelope. Written as captured, a 64-turn session was a 20 MB line holding
//! well under 1 MB of distinct text. This module holds the three pieces that
//! close that gap, none of which touch the request path:
//!
//! - [`split_body`] / [`render_body`]: a chat request as its message list plus
//!   the rest of the object, so a turn can store only the messages it added to
//!   an earlier turn's list ([`ReqDelta`]).
//! - [`assemble_sse`]: an OpenAI-style stream folded into the one
//!   `chat.completion` object it amounts to.
//! - zstd helpers for the rotated slots of the chain.
//!
//! Messages are kept as their **raw JSON text** (`RawValue`), never re-encoded,
//! so a rebuilt body carries every message byte for byte; only the separators
//! of the top-level object are normalized.

use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

/// Where a turn's request body differs from an earlier turn's in the same
/// slot: the first `keep` messages of turn `base`'s list, then `add`, inside
/// `tmpl` (or `base`'s template when absent — the tools and sampling options
/// rarely change mid-session).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReqDelta {
    pub base: u64,
    pub keep: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<Box<RawValue>>,
    /// The request object with `"messages":null` standing in for the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmpl: Option<Box<RawValue>>,
}

/// A chat request split at its `messages` key.
pub struct SplitBody {
    /// The request object in its original key order, `"messages":null` in
    /// place of the list.
    pub tmpl: String,
    /// Each message's raw JSON text.
    pub msgs: Vec<Arc<str>>,
}

/// Top-level members of a JSON object in their original order, values left
/// unparsed.
struct Members<'a>(Vec<(String, &'a RawValue)>);

impl<'de> Deserialize<'de> for Members<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Members<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, &'de RawValue>()? {
                    out.push((k, v));
                }
                Ok(Members(out))
            }
        }
        d.deserialize_map(V)
    }
}

/// Split a chat request body, `None` when it is not a JSON object with a
/// `messages` array (a truncated capture, an embeddings call, …) — such a body
/// is simply stored whole.
pub fn split_body(body: &str) -> Option<SplitBody> {
    let members: Members = serde_json::from_str(body).ok()?;
    let mut tmpl = String::with_capacity(256);
    let mut msgs = None;
    tmpl.push('{');
    for (i, (k, v)) in members.0.iter().enumerate() {
        if i > 0 {
            tmpl.push(',');
        }
        tmpl.push_str(&serde_json::to_string(k).ok()?);
        tmpl.push(':');
        if k == "messages" && msgs.is_none() {
            let list: Vec<&RawValue> = serde_json::from_str(v.get()).ok()?;
            msgs = Some(list.into_iter().map(|m| Arc::from(m.get())).collect());
            tmpl.push_str("null");
        } else {
            tmpl.push_str(v.get());
        }
    }
    tmpl.push('}');
    Some(SplitBody { tmpl, msgs: msgs? })
}

/// The body [`split_body`] took apart, put back together around `msgs`.
pub fn render_body(tmpl: &str, msgs: &[Arc<str>]) -> Option<String> {
    let members: Members = serde_json::from_str(tmpl).ok()?;
    let size = tmpl.len() + msgs.iter().map(|m| m.len() + 1).sum::<usize>();
    let mut out = String::with_capacity(size + 2);
    out.push('{');
    for (i, (k, v)) in members.0.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(k).ok()?);
        out.push(':');
        if k == "messages" {
            out.push('[');
            for (j, m) in msgs.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                out.push_str(m);
            }
            out.push(']');
        } else {
            out.push_str(v.get());
        }
    }
    out.push('}');
    Some(out)
}

/// A 128-bit fingerprint of one message's text: what the writer compares
/// prefixes by, so it never holds a second copy of a session's history.
pub fn fingerprint(text: &str) -> (u64, u64) {
    let mut a = std::collections::hash_map::DefaultHasher::new();
    0u8.hash(&mut a);
    text.hash(&mut a);
    let mut b = std::collections::hash_map::DefaultHasher::new();
    1u8.hash(&mut b);
    text.hash(&mut b);
    (a.finish(), b.finish())
}

/// Length of the common prefix of two lists.
pub fn common_prefix<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// One tool call as its deltas build it up.
#[derive(Default)]
struct ToolAcc {
    id: Option<Value>,
    kind: Option<Value>,
    name: String,
    args: String,
}

/// Fold an OpenAI-style SSE stream into the `chat.completion` object it
/// amounts to: content, reasoning and tool calls concatenated, finish reason
/// and usage from the chunks that carried them, any other top-level field
/// (llama.cpp's `timings`) from the last chunk that had it. `sse_chunks` says
/// how many chunks were folded.
///
/// Anything this cannot fold without losing information returns `None` and
/// the stream is kept as captured: a chunk that does not parse (a capture cut
/// at the content limit), an error payload, a second choice, or a delta field
/// it does not know.
pub fn assemble_sse(text: &str) -> Option<String> {
    let head = text.trim_start();
    if !(head.starts_with("data:") || head.starts_with(':') || head.starts_with("event:")) {
        return None;
    }
    let mut chunks = 0usize;
    let mut role: Option<Value> = None;
    let mut content = String::new();
    let mut saw_content = false;
    // Some servers send both spellings; each is kept as it came.
    let mut reasoning = [String::new(), String::new()];
    let mut tools: Vec<ToolAcc> = Vec::new();
    let mut finish: Option<Value> = None;
    let mut usage: Option<Value> = None;
    let mut top = serde_json::Map::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(payload) = line.strip_prefix("data:") else {
            continue; // keep-alives, `event:` lines
        };
        let payload = payload.trim();
        if payload == "[DONE]" || payload.is_empty() {
            continue;
        }
        let Value::Object(chunk) = serde_json::from_str::<Value>(payload).ok()? else {
            return None;
        };
        if chunk.contains_key("error") {
            return None;
        }
        chunks += 1;
        for (k, v) in &chunk {
            match k.as_str() {
                "choices" | "object" => {}
                "usage" => {
                    if !v.is_null() {
                        usage = Some(v.clone());
                    }
                }
                _ if v.is_null() => {}
                _ => {
                    top.insert(k.clone(), v.clone());
                }
            }
        }
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            continue;
        };
        for ch in choices {
            if ch.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                return None;
            }
            if let Some(fr) = ch.get("finish_reason").filter(|v| !v.is_null()) {
                finish = Some(fr.clone());
            }
            let Some(Value::Object(delta)) = ch.get("delta") else {
                continue;
            };
            for (k, v) in delta {
                if v.is_null() {
                    continue;
                }
                match (k.as_str(), v) {
                    ("role", _) => role = Some(v.clone()),
                    ("content", Value::String(s)) => {
                        saw_content = true;
                        content.push_str(s);
                    }
                    ("reasoning_content", Value::String(s)) => reasoning[0].push_str(s),
                    ("reasoning", Value::String(s)) => reasoning[1].push_str(s),
                    ("tool_calls", Value::Array(list)) => {
                        for (pos, tc) in list.iter().enumerate() {
                            let i = tc.get("index").and_then(Value::as_u64).map_or(pos, |i| i as usize);
                            if i > 1024 {
                                return None;
                            }
                            while tools.len() <= i {
                                tools.push(ToolAcc::default());
                            }
                            let acc = &mut tools[i];
                            if let Some(id) = tc.get("id").filter(|v| !v.is_null()) {
                                acc.id = Some(id.clone());
                            }
                            if let Some(t) = tc.get("type").filter(|v| !v.is_null()) {
                                acc.kind = Some(t.clone());
                            }
                            if let Some(f) = tc.get("function") {
                                if let Some(n) = f.get("name").and_then(Value::as_str) {
                                    acc.name.push_str(n);
                                }
                                match f.get("arguments") {
                                    Some(Value::String(a)) => acc.args.push_str(a),
                                    None | Some(Value::Null) => {}
                                    Some(_) => return None,
                                }
                            }
                        }
                    }
                    // `function_call`, `refusal`, audio, …: kept as captured.
                    _ => return None,
                }
            }
        }
    }
    if chunks == 0 {
        return None;
    }
    let mut message = serde_json::Map::new();
    message.insert("role".into(), role.unwrap_or_else(|| Value::String("assistant".into())));
    message.insert(
        "content".into(),
        if saw_content || tools.is_empty() { Value::String(content) } else { Value::Null },
    );
    for (key, text) in ["reasoning_content", "reasoning"].into_iter().zip(reasoning) {
        if !text.is_empty() {
            message.insert(key.into(), Value::String(text));
        }
    }
    if !tools.is_empty() {
        let calls = tools
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let mut o = serde_json::Map::new();
                o.insert("index".into(), Value::from(i));
                if let Some(id) = t.id {
                    o.insert("id".into(), id);
                }
                o.insert("type".into(), t.kind.unwrap_or_else(|| Value::String("function".into())));
                o.insert(
                    "function".into(),
                    serde_json::json!({ "name": t.name, "arguments": t.args }),
                );
                Value::Object(o)
            })
            .collect();
        message.insert("tool_calls".into(), Value::Array(calls));
    }
    let mut out = serde_json::Map::new();
    if let Some(id) = top.remove("id") {
        out.insert("id".into(), id);
    }
    out.insert("object".into(), Value::String("chat.completion".into()));
    for k in ["created", "model"] {
        if let Some(v) = top.remove(k) {
            out.insert(k.into(), v);
        }
    }
    out.insert(
        "choices".into(),
        serde_json::json!([{ "index": 0, "message": message, "finish_reason": finish }]),
    );
    if let Some(u) = usage {
        out.insert("usage".into(), u);
    }
    out.extend(top);
    out.insert("sse_chunks".into(), Value::from(chunks));
    serde_json::to_string(&Value::Object(out)).ok()
}

// ------------------------------------------------------------------ zstd

/// Suffix of a compressed slot: `requests.jsonl.3.zst` holds what was
/// `requests.jsonl.3`.
pub const ZST: &str = ".zst";

/// zstd level for rotated slots: written once, in the background, read rarely.
const LEVEL: i32 = 9;

/// Compress `src` into `dst` (through a temporary file renamed into place, so
/// a reader sees either nothing or the whole frame), then remove `src`.
pub fn compress_slot(src: &Path, dst: &Path) -> std::io::Result<()> {
    let data = std::fs::read(src)?;
    let tmp = dst.with_extension("zst.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut enc = zstd::stream::Encoder::new(file, LEVEL)?;
        enc.set_pledged_src_size(Some(data.len() as u64))?;
        enc.include_contentsize(true)?;
        enc.long_distance_matching(true)?;
        // A window that covers the whole slot (capped at the 128 MiB every
        // stock decoder accepts without flags), so `zstdcat` reads it as is.
        let log = (64 - (data.len().max(1 << 20) as u64 - 1).leading_zeros()).clamp(20, 27);
        enc.window_log(log)?;
        enc.write_all(&data)?;
        enc.finish()?.sync_all()?;
    }
    std::fs::rename(&tmp, dst)?;
    std::fs::remove_file(src)
}

/// The whole of a slot file, decompressing a `.zst` one.
pub fn read_slot_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let is_zst = path.to_str().is_some_and(|p| p.ends_with(ZST));
    let file = std::fs::File::open(path)?;
    if !is_zst {
        let mut out = Vec::new();
        std::io::BufReader::new(file).read_to_end(&mut out)?;
        return Ok(out);
    }
    let mut dec = zstd::stream::Decoder::new(file)?;
    dec.window_log_max(27)?;
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}

/// Uncompressed size a `.zst` slot declares in its frame header.
pub fn zst_content_size(path: &Path) -> Option<u64> {
    let mut head = [0u8; 18];
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut head).ok()?;
    zstd::zstd_safe::get_frame_content_size(&head[..n]).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_render_round_trip_byte_for_byte() {
        let body = r#"{"model":"q","messages":[{"role":"system","content":"be \"brief\""},{"role":"user","content":[{"type":"text","text":"hé"}]}],"stream":true,"tools":[{"type":"function"}]}"#;
        let s = split_body(body).expect("a chat body");
        assert_eq!(s.msgs.len(), 2);
        assert_eq!(&*s.msgs[0], r#"{"role":"system","content":"be \"brief\""}"#);
        assert_eq!(s.tmpl, r#"{"model":"q","messages":null,"stream":true,"tools":[{"type":"function"}]}"#);
        assert_eq!(render_body(&s.tmpl, &s.msgs).as_deref(), Some(body));
        // Not a chat body, and a cut one: stored whole.
        assert!(split_body(r#"{"input":"x"}"#).is_none());
        assert!(split_body(r#"{"messages":[{"role":"u"#).is_none());
        assert!(split_body("data: x").is_none());
    }

    #[test]
    fn sse_folds_into_one_completion() {
        let sse = concat!(
            ": keepalive\n\n",
            "data: {\"id\":\"c1\",\"created\":5,\"model\":\"m\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}],\"usage\":null}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\"\"}}]}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"ls\\\"}\"}}]}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4},\"timings\":{\"predicted_n\":4}}\n\n",
            "data: [DONE]\n\n",
        );
        let out: Value = serde_json::from_str(&assemble_sse(sse).expect("folds")).unwrap();
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["id"], "c1");
        assert_eq!(out["model"], "m");
        let m = &out["choices"][0]["message"];
        assert_eq!(m["content"], "Hello");
        assert_eq!(m["reasoning_content"], "think");
        assert_eq!(m["tool_calls"][0]["id"], "t1");
        assert_eq!(m["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(m["tool_calls"][0]["function"]["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(out["usage"]["completion_tokens"], 4);
        assert_eq!(out["timings"]["predicted_n"], 4);
        assert_eq!(out["sse_chunks"], 7);
        // What the audit reads from the stored reply is unchanged by folding.
        assert_eq!(crate::audit::reply_text(sse), crate::audit::reply_text(&out.to_string()));
    }

    #[test]
    fn sse_that_cannot_fold_losslessly_is_kept() {
        // Cut at the content limit mid-chunk.
        assert!(assemble_sse("data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: {\"cho").is_none());
        // An error payload.
        assert!(assemble_sse("data: {\"error\":{\"message\":\"boom\"}}\n\n").is_none());
        // A second choice.
        assert!(assemble_sse("data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"a\"}}]}\n\n").is_none());
        // Not a stream at all.
        assert!(assemble_sse("{\"choices\":[]}").is_none());
    }

    #[test]
    fn compressed_slot_reads_back_and_declares_its_size() {
        let dir = std::env::temp_dir().join(format!("omq-codec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("log.jsonl.0");
        let dst = dir.join("log.jsonl.0.zst");
        let data = "{\"v\":4}\n".repeat(5000);
        std::fs::write(&src, &data).unwrap();
        compress_slot(&src, &dst).unwrap();
        assert!(!src.exists(), "the plain slot is replaced");
        assert!(std::fs::metadata(&dst).unwrap().len() < data.len() as u64 / 10);
        assert_eq!(read_slot_file(&dst).unwrap(), data.as_bytes());
        assert_eq!(zst_content_size(&dst), Some(data.len() as u64));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
