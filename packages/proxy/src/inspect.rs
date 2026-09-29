//! Local cut inspector: `GET /` on the proxy port lists recent conversations,
//! `GET /c/{conv_id}` shows, message by message, what the client sent versus
//! what parsec forwarded upstream — i.e. exactly what curation cut.
//!
//! Off the determinism boundary by construction: recording only retains the
//! refcounted inbound/outbound `Bytes` the serving path already built (no
//! copy, no parse), and nothing here feeds back into served bytes. All
//! parsing and diffing happens lazily when a page is viewed. Memory only —
//! nothing is written to disk or sent anywhere, and the pages refuse any
//! `Host` that is not loopback (DNS-rebinding guard).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde_json::Value;

use crate::server::AppState;

/// Conversations retained when `PARSEC_INSPECT` is unset.
const DEFAULT_CONVS: usize = 20;
/// Unchanged blocks longer than this are previewed, not rendered in full.
const PREVIEW_CHARS: usize = 4000;

/// Latest request of one conversation: the bytes as received and as sent.
struct Snapshot {
    conv_id: String,
    seq: u64,
    model: String,
    fail_open: bool,
    freeze_cut_tokens: i64,
    tools_total: Option<usize>,
    tools_kept: Option<usize>,
    inbound: Bytes,
    outbound: Bytes,
}

pub struct Inspector {
    cap: usize,
    seq: AtomicU64,
    /// Most recent first; at most one entry per conversation.
    recent: Mutex<VecDeque<Arc<Snapshot>>>,
}

/// Summary fields copied out of the request's plan stats.
pub(crate) struct Record<'a> {
    pub conv_id: &'a str,
    pub model: &'a str,
    pub fail_open: bool,
    pub freeze_cut_tokens: i64,
    pub tools_total: Option<usize>,
    pub tools_kept: Option<usize>,
}

impl Inspector {
    /// `PARSEC_INSPECT` — how many recent conversations the local cut
    /// inspector (`http://127.0.0.1:<port>/`) keeps in memory, latest request
    /// each. Unset/junk ⇒ 20; `0` disables recording (the pages then show an
    /// empty list).
    pub fn from_env() -> Self {
        let cap = std::env::var("PARSEC_INSPECT")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_CONVS);
        Self::with_cap(cap)
    }

    pub fn with_cap(cap: usize) -> Self {
        Self {
            cap,
            seq: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn record(&self, r: Record<'_>, inbound: &Bytes, outbound: &Bytes) {
        if self.cap == 0 || r.conv_id.is_empty() {
            return;
        }
        let snap = Arc::new(Snapshot {
            conv_id: r.conv_id.to_string(),
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            model: r.model.to_string(),
            fail_open: r.fail_open,
            freeze_cut_tokens: r.freeze_cut_tokens,
            tools_total: r.tools_total,
            tools_kept: r.tools_kept,
            inbound: inbound.clone(),
            outbound: outbound.clone(),
        });
        let mut q = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
        q.retain(|s| s.conv_id != snap.conv_id);
        q.push_front(snap);
        q.truncate(self.cap);
    }

    fn list(&self) -> Vec<Arc<Snapshot>> {
        let q = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
        q.iter().cloned().collect()
    }

    fn get(&self, conv_id: &str) -> Option<Arc<Snapshot>> {
        let q = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
        q.iter().find(|s| s.conv_id == conv_id).cloned()
    }
}

/// Only loopback hosts may read conversation content: a browser tab on some
/// other site that DNS-rebinds to 127.0.0.1 still sends its own Host.
fn loopback_host(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

pub(crate) async fn index(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !loopback_host(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let snaps = st.inspect.list();
    let mut h = String::new();
    page_head(&mut h, "parsec · conversations");
    h.push_str("<h1>parsec · recent conversations</h1>");
    if st.inspect.cap == 0 {
        h.push_str("<p class=muted>Recording is off (<code>PARSEC_INSPECT=0</code>).</p>");
    } else if snaps.is_empty() {
        h.push_str(
            "<p class=muted>No requests yet since the proxy started. Conversations \
             appear here after their next turn.</p>",
        );
    } else {
        h.push_str(
            "<table><tr><th>#</th><th>conversation</th><th>model</th><th>msgs</th>\
             <th>sent → forwarded</th><th>cut</th><th>changed msgs</th><th>tools</th></tr>",
        );
        for s in &snaps {
            let d = Diff::of(s);
            let _ = write!(
                h,
                "<tr><td>{}</td><td><a href=\"/c/{}\"><code>{}</code></a>{}</td><td>{}</td>\
                 <td>{}</td><td>{} → {}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                s.seq,
                esc(&s.conv_id),
                esc(&s.conv_id),
                if s.fail_open {
                    " <span class=warn>fail-open</span>"
                } else {
                    ""
                },
                esc(&s.model),
                d.n_msgs,
                tok(d.in_chars),
                tok(d.out_chars),
                cut_pct(d.in_chars, d.out_chars),
                d.changed_msgs,
                tools_cell(s),
            );
        }
        h.push_str("</table>");
    }
    page_foot(&mut h, st.inspect.cap);
    Html(h).into_response()
}

pub(crate) async fn conversation(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(conv_id): Path<String>,
) -> Response {
    if !loopback_host(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(s) = st.inspect.get(&conv_id) else {
        return (
            StatusCode::NOT_FOUND,
            Html("<p>Conversation not in memory. <a href=\"/\">Back</a></p>".to_string()),
        )
            .into_response();
    };
    let d = Diff::of(&s);
    let mut h = String::new();
    page_head(&mut h, &format!("parsec · {}", s.conv_id));

    let (Some(inb), Some(out)) = (parse(&s.inbound), parse(&s.outbound)) else {
        h.push_str("<p class=warn>Body is not JSON; nothing to diff.</p>");
        page_foot(&mut h, st.inspect.cap);
        return Html(h).into_response();
    };

    // Body first so the header can carry the kept/cut totals it tallies.
    let mut b = String::new();
    let mut tally = Tally::default();
    render_tools(&mut b, &inb, &out);
    let sys_in = system_text(inb.get("system"));
    let sys_out = system_text(out.get("system"));
    tally.add(render_msg(
        &mut b,
        "system",
        None,
        &[("system".into(), sys_in)],
        &[("system".into(), sys_out)],
    ));

    let empty = Vec::new();
    let mi = inb
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mo = out
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for i in 0..mi.len().max(mo.len()) {
        let role = mi
            .get(i)
            .or_else(|| mo.get(i))
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let bi = mi
            .get(i)
            .map(|m| blocks(m.get("content")))
            .unwrap_or_default();
        let bo = mo
            .get(i)
            .map(|m| blocks(m.get("content")))
            .unwrap_or_default();
        tally.add(render_msg(&mut b, role, Some(i), &bi, &bo));
    }

    let _ = write!(
        h,
        "<p><a href=\"/\">← conversations</a></p><h1><code>{}</code></h1>\
         <p>{} · request #{} · {} messages · ~{} → ~{} tokens sent ({}) · \
         engine freeze cut {} tokens · tools {}{}</p>\
         <p class=totals><span class=keptc>kept ~{} tokens</span> · \
         <span class=cut>cut ~{} tokens</span> · {} of {} messages kept verbatim</p>",
        esc(&s.conv_id),
        esc(&s.model),
        s.seq,
        d.n_msgs,
        tok(d.in_chars),
        tok(d.out_chars),
        cut_pct(d.in_chars, d.out_chars),
        s.freeze_cut_tokens,
        tools_cell(&s),
        if s.fail_open {
            " · <span class=warn>fail-open: forwarded verbatim</span>"
        } else {
            ""
        },
        tok(tally.kept_chars),
        tok(tally.cut_chars),
        d.n_msgs.saturating_sub(d.changed_msgs),
        d.n_msgs,
    );
    h.push_str(
        "<p class=muted>Token figures here are chars/4 estimates for orientation; \
         the ledger's <code>count_tokens</code> counterfactual is the savings number.</p>\
         <p class=views>view: \
         <label><input type=radio name=view id=v-diff checked> kept + cut</label>\
         <label><input type=radio name=view id=v-kept> kept (what the model saw)</label>\
         <label><input type=radio name=view id=v-cut> cut only</label></p>",
    );
    h.push_str(&b);
    page_foot(&mut h, st.inspect.cap);
    Html(h).into_response()
}

/// Kept/cut character totals across a rendered request.
#[derive(Default)]
struct Tally {
    kept_chars: usize,
    cut_chars: usize,
}

impl Tally {
    fn add(&mut self, (kept, cut): (usize, usize)) {
        self.kept_chars += kept;
        self.cut_chars += cut;
    }
}

fn parse(b: &Bytes) -> Option<Value> {
    serde_json::from_slice(b).ok()
}

/// Whole-request size summary for list rows and the page header.
struct Diff {
    n_msgs: usize,
    in_chars: usize,
    out_chars: usize,
    changed_msgs: usize,
}

impl Diff {
    fn of(s: &Snapshot) -> Self {
        let (inb, out) = (parse(&s.inbound), parse(&s.outbound));
        let msgs = |v: &Option<Value>| {
            v.as_ref()
                .and_then(|v| v.get("messages"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let (mi, mo) = (msgs(&inb), msgs(&out));
        let changed_msgs = (0..mi.len().max(mo.len()))
            .filter(|&i| mi.get(i) != mo.get(i))
            .count();
        Self {
            n_msgs: mi.len(),
            in_chars: s.inbound.len(),
            out_chars: s.outbound.len(),
            changed_msgs,
        }
    }
}

/// A message's content flattened to labelled text blocks.
fn blocks(content: Option<&Value>) -> Vec<(String, String)> {
    match content {
        Some(Value::String(s)) => vec![("text".into(), s.clone())],
        Some(Value::Array(bs)) => bs.iter().map(block).collect(),
        _ => Vec::new(),
    }
}

fn block(b: &Value) -> (String, String) {
    let ty = b.get("type").and_then(Value::as_str).unwrap_or("?");
    let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match ty {
        "text" => ("text".into(), s("text")),
        "thinking" => ("thinking".into(), s("thinking")),
        "tool_use" => (
            format!("tool_use · {}", s("name")),
            b.get("input").map(pretty).unwrap_or_default(),
        ),
        "tool_result" => {
            let text = match b.get("content") {
                Some(Value::String(t)) => t.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .map(|p| match p.get("type").and_then(Value::as_str) {
                        Some("text") => p
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        Some(other) => format!("[{other}]"),
                        None => String::new(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            let err = b.get("is_error").and_then(Value::as_bool) == Some(true);
            (
                format!("tool_result{}", if err { " · error" } else { "" }),
                text,
            )
        }
        other => (other.to_string(), String::new()),
    }
}

fn system_text(sys: Option<&Value>) -> String {
    match sys {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(bs)) => bs
            .iter()
            .map(|b| b.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

fn render_tools(h: &mut String, inb: &Value, out: &Value) {
    let names = |v: &Value| -> Vec<(String, Value)> {
        v.get("tools")
            .and_then(Value::as_array)
            .map(|ts| {
                ts.iter()
                    .map(|t| {
                        let n = t.get("name").and_then(Value::as_str).unwrap_or("?");
                        (n.to_string(), t.clone())
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let (ti, to) = (names(inb), names(out));
    if ti.is_empty() && to.is_empty() {
        return;
    }
    let mut dropped = Vec::new();
    let mut altered = Vec::new();
    let mut kept = Vec::new();
    for (n, t) in &ti {
        match to.iter().find(|(m, _)| m == n) {
            None => dropped.push(n.as_str()),
            Some((_, u)) if u != t => altered.push(n.as_str()),
            _ => kept.push(n.as_str()),
        }
    }
    let changed = !dropped.is_empty() || !altered.is_empty();
    let _ = write!(
        h,
        "<details class=\"msg{}\"><summary><b>tools</b> {} sent → {} forwarded{}</summary>",
        if changed { " changed" } else { "" },
        ti.len(),
        to.len(),
        if changed {
            format!(" · {} dropped · {} altered", dropped.len(), altered.len())
        } else {
            String::new()
        },
    );
    if !kept.is_empty() {
        let _ = write!(
            h,
            "<p><b class=keptc>kept:</b> <code>{}</code></p>",
            esc(&kept.join(", "))
        );
    }
    if !dropped.is_empty() {
        let _ = write!(
            h,
            "<p><b>dropped:</b> <code>{}</code></p>",
            esc(&dropped.join(", "))
        );
    }
    if !altered.is_empty() {
        let _ = write!(
            h,
            "<p><b>altered (stubbed):</b> <code>{}</code></p>",
            esc(&altered.join(", "))
        );
    }
    h.push_str("</details>");
}

fn render_msg(
    h: &mut String,
    role: &str,
    idx: Option<usize>,
    bi: &[(String, String)],
    bo: &[(String, String)],
) -> (usize, usize) {
    let changed = bi != bo;
    if !changed && bi.iter().all(|(_, t)| t.is_empty()) {
        return (0, 0);
    }
    let label = match idx {
        Some(i) => format!("#{i} {role}"),
        None => role.to_string(),
    };
    // Kept = what was forwarded (fold markers included); cut = the removed
    // spans. Tallied from the same splits the blocks render from.
    let kept: usize = bo.iter().map(|(_, t)| t.chars().count()).sum();
    let cut: usize = (0..bi.len().max(bo.len()))
        .map(|j| match (bi.get(j), bo.get(j)) {
            (Some((_, a)), Some((_, b))) => split(a, b).1.chars().count(),
            (Some((_, a)), None) => a.chars().count(),
            _ => 0,
        })
        .sum();
    let _ = write!(
        h,
        "<details class=\"msg {role}{}\" open><summary><b>{}</b> {} · {}</summary>",
        if changed { " changed" } else { "" },
        esc(&label),
        esc(&kinds(bi)),
        if changed {
            format!(
                "<span class=keptc>kept ~{}</span> · <span class=cut>cut ~{} tokens</span>",
                kept / 4,
                cut / 4
            )
        } else {
            format!(
                "<span class=badge>kept</span> <span class=muted>~{} tokens</span>",
                kept / 4
            )
        },
    );
    for j in 0..bi.len().max(bo.len()) {
        match (bi.get(j), bo.get(j)) {
            (Some((k, a)), Some((_, b))) if a == b => {
                let _ = write!(
                    h,
                    "<div class=blk><div class=kind>{}</div>{}</div>",
                    esc(k),
                    preview(a)
                );
            }
            (Some((k, a)), Some((_, b))) => {
                let _ = write!(
                    h,
                    "<div class=\"blk changed\"><div class=kind>{}</div>",
                    esc(k)
                );
                render_cut(h, a, b);
                h.push_str("</div>");
            }
            (Some((k, a)), None) => {
                let _ = write!(
                    h,
                    "<div class=\"blk changed removed\"><div class=kind>{} · removed</div><pre class=del>{}</pre></div>",
                    esc(k),
                    esc(a)
                );
            }
            (None, Some((k, b))) => {
                let _ = write!(
                    h,
                    "<div class=\"blk changed\"><div class=kind>{} · added</div><pre class=ins>{}</pre></div>",
                    esc(k),
                    esc(b)
                );
            }
            (None, None) => {}
        }
    }
    h.push_str("</details>");
    (kept, cut)
}

fn kinds(bs: &[(String, String)]) -> String {
    let mut v: Vec<&str> = bs.iter().map(|(k, _)| k.as_str()).collect();
    v.dedup();
    v.join(", ")
}

/// Show a changed block as shared prefix · removed middle · inserted middle ·
/// shared suffix — fold markers replace a contiguous span, so this isolates
/// exactly what was cut and what stands in for it.
fn render_cut(h: &mut String, a: &str, b: &str) {
    let (pre, del, ins, suf) = split(a, b);
    let _ = write!(
        h,
        "<pre>{}<details class=delwrap><summary class=del>− {} chars cut (click to show)</summary>\
         <span class=del>{}</span></details><span class=ins>{}</span>{}</pre>",
        ctx_before(&pre),
        del.chars().count(),
        esc(&del),
        esc(&ins),
        ctx_after(&suf),
    );
}

/// (shared prefix, removed middle, inserted middle, shared suffix).
fn split(a: &str, b: &str) -> (String, String, String, String) {
    if a == b {
        return (a.to_string(), String::new(), String::new(), String::new());
    }
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let p = ac.iter().zip(&bc).take_while(|(x, y)| x == y).count();
    let s = ac[p..]
        .iter()
        .rev()
        .zip(bc[p..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let st = |cs: &[char]| cs.iter().collect::<String>();
    (
        st(&ac[..p]),
        st(&ac[p..ac.len() - s]),
        st(&bc[p..bc.len() - s]),
        st(&ac[ac.len() - s..]),
    )
}

fn preview(t: &str) -> String {
    let n = t.chars().count();
    if n <= PREVIEW_CHARS {
        return format!("<pre>{}</pre>", esc(t));
    }
    let head: String = t.chars().take(PREVIEW_CHARS).collect();
    format!(
        "<pre>{}</pre><details><summary class=muted>… {} more chars</summary><pre>{}</pre></details>",
        esc(&head),
        n - PREVIEW_CHARS,
        esc(&t.chars().skip(PREVIEW_CHARS).collect::<String>()),
    )
}

/// Kept context before a cut: the last `PREVIEW_CHARS` shown, the rest one
/// click away (collapsed, never dropped).
fn ctx_before(t: &str) -> String {
    let n = t.chars().count();
    if n <= PREVIEW_CHARS {
        return esc(t);
    }
    let head: String = t.chars().take(n - PREVIEW_CHARS).collect();
    let tail: String = t.chars().skip(n - PREVIEW_CHARS).collect();
    format!(
        "<details class=ctx><summary class=muted>… {} earlier kept chars</summary>{}</details>{}",
        n - PREVIEW_CHARS,
        esc(&head),
        esc(&tail)
    )
}

/// Kept context after a cut: the first `PREVIEW_CHARS` shown, the rest one
/// click away.
fn ctx_after(t: &str) -> String {
    let n = t.chars().count();
    if n <= PREVIEW_CHARS {
        return esc(t);
    }
    let head: String = t.chars().take(PREVIEW_CHARS).collect();
    let tail: String = t.chars().skip(PREVIEW_CHARS).collect();
    format!(
        "{}<details class=ctx><summary class=muted>… {} more kept chars</summary>{}</details>",
        esc(&head),
        n - PREVIEW_CHARS,
        esc(&tail)
    )
}

fn tok(bytes: usize) -> String {
    let t = bytes / 4;
    if t >= 1000 {
        format!("{:.1}k", t as f64 / 1000.0)
    } else {
        t.to_string()
    }
}

fn cut_pct(a: usize, b: usize) -> String {
    if a == 0 || b >= a {
        return "0%".into();
    }
    format!("−{:.0}%", (a - b) as f64 * 100.0 / a as f64)
}

fn tools_cell(s: &Snapshot) -> String {
    match (s.tools_kept, s.tools_total) {
        (Some(k), Some(t)) => format!("{k}/{t}"),
        _ => "–".into(),
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

fn page_head(h: &mut String, title: &str) {
    let _ = write!(
        h,
        "<!doctype html><html><head><meta charset=utf-8><title>{}</title><style>{}</style></head><body>",
        esc(title),
        CSS
    );
}

fn page_foot(h: &mut String, cap: usize) {
    let _ = write!(
        h,
        "<p class=muted>In-memory only: the latest request of the last {cap} conversations, \
         since this proxy started. Nothing is written to disk. <code>PARSEC_INSPECT</code> sets the count.</p>\
         </body></html>"
    );
}

const CSS: &str = "
body{font:14px/1.45 -apple-system,system-ui,sans-serif;margin:24px auto;max-width:1100px;padding:0 16px;color:#1d1d1f;background:#fff}
@media(prefers-color-scheme:dark){body{color:#e6e6e6;background:#161618}a{color:#8ab4ff}.msg{border-color:#333!important}th,td{border-color:#333!important}}
h1{font-size:18px}code{font:12px ui-monospace,Menlo,monospace}
table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:6px 8px;border-bottom:1px solid #e3e3e3;font-variant-numeric:tabular-nums}
.muted{color:#888}.warn{color:#c8641a;font-weight:600}.cut{color:#c8641a}
.msg{border:1px solid #e3e3e3;border-radius:6px;margin:8px 0;padding:6px 10px}
.msg.changed{border-left:4px solid #c8641a}.msg>summary{cursor:pointer}
.msg.user>summary b{color:#2a6fdb}.msg.assistant>summary b{color:#7a4fd0}
.keptc{color:#1f8a4c}.badge{font-size:11px;color:#1f8a4c;border:1px solid currentColor;border-radius:4px;padding:0 4px}
.totals{font-size:15px}.views label{margin-right:14px;cursor:pointer}
body:has(#v-kept:checked) .delwrap,body:has(#v-kept:checked) .removed{display:none}
body:has(#v-cut:checked) .msg:not(.changed){display:none}
.ctx{display:inline}.ctx>summary{cursor:pointer}
.blk{margin:8px 0}.kind{font-size:11px;color:#888;text-transform:uppercase;letter-spacing:.04em}
pre{white-space:pre-wrap;word-break:break-word;font:12px/1.4 ui-monospace,Menlo,monospace;margin:4px 0;max-height:none}
.del{background:rgba(220,60,60,.15);text-decoration:line-through;text-decoration-color:rgba(220,60,60,.5)}
summary.del{text-decoration:none;cursor:pointer;display:inline}
.delwrap{display:inline}.delwrap[open]>summary{display:block}
.ins{background:rgba(40,170,90,.18)}
";

#[cfg(test)]
mod tests {
    use super::*;

    fn host(h: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(header::HOST, h.parse().unwrap());
        m
    }

    #[test]
    fn loopback_only() {
        assert!(loopback_host(&host("127.0.0.1:8787")));
        assert!(loopback_host(&host("localhost:8787")));
        assert!(loopback_host(&host("[::1]:8787")));
        assert!(!loopback_host(&host("evil.example:8787")));
        assert!(!loopback_host(&HeaderMap::new()));
    }

    #[test]
    fn keeps_latest_per_conv_and_caps() {
        let ins = Inspector::with_cap(2);
        let rec = |c| Record {
            conv_id: c,
            model: "m",
            fail_open: false,
            freeze_cut_tokens: 0,
            tools_total: None,
            tools_kept: None,
        };
        let b = Bytes::from_static(b"{}");
        ins.record(rec("a"), &b, &b);
        ins.record(rec("b"), &b, &b);
        ins.record(rec("a"), &b, &b);
        ins.record(rec("c"), &b, &b);
        let ids: Vec<_> = ins.list().iter().map(|s| s.conv_id.clone()).collect();
        assert_eq!(ids, ["c", "a"]);
        assert_eq!(ins.get("a").map(|s| s.seq), Some(3));
    }

    #[test]
    fn cut_isolates_replaced_span() {
        let mut h = String::new();
        render_cut(&mut h, "keep AAAA tail", "keep [folded] tail");
        assert!(h.contains("<span class=del>AAAA</span>"));
        assert!(h.contains("<span class=ins>[folded]</span>"));
    }

    #[test]
    fn unchanged_message_is_shown_open_as_kept() {
        let mut h = String::new();
        let b = vec![("text".to_string(), "abcdefgh".to_string())];
        let (kept, cut) = render_msg(&mut h, "user", Some(0), &b, &b);
        assert_eq!((kept, cut), (8, 0));
        assert!(h.contains("\" open>"));
        assert!(h.contains("<span class=badge>kept</span>"));
        assert!(h.contains("abcdefgh"));
    }

    #[test]
    fn changed_message_tallies_kept_and_cut() {
        let mut h = String::new();
        let a = vec![("tool_result".to_string(), "keep AAAA tail".to_string())];
        let b = vec![("tool_result".to_string(), "keep [x] tail".to_string())];
        assert_eq!(render_msg(&mut h, "user", Some(1), &a, &b), (13, 4));
    }

    #[test]
    fn long_kept_context_is_collapsed_not_dropped() {
        let long = "k".repeat(PREVIEW_CHARS + 10);
        let mut h = String::new();
        render_cut(&mut h, &format!("{long}CUT"), &format!("{long}[m]"));
        assert!(h.contains("10 earlier kept chars"));
        // the 10 older chars sit behind the fold; the last PREVIEW_CHARS show
        let shown = format!(
            "</summary>{}</details>{}",
            "k".repeat(10),
            "k".repeat(PREVIEW_CHARS)
        );
        assert!(h.contains(&shown));
    }

    #[test]
    fn disabled_records_nothing() {
        let ins = Inspector::with_cap(0);
        let b = Bytes::from_static(b"{}");
        ins.record(
            Record {
                conv_id: "a",
                model: "m",
                fail_open: false,
                freeze_cut_tokens: 0,
                tools_total: None,
                tools_kept: None,
            },
            &b,
            &b,
        );
        assert!(ins.list().is_empty());
    }
}
