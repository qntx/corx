//! `corx-corpus`: runs the URL corpus (`corpus/urls.txt`) against a corx
//! deployment, scores each response against the line's expectation, and
//! prints a table + totals. `compare` diffs two `run --json` outputs.
//! Reports only — never gates: `run` exits 0 once the list parsed.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a CLI reporting tool"
)]

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinSet;

/// Client User-Agent the tool sends to the service.
const UA: &str = concat!("corx-corpus/", env!("CARGO_PKG_VERSION"));

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// What to do.
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// GET every listed URL's preview and score the responses.
    Run {
        /// Base URL of the service, e.g. `http://127.0.0.1:8787`.
        base: String,
        /// The corpus list (URL + expectation per line).
        #[arg(long, default_value = "corpus/urls.txt")]
        list: PathBuf,
        /// Write the records + totals as JSON here.
        #[arg(long)]
        json: Option<PathBuf>,
        /// In-flight requests at once.
        #[arg(
            long,
            default_value_t = NonZero::<usize>::new(4).unwrap_or(NonZero::<usize>::MIN)
        )]
        concurrency: NonZero<usize>,
        /// Per-request timeout in seconds.
        #[arg(long, default_value_t = 20)]
        timeout_secs: u64,
    },
    /// Compare two `run --json` outputs, joined by URL (a = before, b = after).
    Compare {
        /// The "before" run's JSON.
        a: PathBuf,
        /// The "after" run's JSON.
        b: PathBuf,
        /// Write the joined rows as JSON here.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Print the comparison as a Markdown table.
        #[arg(long)]
        markdown: bool,
    },
}

/// Metadata-bearing fields in TDSIF order.
const FIELD_ORDER: [char; 5] = ['T', 'D', 'S', 'I', 'F'];

/// Bit index of `I` in [`FIELD_ORDER`].
const I_BIT: usize = 3;

/// The expectation after the URL on a corpus line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Expect {
    /// Informational only; always passes.
    Any,
    /// A page result; optional `source` and required fields.
    Page {
        /// Required `source` value, if the expectation names one.
        source: Option<String>,
        /// Required fields, as bit indexes into [`FIELD_ORDER`].
        fields: u8,
    },
    /// A media result; optional `type/subtype` match (`*` subtype).
    Media {
        /// Required `type/subtype` (`subtype` may be `*`).
        mime: Option<(String, String)>,
    },
    /// Exactly this error slug.
    Error(String),
}

/// Parses the expectation grammar; `None` = malformed.
fn parse_expect(raw: &str) -> Option<Expect> {
    if raw == "any" {
        return Some(Expect::Any);
    }
    if raw == "media" {
        return Some(Expect::Media { mime: None });
    }
    if raw.starts_with("media:") {
        return Some(Expect::Media {
            mime: Some(parse_media(raw)?),
        });
    }
    if let Some(slug) = raw.strip_prefix("error:") {
        let ok = !slug.is_empty() && slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        return ok.then(|| Expect::Error(slug.to_owned()));
    }
    let (head, fields) = match raw.split_once(':') {
        Some((h, f)) => (h, parse_fields(f)?),
        None => (raw, 0u8),
    };
    // `page` alone is "any page"; `page:<F>(+<F>)*` adds required fields;
    // `page/<source>` and `page/<source>:<F>(+<F>)*` pin the source.
    let source = match head {
        "page" => None,
        "page/html" => Some("html".to_owned()),
        "page/oembed" => Some("oembed".to_owned()),
        _ => return None,
    };
    Some(Expect::Page { source, fields })
}

/// Parses a `<type>/<subtype or *>` mime spec like `image/png` or `image/*`.
fn parse_media(raw: &str) -> Option<(String, String)> {
    let spec = raw.strip_prefix("media:")?;
    let (ty, sub) = spec.split_once('/')?;
    let ok = !ty.is_empty()
        && !sub.is_empty()
        && !ty.contains('*')
        && (sub == "*"
            || sub
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'));
    ok.then(|| (ty.to_owned(), sub.to_owned()))
}

/// Parses `<F>(+<F>)*`, F ∈ {T,D,S,I,F}, into a bit mask.
fn parse_fields(raw: &str) -> Option<u8> {
    if raw.is_empty() {
        return None;
    }
    let mut mask = 0u8;
    for tok in raw.split('+') {
        let mut chars = tok.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            return None;
        };
        let bit = FIELD_ORDER
            .iter()
            .position(|&f| f == ch)
            .map(|p| 1u8 << p)?;
        if mask & bit != 0 {
            return None; // duplicate field letter
        }
        mask |= bit;
    }
    Some(mask)
}

/// Renders the TDSIF mask of a page body (`"TD-IF"`).
fn fields_mask(body: &Value) -> String {
    const KEYS: [&str; 5] = ["title", "description", "siteName", "image", "favicon"];
    FIELD_ORDER
        .iter()
        .zip(KEYS.iter())
        .map(|(f, k)| {
            if body.get(*k).is_some_and(|v| !v.is_null()) {
                *f
            } else {
                '-'
            }
        })
        .collect()
}

/// TDSIF string → bit mask (only letters landing in their own slot count).
fn field_bits(fields: &str) -> u8 {
    FIELD_ORDER
        .iter()
        .zip(fields.chars())
        .enumerate()
        .fold(
            0u8,
            |mask, (i, (f, c))| {
                if c == *f { mask | (1u8 << i) } else { mask }
            },
        )
}

/// The minimum usable page: title or image.
const USABLE_PAGE_FIELDS: u8 = 1u8 | (1u8 << I_BIT);

/// A parsed corpus line.
struct Entry {
    /// The URL under test.
    url: String,
    /// The raw expectation text (for the record's `expect` field).
    raw: String,
    /// The parsed expectation.
    expect: Expect,
}

/// Reads the corpus list; on error returns a message naming the line.
fn parse_list(text: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for (n, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(url), Some(raw), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!(
                "line {}: want `<url> <expectation>`: {line}",
                n + 1
            ));
        };
        let Some(expect) = parse_expect(raw) else {
            return Err(format!("line {}: bad expectation {raw:?}", n + 1));
        };
        out.push(Entry {
            url: url.to_owned(),
            raw: raw.to_owned(),
            expect,
        });
    }
    Ok(out)
}

/// One URL's outcome, matching `run --json`'s record shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    /// The requested URL.
    url: String,
    /// The expectation text from the list.
    expect: String,
    /// HTTP status (absent on transport failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    /// `page` | `media` | error slug | `transport:<reason>`.
    outcome: String,
    /// `kind` when the body parsed as a preview.
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    /// `source` when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    /// TDSIF mask, pages only.
    #[serde(skip_serializing_if = "Option::is_none")]
    fields: Option<String>,
    /// `mediaType` when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    media_type: Option<String>,
    /// `upstreamStatus` when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_status: Option<u16>,
    /// `error` slug when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// First 60 chars of `title`, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// Wall-clock latency.
    latency_ms: u64,
    /// Whether the expectation held.
    pass: bool,
}

/// Scores a record against an expectation.
fn passes(expect: &Expect, rec: &Record) -> bool {
    match expect {
        Expect::Any => true,
        Expect::Page { source, fields } => {
            if rec.kind.as_deref() != Some("page") {
                return false;
            }
            if let Some(s) = source
                && rec.source.as_deref() != Some(s.as_str())
            {
                return false;
            }
            let mask = rec.fields.as_deref().map_or(0, field_bits);
            mask & fields == *fields
        }
        Expect::Media { mime } => {
            if rec.kind.as_deref() != Some("media") {
                return false;
            }
            mime.as_ref().is_none_or(|(ty, sub)| {
                let mt = rec.media_type.as_deref().unwrap_or("");
                let (got_ty, got_sub) = mt.split_once('/').unwrap_or(("", ""));
                got_ty == ty && (sub == "*" || got_sub == sub)
            })
        }
        Expect::Error(slug) => rec.error.as_deref() == Some(slug.as_str()),
    }
}

/// One request: `GET <base>/v1/preview?url=<target>`. Never retries.
async fn fetch_one(client: &reqwest::Client, base: &str, entry: &Entry) -> Record {
    let started = Instant::now();
    let endpoint = format!("{base}/v1/preview");
    let res = client
        .get(&endpoint)
        .query(&[("url", entry.url.as_str())])
        .send()
        .await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut rec = Record {
        url: entry.url.clone(),
        expect: entry.raw.clone(),
        status: None,
        outcome: String::new(),
        kind: None,
        source: None,
        fields: None,
        media_type: None,
        upstream_status: None,
        error: None,
        title: None,
        latency_ms,
        pass: false,
    };
    match res {
        Err(e) => {
            let reason = if e.is_timeout() {
                "transport:timeout"
            } else if e.is_connect() {
                "transport:connect"
            } else {
                "transport:request"
            };
            reason.clone_into(&mut rec.outcome);
        }
        Ok(r) => {
            rec.status = Some(r.status().as_u16());
            match r.text().await {
                Err(_) => "transport:decode".clone_into(&mut rec.outcome),
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Err(_) => "transport:decode".clone_into(&mut rec.outcome),
                    Ok(body) => fill_from_body(&mut rec, &body),
                },
            }
        }
    }
    rec.pass = passes(&entry.expect, &rec);
    rec
}

/// Copies body fields into the record and derives the outcome.
fn fill_from_body(rec: &mut Record, body: &Value) {
    let pick_str = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_owned);
    rec.kind = pick_str("kind");
    rec.source = pick_str("source");
    rec.media_type = pick_str("mediaType");
    rec.error = pick_str("error");
    rec.upstream_status = body
        .get("upstreamStatus")
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok());
    rec.title = pick_str("title").map(|t| t.chars().take(60).collect());
    if rec.kind.as_deref() == Some("page") {
        rec.fields = Some(fields_mask(body));
    }
    match rec.kind.as_deref() {
        Some("page") => "page".clone_into(&mut rec.outcome),
        Some("media") => "media".clone_into(&mut rec.outcome),
        _ => match rec.error.as_deref() {
            Some(e) => e.clone_into(&mut rec.outcome),
            None => "transport:shape".clone_into(&mut rec.outcome),
        },
    }
}

/// Nearest-rank percentile over sorted values.
fn nearest_rank(sorted: &[u64], pct: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (pct * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

/// The three-latency summary used in `totals`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Latency {
    /// Nearest-rank p50.
    p50: u64,
    /// Nearest-rank p95.
    p95: u64,
    /// Maximum.
    max: u64,
}

/// p50/p95/max over an iterator of records.
fn latencies<'a>(recs: impl Iterator<Item = &'a Record>) -> Latency {
    let mut v: Vec<u64> = recs.map(|r| r.latency_ms).collect();
    v.sort_unstable();
    Latency {
        p50: nearest_rank(&v, 50).unwrap_or(0),
        p95: nearest_rank(&v, 95).unwrap_or(0),
        max: v.last().copied().unwrap_or(0),
    }
}

/// One human-table row.
fn line(r: &Record) -> String {
    let verdict = if r.pass { "PASS" } else { "FAIL" };
    let status = r.status.map_or_else(|| "-".to_owned(), |v| v.to_string());
    let detail = if r.kind.as_deref() == Some("page") {
        r.fields.clone()
    } else {
        r.media_type.clone()
    };
    let detail = detail.unwrap_or_else(|| "-".to_owned());
    let src = r.source.clone().unwrap_or_else(|| "-".to_owned());
    let us = r
        .upstream_status
        .map_or_else(|| "-".to_owned(), |v| v.to_string());
    let title = r.title.clone().unwrap_or_else(|| "-".to_owned());
    let secs = r.latency_ms as f64 / 1000.0;
    format!(
        "{verdict:<4}  {status:>3}  {secs:>6.2}s  {outcome:<20}  {detail:<5}  {src:<7}  {us:>3}  {title:<60}  {url}",
        outcome = r.outcome,
        url = r.url,
    )
}

/// Outcome histogram over the records.
fn outcome_counts(records: &[Record]) -> BTreeMap<String, usize> {
    let mut outcomes: BTreeMap<String, usize> = BTreeMap::new();
    for r in records {
        *outcomes.entry(r.outcome.clone()).or_default() += 1;
    }
    outcomes
}

/// Prints the per-URL table, totals, and writes `--json` when requested.
fn finish_run(records: &[Record], base: &str, started_at: u64, json: Option<&Path>) -> ExitCode {
    for r in records {
        println!("{}", line(r));
    }
    let pass = records.iter().filter(|r| r.pass).count();
    let outcomes = outcome_counts(records);
    let all = latencies(records.iter());
    let ok = latencies(records.iter().filter(|r| r.pass));
    println!();
    println!(
        "{} urls: {} pass, {} fail",
        records.len(),
        pass,
        records.len() - pass
    );
    println!(
        "outcomes: {}",
        outcomes
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "latency ms: all p50={} p95={} max={} | pass p50={} p95={} max={}",
        all.p50, all.p95, all.max, ok.p50, ok.p95, ok.max
    );
    if let Some(path) = json {
        let doc = serde_json::json!({
            "base": base,
            "startedAt": started_at,
            "records": records,
            "totals": {
                "count": records.len(),
                "pass": pass,
                "fail": records.len() - pass,
                "outcomes": outcomes,
                "latencyMs": all,
            },
        });
        if let Err(e) = std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default())
        {
            eprintln!("cannot write {}: {e}", path.display());
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

/// `run`: fetch every entry with bounded concurrency; keep list order.
async fn run(base: &str, list: &Path, args: &RunArgs) -> ExitCode {
    let text = match std::fs::read_to_string(list) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read {}: {e}", list.display());
            return ExitCode::from(2);
        }
    };
    let entries = match parse_list(&text) {
        Ok(e) => e,
        Err(msg) => {
            eprintln!("{}: {msg}", list.display());
            return ExitCode::from(2);
        }
    };
    let client = match reqwest::Client::builder()
        .user_agent(UA)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(args.timeout_secs))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot build http client: {e}");
            return ExitCode::from(2);
        }
    };
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let cap = args.concurrency.get();
    let mut recs: Vec<Option<Record>> = Vec::new();
    recs.resize_with(entries.len(), || None);
    let mut set: JoinSet<(usize, Record)> = JoinSet::new();
    let mut next = 0usize;
    while next < entries.len() || !set.is_empty() {
        while next < entries.len() && set.len() < cap {
            let Some(entry) = entries.get(next) else {
                break;
            };
            let client = client.clone();
            let base = base.to_owned();
            let entry = Entry {
                url: entry.url.clone(),
                raw: entry.raw.clone(),
                expect: entry.expect.clone(),
            };
            let i = next;
            set.spawn(async move { (i, fetch_one(&client, &base, &entry).await) });
            next += 1;
        }
        match set.join_next().await {
            Some(Ok((i, rec))) => {
                if let Some(slot) = recs.get_mut(i) {
                    *slot = Some(rec);
                }
            }
            Some(Err(e)) => {
                eprintln!("task failed: {e}");
                return ExitCode::from(2);
            }
            None => break,
        }
    }
    let records: Vec<Record> = recs.into_iter().flatten().collect();
    finish_run(&records, base, started_at, args.json.as_deref())
}

/// Result ranking for `compare`: error < empty page < T-or-I < T+I < T+D+I.
/// A media result ranks like `page:T+I` in both positions of the settled
/// rule (the parenthetical in the design is literal).
fn rank(r: &Record) -> u8 {
    match r.kind.as_deref() {
        Some("media") => 3,
        Some("page") => {
            let bits = r.fields.as_deref().map_or(0, field_bits);
            let t = bits & 1u8 != 0;
            let i = bits & (1u8 << I_BIT) != 0;
            let d = bits & 2u8 != 0;
            match (t, i, d) {
                (true, true, true) => 4,
                (true, true, false) => 3,
                (true, false, _) | (false, true, _) => 2,
                _ => 1,
            }
        }
        _ => 0,
    }
}

/// Whether the record is a usable result: page with T or I, or media.
fn usable(r: &Record) -> bool {
    match r.kind.as_deref() {
        Some("media") => true,
        Some("page") => r.fields.as_deref().map_or(0, field_bits) & USABLE_PAGE_FIELDS != 0,
        _ => false,
    }
}

/// A `run --json` document.
#[derive(Deserialize)]
struct RunDoc {
    /// The per-URL records.
    records: Vec<Record>,
}

/// One joined compare row.
#[derive(Serialize)]
struct Row {
    /// The URL.
    url: String,
    /// `+` improved, `-` regressed, `=` same, `?` no before record.
    marker: char,
    /// Before-run outcome.
    #[serde(rename = "aOutcome", skip_serializing_if = "Option::is_none")]
    a_outcome: Option<String>,
    /// After-run outcome.
    #[serde(rename = "bOutcome")]
    b_outcome: String,
    /// Before-run fields/mediaType.
    #[serde(rename = "aFields", skip_serializing_if = "Option::is_none")]
    a_fields: Option<String>,
    /// After-run fields/mediaType.
    #[serde(rename = "bFields", skip_serializing_if = "Option::is_none")]
    b_fields: Option<String>,
    /// Before-run latency ms.
    #[serde(rename = "aLatencyMs", skip_serializing_if = "Option::is_none")]
    a_latency_ms: Option<u64>,
    /// After-run latency ms.
    #[serde(rename = "bLatencyMs")]
    b_latency_ms: u64,
}

/// The fields-or-mediaType detail column for a record.
fn detail_of(r: &Record) -> Option<String> {
    if r.kind.as_deref() == Some("page") {
        r.fields.clone()
    } else {
        r.media_type.clone()
    }
}

/// Reads a `run --json` file.
fn load_run(p: &Path) -> Result<RunDoc, String> {
    let s = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))
}

/// Joins b's records against a's by URL, marking each row.
fn join_rows(a: &RunDoc, b: &RunDoc) -> Vec<Row> {
    let a_map: BTreeMap<&str, &Record> = a.records.iter().map(|r| (r.url.as_str(), r)).collect();
    let mut rows = Vec::new();
    for br in &b.records {
        match a_map.get(br.url.as_str()) {
            Some(ar) => rows.push(Row {
                url: br.url.clone(),
                marker: match rank(br).cmp(&rank(ar)) {
                    Ordering::Greater => '+',
                    Ordering::Less => '-',
                    Ordering::Equal => '=',
                },
                a_outcome: Some(ar.outcome.clone()),
                b_outcome: br.outcome.clone(),
                a_fields: detail_of(ar),
                b_fields: detail_of(br),
                a_latency_ms: Some(ar.latency_ms),
                b_latency_ms: br.latency_ms,
            }),
            None => rows.push(Row {
                url: br.url.clone(),
                marker: '?',
                a_outcome: None,
                b_outcome: br.outcome.clone(),
                a_fields: None,
                b_fields: detail_of(br),
                a_latency_ms: None,
                b_latency_ms: br.latency_ms,
            }),
        }
    }
    rows
}

/// Prints compare rows as the plain table or a Markdown table.
fn print_rows(rows: &[Row], markdown: bool) {
    if markdown {
        println!("| url | before | after | ms |");
        println!("|---|---|---|---|");
    }
    for r in rows {
        let av = r.a_outcome.clone().unwrap_or_else(|| "-".to_owned());
        let af = r.a_fields.clone().unwrap_or_else(|| "-".to_owned());
        let bf = r.b_fields.clone().unwrap_or_else(|| "-".to_owned());
        let lat = r.a_latency_ms.map_or_else(
            || format!("-→{}ms", r.b_latency_ms),
            |x| format!("{x}→{}ms", r.b_latency_ms),
        );
        if markdown {
            println!(
                "| {} | {} ({}) | {} ({}) | {} |",
                r.url, av, af, r.b_outcome, bf, lat
            );
        } else {
            println!(
                "{}  {:<20} → {:<20}  {:<5} → {:<5}  {:>15}  {}",
                r.marker, av, r.b_outcome, af, bf, lat, r.url
            );
        }
    }
}

/// Pass count (against b's expectations) and usable-result count.
fn tally(recs: &[Record], b: &RunDoc) -> (usize, usize) {
    let mut pass = 0;
    let mut usable_n = 0;
    for r in recs {
        let hit = b.records.iter().find(|x| x.url == r.url);
        if let Some(br) = hit
            && let Some(e) = parse_expect(&br.expect)
            && passes(&e, r)
        {
            pass += 1;
        }
        if usable(r) {
            usable_n += 1;
        }
    }
    (pass, usable_n)
}

/// `compare`: join two run files by URL and mark each row.
fn compare(a_path: &Path, b_path: &Path, json: Option<&Path>, markdown: bool) -> ExitCode {
    let (a, b) = match (load_run(a_path), load_run(b_path)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(m), _) | (_, Err(m)) => {
            eprintln!("{m}");
            return ExitCode::from(2);
        }
    };
    let rows = join_rows(&a, &b);
    print_rows(&rows, markdown);
    let (a_pass, a_usable) = tally(&a.records, &b);
    let (b_pass, b_usable) = tally(&b.records, &b);
    let al = latencies(a.records.iter());
    let bl = latencies(b.records.iter());
    println!();
    println!(
        "before: {a_pass} pass, {a_usable} usable, p50={} p95={}",
        al.p50, al.p95
    );
    println!(
        "after:  {b_pass} pass, {b_usable} usable, p50={} p95={}",
        bl.p50, bl.p95
    );
    if let Some(path) = json {
        let doc = serde_json::json!({
            "a": a_path, "b": b_path, "rows": rows,
            "before": { "pass": a_pass, "usable": a_usable, "latencyMs": al },
            "after": { "pass": b_pass, "usable": b_usable, "latencyMs": bl },
        });
        if let Err(e) = std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default())
        {
            eprintln!("cannot write {}: {e}", path.display());
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

/// `run` subcommand arguments.
struct RunArgs {
    /// `--json` output path.
    json: Option<PathBuf>,
    /// In-flight cap.
    concurrency: NonZero<usize>,
    /// Per-request timeout.
    timeout_secs: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run {
            base,
            list,
            json,
            concurrency,
            timeout_secs,
        } => {
            run(
                &base,
                &list,
                &RunArgs {
                    json,
                    concurrency,
                    timeout_secs,
                },
            )
            .await
        }
        Cmd::Compare {
            a,
            b,
            json,
            markdown,
        } => compare(&a, &b, json.as_deref(), markdown),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a malformed fixture fails the test"
)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_expectation_form() {
        assert_eq!(parse_expect("any"), Some(Expect::Any), "any");
        assert_eq!(
            parse_expect("page"),
            Some(Expect::Page {
                source: None,
                fields: 0
            }),
            "bare page"
        );
        assert_eq!(
            parse_expect("page:T+I"),
            Some(Expect::Page {
                source: None,
                fields: (1 << 0) | (1 << 3)
            }),
            "page fields"
        );
        assert_eq!(
            parse_expect("page:T+D"),
            Some(Expect::Page {
                source: None,
                fields: (1 << 0) | (1 << 1)
            }),
            "page T+D"
        );
        assert_eq!(
            parse_expect("page/oembed:T"),
            Some(Expect::Page {
                source: Some("oembed".to_owned()),
                fields: 1
            }),
            "oembed with field"
        );
        assert_eq!(
            parse_expect("page/oembed"),
            Some(Expect::Page {
                source: Some("oembed".to_owned()),
                fields: 0
            }),
            "oembed bare"
        );
        assert_eq!(
            parse_expect("page/html:T"),
            Some(Expect::Page {
                source: Some("html".to_owned()),
                fields: 1
            }),
            "html with field"
        );
        assert_eq!(
            parse_expect("media"),
            Some(Expect::Media { mime: None }),
            "bare media"
        );
        assert_eq!(
            parse_expect("media:image/png"),
            Some(Expect::Media {
                mime: Some(("image".to_owned(), "png".to_owned()))
            }),
            "exact mime"
        );
        assert_eq!(
            parse_expect("media:image/*"),
            Some(Expect::Media {
                mime: Some(("image".to_owned(), "*".to_owned()))
            }),
            "type wildcard"
        );
        assert_eq!(
            parse_expect("media:application/json"),
            Some(Expect::Media {
                mime: Some(("application".to_owned(), "json".to_owned()))
            }),
            "application/json"
        );
        assert_eq!(
            parse_expect("error:no_metadata"),
            Some(Expect::Error("no_metadata".to_owned())),
            "error slug"
        );
    }

    #[test]
    fn rejects_bad_expectations() {
        for bad in [
            "page:T+",         // trailing +
            "page:X",          // unknown field
            "page:T+T",        // duplicate
            "media:",          // empty spec
            "media:/png",      // missing type
            "media:*/png",     // wildcard type
            "error:",          // empty slug
            "pag",             // unknown head
            "page/oembed:T+B", // unknown field letter
        ] {
            assert_eq!(parse_expect(bad), None, "{bad} must be rejected");
        }
    }

    fn rec(kind: Option<&str>, fields: Option<&str>, source: Option<&str>) -> Record {
        Record {
            url: "https://x/".to_owned(),
            expect: String::new(),
            status: Some(200),
            outcome: String::new(),
            kind: kind.map(str::to_owned),
            source: source.map(str::to_owned),
            fields: fields.map(str::to_owned),
            media_type: None,
            upstream_status: None,
            error: None,
            title: None,
            latency_ms: 1,
            pass: false,
        }
    }

    #[test]
    fn evaluates_expectations() {
        let page_tif = rec(Some("page"), Some("TD-IF"), Some("html"));
        assert!(passes(&parse_expect("any").unwrap(), &page_tif), "any");
        assert!(passes(&parse_expect("page").unwrap(), &page_tif), "page");
        assert!(
            passes(&parse_expect("page:T+I").unwrap(), &page_tif),
            "T+I present"
        );
        assert!(
            passes(&parse_expect("page:I+F").unwrap(), &page_tif),
            "I+F present"
        );
        assert!(
            !passes(&parse_expect("page:T+S").unwrap(), &page_tif),
            "S missing"
        );
        assert!(
            passes(&parse_expect("page/html:T").unwrap(), &page_tif),
            "html source"
        );
        assert!(
            !passes(&parse_expect("page/oembed:T").unwrap(), &page_tif),
            "wrong source"
        );

        let oembed = rec(Some("page"), Some("T----"), Some("oembed"));
        assert!(
            passes(&parse_expect("page/oembed:T").unwrap(), &oembed),
            "oembed source + field"
        );
        assert!(
            !passes(&parse_expect("page/oembed:D").unwrap(), &oembed),
            "oembed missing D"
        );

        let media = Record {
            media_type: Some("image/png".to_owned()),
            ..rec(Some("media"), None, None)
        };
        assert!(
            passes(&parse_expect("media").unwrap(), &media),
            "bare media"
        );
        assert!(
            passes(&parse_expect("media:image/*").unwrap(), &media),
            "type wildcard"
        );
        assert!(
            passes(&parse_expect("media:image/png").unwrap(), &media),
            "exact mime"
        );
        assert!(
            !passes(&parse_expect("media:video/*").unwrap(), &media),
            "wrong type"
        );

        let err = Record {
            error: Some("upstream_status".to_owned()),
            outcome: "upstream_status".to_owned(),
            ..rec(None, None, None)
        };
        assert!(
            passes(&parse_expect("error:upstream_status").unwrap(), &err),
            "error slug"
        );
        assert!(
            !passes(&parse_expect("error:no_metadata").unwrap(), &err),
            "wrong slug"
        );
    }

    #[test]
    fn fields_mask_order() {
        let body = serde_json::json!({
            "title": "t", "description": null, "siteName": "s",
            "image": {"url": "x"}, "favicon": "f",
        });
        // description is null → '-'; the rest present.
        assert_eq!(fields_mask(&body), "T-SIF", "mask order TDSIF");
    }

    #[test]
    fn percentiles_nearest_rank() {
        let v: Vec<u64> = (1..=10).collect();
        assert_eq!(nearest_rank(&v, 50), Some(5), "p50 of 10");
        assert_eq!(nearest_rank(&v, 95), Some(10), "p95 of 10");
        assert_eq!(nearest_rank(&v, 100), Some(10), "p100 of 10");
        let one: Vec<u64> = vec![7];
        assert_eq!(nearest_rank(&one, 95), Some(7), "single value");
        assert_eq!(nearest_rank(&[], 50), None, "empty");
    }

    #[test]
    fn compare_ranking() {
        let err = rec(None, None, None);
        let empty_page = rec(Some("page"), Some("---S-"), None);
        let t_only = rec(Some("page"), Some("T----"), None);
        let ti = rec(Some("page"), Some("T--I-"), None);
        let tdi = rec(Some("page"), Some("TD-I-"), None);
        let media = Record {
            media_type: Some("image/png".to_owned()),
            ..rec(Some("media"), None, None)
        };
        assert_eq!(rank(&err), 0, "error lowest");
        assert_eq!(rank(&empty_page), 1, "page without T or I");
        assert_eq!(rank(&t_only), 2, "T only");
        assert_eq!(rank(&ti), 3, "T+I");
        assert_eq!(rank(&tdi), 4, "T+D+I");
        assert_eq!(rank(&media), 3, "media = T+I");
        assert!(rank(&tdi) > rank(&ti), "monotonic");
    }
}
