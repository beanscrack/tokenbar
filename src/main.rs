// TokenBar — tray + Discord Rich Presence token-usage meter.
// Sources: Hermes (state.db read-only), Claude Code (~/.claude/projects/*.jsonl),
// Codex CLI (~/.codex/sessions/**/rollout-*.jsonl).
// Every JSONL byte is parsed once; unchanged files are served from a persisted
// day-bucket cache (append-only tail tracking), so steady-state work is tiny.

#![windows_subsystem = "windows"]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::{json, Map, Value};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HINSTANCE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM,
    ERROR_ALREADY_EXISTS,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, RegisterClassExW, SetTimer,
    TranslateMessage, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, WM_CLOSE, WM_DESTROY,
    WM_QUIT, WM_TIMER, WNDCLASSEXW, CS_HREDRAW, CS_VREDRAW, WS_OVERLAPPED,
};
use std::sync::atomic::{AtomicBool, Ordering};

static PIPE_DEAD: AtomicBool = AtomicBool::new(false);
static PANEL_PORT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(53123);

// ============================================================ small utilities

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn carg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    // Howard Hinnant's algorithm
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn parse_iso(s: &str) -> Option<i64> {
    // "2026-11-11T05:27:31.123Z" | "...+02:00"
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |i: usize, w: usize| -> Option<i64> {
        let mut v: i64 = 0;
        for c in &b[i..i + w] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if b[10] != b'T' && b[10] != b' ' {
        return None;
    }
    let (h, mi, se) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let mut off = 0i64;
    if b.len() >= 25 && (b[19] == b'+' || b[19] == b'-') {
        let o = num(20, 2)? * 3600 + num(23, 2)? * 60;
        off = if b[19] == b'+' { o } else { -o };
    }
    match days_from_civil(y, mo, d).checked_mul(86400) {
        Some(x) => Some(x + h * 3600 + mi * 60 + se - off),
        None => None,
    }
}

fn parse_flex_ts(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => parse_iso(s),
        Value::Number(n) => n.as_f64().map(|f| {
            let a = f.abs();
            if a > 1e15 {
                (f / 1e6) as i64 // ns
            } else if a > 1e12 {
                (f / 1e3) as i64 // ms
            } else {
                f as i64 // s
            }
        }),
        _ => None,
    }
}

fn tz_offset_secs() -> i64 {
    unsafe {
        let mut tzi: windows_sys::Win32::System::Time::TIME_ZONE_INFORMATION = std::mem::zeroed();
        let status = windows_sys::Win32::System::Time::GetTimeZoneInformation(&mut tzi);
        // 2 == DAYLIGHT
        let bias = if status == 2 { tzi.DaylightBias } else { tzi.Bias };
        -(bias as i64) * 60
    }
}

fn u64g(m: &Map<String, Value>, k: &str) -> u64 {
    m.get(k).and_then(|v| v.as_u64()).unwrap_or(0)
}

fn compact(n: u64) -> String {
    const SUF: [(u64, &str); 5] = [
        (1_000_000_000_000, "T"),
        (1_000_000_000, "B"),
        (1_000_000, "M"),
        (1_000, "K"),
        (1, ""),
    ];
    for (d, s) in SUF {
        if n >= d {
            let v = n as f64 / d as f64;
            return if v >= 100.0 || d == 1 {
                format!("{}{}", v as u64, s)
            } else {
                format!("{:.2}{}", v, s)
            };
        }
    }
    n.to_string()
}

fn print_console(args: &[String], msg: &str) {
    if !has(args, "--once") && !has(args, "--verbose") {
        return;
    }
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h != INVALID_HANDLE_VALUE {
            let b = msg.as_bytes();
            let mut written: u32 = 0;
            WriteFile(h, b.as_ptr(), b.len() as u32, &mut written, std::ptr::null_mut());
        }
    }
}

fn app_dir() -> PathBuf {
    let la = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let d = PathBuf::from(la).join("tokenbar");
    fs::create_dir_all(&d).ok();
    d
}

fn log_line(msg: &str) {
    let d = app_dir();
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(d.join("app.log"))
    {
        let _ = writeln!(f, "{} {}", iso_now(), msg);
    }
}

fn iso_now() -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let day = t.div_euclid(86400);
    let sod = t.rem_euclid(86400);
    let z = day + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

// ============================================================ config

#[derive(Clone)]
struct Config {
    app_id: String,
    interval_s: u64,
    sources: [bool; 3], // [claude_code, codex, hermes]
    window_days: u32,
    verbose: bool,
    panel: bool,
    panel_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            app_id: String::new(),
            interval_s: 30,
            sources: [true; 3],
            window_days: 45,
            verbose: false,
            panel: true,
            panel_port: 53123,
        }
    }
}

fn load_config(path: &Path) -> Config {
    let mut cfg = Config::default();
    if let Ok(text) = fs::read_to_string(path) {
        for line in text.lines() {
            let l = line.trim();
            if let Some((k, v)) = l.split_once('=') {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                match k.trim() {
                    "app_id" => cfg.app_id = v.to_string(),
                    "interval_s" => cfg.interval_s = v.parse().unwrap_or(30),
                    "claude_code" => cfg.sources[0] = v == "true",
                    "codex" => cfg.sources[1] = v == "true",
                    "hermes" => cfg.sources[2] = v == "true",
                    "window_days" => cfg.window_days = v.parse().unwrap_or(45),
                    "verbose" => cfg.verbose = v == "true",
                    "panel" => cfg.panel = v == "true",
                    "panel_port" => cfg.panel_port = v.parse().unwrap_or(53123),
                    _ => {}
                }
            }
        }
    }
    cfg
}

fn save_config(dir: &Path, cfg: &Config) {
    let s = format!(
        "# TokenBar config\napp_id = \"{}\"\ninterval_s = {}\nclaude_code = {}\ncodex = {}\nhermes = {}\nwindow_days = {}\nverbose = {}\npanel = {}\npanel_port = {}\n",
        cfg.app_id, cfg.interval_s, cfg.sources[0], cfg.sources[1], cfg.sources[2],
        cfg.window_days, cfg.verbose, cfg.panel, cfg.panel_port
    );
    let _ = fs::write(dir.join("config.toml"), s);
}

// ============================================================ t3-style formatting
// Mirrors t3code formatTokens/formatUsd: K/M/B/T trimmed (0-2 decimals), $X.XX, word labels.

fn fmt_t3(n: u64) -> String {
    let (d, suf) = if n >= 1_000_000_000_000 {
        (1e12, "T")
    } else if n >= 1_000_000_000 {
        (1e9, "B")
    } else if n >= 1_000_000 {
        (1e6, "M")
    } else if n >= 1_000 {
        (1e3, "K")
    } else {
        return format!("{}", n);
    };
    let x = n as f64 / d;
    let dec = if x >= 100.0 {
        0
    } else if x >= 10.0 {
        1
    } else {
        2
    };
    let s = format!("{:.1$}", x, dec);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{}{}", s, suf)
}

fn fmt_usd(c: f64) -> String {
    format!("${:.2}", c)
}

// ============================================================ accumulators

const T: usize = 5; // in, out, cache_read, cache_creation, reasoning

#[derive(Clone, Default)]
struct Acc {
    v: [u64; T],
    cost: f64,
}

impl Acc {
    fn add(&mut self, o: &Acc) {
        for i in 0..T {
            self.v[i] += o.v[i];
        }
        self.cost += o.cost;
    }
    fn merge_max(&mut self, o: &Acc) {
        for i in 0..T {
            self.v[i] = self.v[i].max(o.v[i]);
        }
        self.cost = self.cost.max(o.cost);
    }
}

type ByDay = BTreeMap<u32, Acc>; // key 0 = older than retention window

fn acc_arr(a: &Acc) -> Vec<u64> {
    let mut v = a.v.to_vec();
    v.push((a.cost * 1e4) as u64);
    v
}

fn acc_from(v: &Value) -> Acc {
    let mut a = Acc::default();
    if let Some(arr) = v.as_array() {
        for i in 0..T.min(arr.len()) {
            a.v[i] = arr.get(i).and_then(|x| x.as_u64()).unwrap_or(0);
        }
        if arr.len() > T {
            a.cost = arr[T].as_u64().unwrap_or(0) as f64 / 1e4;
        }
    }
    a
}

#[derive(Default)]
struct FileEntry {
    size: u64,
    mtime: i64,
    offset: u64,
    days: ByDay,
}

type Cache = HashMap<PathBuf, FileEntry>;

#[derive(Clone, Default)]
struct CycleReport {
    scanned: usize,
    delta_bytes: u64,
    events: usize,
    duplicated: usize,
    parse_ms: u128,
}

#[derive(Default)]
struct State {
    cc: Cache,
    cx: Cache,
}

fn serialize_cache(c: &Cache) -> String {
    let mut root = Map::new();
    for (p, fe) in c {
        let mut keys = Vec::new();
        let mut vals = Vec::new();
        for (d, a) in &fe.days {
            keys.push(d);
            vals.push(json!(acc_arr(a)));
        }
        root.insert(
            p.to_string_lossy().to_string(),
            json!({
                "s": fe.size,
                "m": fe.mtime,
                "o": fe.offset,
                "k": keys,
                "v": vals,
            }),
        );
    }
    Value::Object(root).to_string()
}

fn parse_cache(text: &str) -> Cache {
    let mut c = Cache::new();
    let Ok(root) = serde_json::from_str::<Value>(text) else {
        return c;
    };
    let Some(obj) = root.as_object() else { return c };
    for (path, v) in obj {
        let Some(o) = v.as_object() else { continue };
        let mut fe = FileEntry {
            size: o.get("s").and_then(|x| x.as_u64()).unwrap_or(0),
            mtime: o.get("m").and_then(|x| x.as_i64()).unwrap_or(0),
            offset: o.get("o").and_then(|x| x.as_u64()).unwrap_or(0),
            days: ByDay::new(),
        };
        if let (Some(keys), Some(vals)) = (o.get("k").and_then(|x| x.as_array()), o.get("v").and_then(|x| x.as_array())) {
            for (k, v) in keys.iter().zip(vals.iter()) {
                if let Some(kk) = k.as_u64().map(|x| x as u32) {
                    fe.days.insert(kk, acc_from(v));
                }
            }
        }
        c.insert(PathBuf::from(path), fe);
    }
    c
}

// ============================================================ file helpers

fn file_sig(p: &Path) -> Option<(u64, i64)> {
    let md = fs::metadata(p).ok()?;
    let m = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((md.len(), m.as_secs() as i64))
}

/// Read bytes from `from` to EOF; commit only up to the last full line.
fn read_tail(path: &Path, from: u64) -> Option<(String, u64)> {
    let mut f = fs::File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    if size < from {
        return Some((String::new(), 0)); // rotated/truncated -> full reload
    }
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let last_nl = buf.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    let s = String::from_utf8_lossy(&buf[..last_nl]).into_owned();
    Some((s, from + last_nl as u64))
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>, prefix: &str) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_jsonl(&p, out, prefix);
        } else if let Some(n) = p.file_name().map(|n| n.to_string_lossy().to_string()) {
            if n.ends_with(".jsonl") && n.starts_with(prefix) {
                out.push(p);
            }
        }
    }
}

// ============================================================ Claude Code

fn cc_usage_acc(u: &Map<String, Value>) -> Acc {
    let thinking = u
        .get("output_tokens_details")
        .and_then(|v| v.as_object())
        .map(|m| u64g(m, "thinking_tokens"))
        .unwrap_or(0);
    Acc {
        v: [
            u64g(u, "input_tokens") + u64g(u, "cache_creation_input_tokens"),
            u64g(u, "output_tokens"),
            u64g(u, "cache_read_input_tokens"),
            u64g(u, "cache_creation_input_tokens"),
            thinking,
        ],
        cost: 0.0,
    }
}

#[derive(Default)]
struct FeedCtx {
    offset: i64,
    today: u32,
    retention: u32,
}

/// Accumulate a usage record into m; dedup by message id (streaming/sidechain
/// re-emissions carry identical usage under the same id).
fn feed_cc(map: &mut ByDay, ctx: &FeedCtx, seen: &mut HashSet<String>, rec: &Value, rep: &mut CycleReport) {
    let obj = match rec.as_object() {
        Some(o) => o,
        None => return,
    };
    if obj.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false) {
        return;
    }
    let fts = match obj.get("timestamp").and_then(parse_flex_ts) {
        Some(t) => t,
        None => return,
    };
    let obj = match obj.get("message").and_then(|x| x.as_object()) {
        Some(m) => m,
        None => return,
    };
    let usage = match obj.get("usage").and_then(|x| x.as_object()) {
        Some(u) => u,
        None => return,
    };
    let mid = obj.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if !mid.is_empty() {
        if !seen.insert(mid.clone()) {
            rep.duplicated += 1;
            return;
        }
    }
    let a = cc_usage_acc(usage);
    let d = ((fts + ctx.offset) / 86400) as i64;
    let key: u32 = if d >= ctx.retention as i64 && d <= ctx.today as i64 {
        d as u32
    } else {
        0
    };
    map.entry(key).or_default().add(&a);
    rep.events += 1;
}

fn scan_cc(
    path: &Path,
    fe: &mut FileEntry,
    ctx: &FeedCtx,
    rep: &mut CycleReport,
) {
    let (size, mtime) = match file_sig(path) {
        Some(v) => v,
        None => return,
    };
    let (mut text, mut newoff) = match read_tail(path, fe.offset) {
        Some(v) => v,
        None => return,
    };
    if newoff == 0 && fe.offset > 0 {
        // file truncated/rotated below our committed offset: full reload
        match read_tail(path, 0) {
            Some((t, n)) => {
                text = t;
                newoff = n;
            }
            None => return,
        }
        fe.days.clear();
    }
    if fe.offset == 0 {
        fe.days.clear();
    }
    fe.size = size;
    fe.mtime = mtime;
    rep.delta_bytes += newoff.saturating_sub(fe.offset);
    fe.offset = newoff;
    rep.scanned += 1;

    let mut seen: HashSet<String> = HashSet::new();
    for line in text.lines() {
        if line.contains("\"usage\"") {
            if let Ok(rec) = serde_json::from_str::<Value>(line) {
                feed_cc(&mut fe.days, ctx, &mut seen, &rec, rep);
            }
        }
    }
}

// ============================================================ Codex

fn cx_total_to_acc(t: &Map<String, Value>) -> Acc {
    let cached = u64g(t, "cached_input_tokens");
    let reasoning = u64g(t, "reasoning_output_tokens");
    Acc {
        v: [
            u64g(t, "input_tokens"),
            u64g(t, "output_tokens"),
            cached,
            cached,
            reasoning,
        ],
        cost: 0.0,
    }
}

#[derive(Default)]
struct ParsedCx {
    total: Option<Acc>,
    start_ts: i64,
    events: usize,
}

fn parse_codex_file(path: &Path) -> Option<ParsedCx> {
    let text = fs::read_to_string(path).ok()?;
    let mut out = ParsedCx::default();
    for line in text.lines() {
        if !line.contains("\"total_token_usage\"") {
            if line.contains("session_meta") && out.start_ts == 0 {
                if let Ok(rec) = serde_json::from_str::<Value>(line) {
                    if let Some(ts) = rec
                        .get("payload")
                        .and_then(|p| p.get("timestamp"))
                        .and_then(parse_flex_ts)
                    {
                        out.start_ts = ts;
                    }
                }
            }
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<Value>(line) {
            if rec.get("type").and_then(|x| x.as_str()) != Some("event_msg") {
                continue;
            }
            if let Some(t) = rec
                .get("payload")
                .and_then(|p| p.get("info"))
                .and_then(|i| i.get("total_token_usage"))
                .and_then(|x| x.as_object())
            {
                out.total = Some(cx_total_to_acc(t));
                out.events += 1;
                if out.start_ts == 0 {
                    out.start_ts = parse_flex_ts(rec.get("timestamp")?).unwrap_or(0);
                }
            }
        }
    }
    Some(out)
}

fn day_clamp(ts: i64, ctx: &FeedCtx) -> u32 {
    let raw = ((ts + ctx.offset) / 86400).clamp(0, ctx.today as i64) as u32;
    if raw >= ctx.retention {
        raw
    } else {
        0
    }
}

// ============================================================ Hermes

fn hermes_usage(ds: &mut ByDay, ctx: &FeedCtx, rep: &mut CycleReport) -> Result<usize, String> {
    let la = std::env::var("LOCALAPPDATA").map_err(|_| "no LOCALAPPDATA".to_string())?;
    let db = PathBuf::from(la).join("hermes").join("state.db");
    if !db.exists() {
        return Err(format!("missing db: {}", db.display()));
    }
    let con = Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("open: {}", e))?;
    con.busy_timeout(std::time::Duration::from_millis(3000)).ok();
    let mut st = con
        .prepare(
            "SELECT CAST(COALESCE(last_seen, first_seen, 0) AS INTEGER) ts,
                    CAST(COALESCE(input_tokens,0)  AS INTEGER),
                    CAST(COALESCE(output_tokens,0) AS INTEGER),
                    CAST(COALESCE(cache_read_tokens,0)  AS INTEGER),
                    CAST(COALESCE(cache_write_tokens,0) AS INTEGER),
                    CAST(COALESCE(reasoning_tokens,0)   AS INTEGER),
                    CAST(COALESCE(actual_cost_usd, estimated_cost_usd, 0) * 10000 AS INTEGER)
             FROM session_model_usage",
        )
        .map_err(|e| format!("prepare: {}", e))?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
                r.get::<_, i64>(4)? as u64,
                r.get::<_, i64>(5)? as u64,
                r.get::<_, i64>(6)? as f64 / 1e4,
            ))
        })
        .map_err(|e| format!("query: {}", e))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    let n = rows.len();
    for (ts, i, o, cr, cw, rz, cost) in rows {
        let d = (ts + ctx.offset) / 86400;
        let key: u32 = if d >= ctx.retention as i64 && d <= ctx.today as i64 {
            d as u32
        } else {
            0
        };
        let a = Acc {
            v: [i, o, cr, cw, rz],
            cost,
        };
        ds.entry(key).or_default().add(&a);
        rep.events += 1;
    }
    Ok(n)
}

// ============================================================ merge files into day buckets

fn merge_jsonl(
    files: &[PathBuf],
    cache: &mut Cache,
    ds: &mut ByDay,
    ctx: &FeedCtx,
    rep: &mut CycleReport,
    kind: Kind,
) {
    for f in files {
        let (size, mtime) = match file_sig(f) {
            Some(v) => v,
            None => continue,
        };
        let entry = cache.entry(f.clone()).or_default();
        if !(entry.size == size && entry.mtime == mtime && entry.offset > 0) {
            match kind {
                Kind::Cc => scan_cc(f, entry, ctx, rep),
                Kind::Cx => {
                    if let Some(p) = parse_codex_file(f) {
                        rep.scanned += 1;
                        rep.events += p.events;
                        if let Some(a) = &p.total {
                            rep.delta_bytes += size;
                            if !a.v.iter().any(|x| *x > 0) {
                                entry.days.clear();
                            } else {
                                let st = if p.start_ts > 0 { p.start_ts } else { mtime };
                                entry.days.clear();
                                entry.days.insert(day_clamp(st, ctx), a.clone());
                            }
                        }
                        entry.size = size;
                        entry.mtime = mtime;
                        entry.offset = 1;
                    }
                }
            }
        }
        for (d, a) in &entry.days {
            ds.entry(*d).or_default().add(a);
        }
    }
}

enum Kind {
    Cc,
    Cx,
}

// ============================================================ snapshots + totals

#[derive(Clone, Default)]
struct Snapshot {
    today: u32,
    sources: [ByDay; 3],
    report: CycleReport,
    hermes_status: String,
}

impl Snapshot {
    fn src_total(&self, i: usize) -> Acc {
        self.sources[i].values().fold(Acc::default(), |mut a, b| {
            a.add(b);
            a
        })
    }
    fn window(&self, i: usize, days: u32) -> Acc {
        let cutoff = self.today.saturating_sub(days);
        let mut a = Acc::default();
        for (d, acc) in self.sources[i].range(1..) {
            if *d > cutoff {
                a.add(acc);
            }
        }
        a
    }
    fn all_window(&self, days: u32) -> Acc {
        let mut a = Acc::default();
        for i in 0..3 {
            a.add(&self.window(i, days));
        }
        a
    }
    fn grand_total(&self) -> Acc {
        let mut a = Acc::default();
        for i in 0..3 {
            a.add(&self.src_total(i));
        }
        a
    }
}

fn presence_lines(s: &Snapshot) -> (String, String) {
    // t3-style: word labels + trimmed K/M/B/T, no arrows
    let t = win4_all(s, Some(1));
    let d30 = win4_all(s, Some(30));
    let mut details = if t.tokens() == 0 {
        "Today · no use yet".to_string()
    } else {
        let mut d = format!(
            "Today · {} in · {} cache read · {} out · {:.0}% hit",
            fmt_t3(t.inp),
            fmt_t3(t.cache_r),
            fmt_t3(t.out),
            t.hit() * 100.0
        );
        if t.cost > 0.005 {
            d.push_str(&format!(" · {}", fmt_usd(t.cost)));
        }
        d
    };
    details = details.chars().take(127).collect();

    let mut state = "30d · no use".to_string();
    if d30.tokens() > 0 {
        let names = ["Claude Code", "Codex", "Hermes"];
        let mut shares = Vec::new();
        for i in 0..3 {
            let w = win4(s, i, Some(30));
            let pct = if d30.tokens() > 0 {
                100.0 * w.tokens() as f64 / d30.tokens() as f64
            } else {
                0.0
            };
            shares.push(format!("{} {:.0}%", names[i], pct));
        }
        state = format!(
            "30d · {} in · {} out · {} · {}",
            fmt_t3(d30.inp),
            fmt_t3(d30.out),
            if d30.cost > 0.005 {
                fmt_usd(d30.cost)
            } else {
                format!("{:.0}% hit", d30.hit() * 100.0)
            },
            shares.join(" · ")
        );
    }
    state = state.chars().take(127).collect();
    (details, state)
}

// Category mapping per source (toluene slots): t3 labels
// Input / Cache read / Cache write / Output
struct Win4 {
    inp: u64,
    cache_r: u64,
    cache_w: u64,
    out: u64,
    cost: f64,
}
impl Win4 {
    fn tokens(&self) -> u64 {
        self.inp + self.cache_r + self.cache_w + self.out
    }
    fn hit(&self) -> f64 {
        let i = self.inp + self.cache_r + self.cache_w;
        if i == 0 {
            0.0
        } else {
            self.cache_r as f64 / i as f64
        }
    }
    fn is_zero(&self) -> bool {
        self.tokens() == 0
    }
}

fn w4(i: usize, a: &Acc) -> Win4 {
    match i {
        0 => Win4 {
            // claude code: v0 includes cache creation
            inp: a.v[0].saturating_sub(a.v[3]),
            cache_r: a.v[2],
            cache_w: a.v[3],
            out: a.v[1],
            cost: a.cost,
        },
        1 => Win4 {
            inp: a.v[0],
            cache_r: a.v[2],
            cache_w: 0,
            out: a.v[1],
            cost: a.cost,
        },
        _ => Win4 {
            inp: a.v[0],
            cache_r: a.v[2],
            cache_w: a.v[3],
            out: a.v[1],
            cost: a.cost,
        },
    }
}

fn win4(s: &Snapshot, i: usize, days: Option<u32>) -> Win4 {
    let a = match days {
        Some(d) => s.window(i, d),
        None => s.src_total(i),
    };
    w4(i, &a)
}

fn win4_all(s: &Snapshot, days: Option<u32>) -> Win4 {
    let mut t = Win4 {
        inp: 0,
        cache_r: 0,
        cache_w: 0,
        out: 0,
        cost: 0.0,
    };
    for i in 0..3 {
        let w = win4(s, i, days);
        t.inp += w.inp;
        t.cache_r += w.cache_r;
        t.cache_w += w.cache_w;
        t.out += w.out;
        t.cost += w.cost;
    }
    t
}

fn report_input(a: &Acc) -> u64 {
    a.v[0]
}

fn build_tooltip(s: &Snapshot) -> String {
    let names = ["Claude Code", "Codex", "Hermes"];
    let t = win4_all(s, Some(1));
    let gt = win4_all(s, None);
    let mut l = format!("TokenBar — today: {} in · {} out", fmt_t3(t.inp), fmt_t3(t.out));
    if t.cost > 0.005 {
        l.push_str(&format!(" · {}", fmt_usd(t.cost)));
    }
    l.push_str(&format!(" · hit {:.0}%", t.hit() * 100.0));
    l.push_str(" — all-time:");
    for i in 0..3 {
        let w = win4(s, i, None);
        let pct = if gt.tokens() > 0 {
            100.0 * w.tokens() as f64 / gt.tokens() as f64
        } else {
            0.0
        };
        l.push_str(&format!(" {} {} ({:.0}%)", names[i], fmt_t3(w.tokens()), pct));
    }
    l.chars().take(126).collect()
}

// ============================================================ collection

fn collect(cfg: &Config, st: &mut State) -> Snapshot {
    let t0 = Instant::now();
    let offset = tz_offset_secs();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let today = ((now + offset) / 86400) as u32;
    let retention = today.saturating_sub(cfg.window_days + 1);
    let ctx = FeedCtx {
        offset,
        today,
        retention,
    };
    let mut rep = CycleReport::default();
    let mut sources: [ByDay; 3] = Default::default();

    if cfg.sources[0] {
        let root = PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default())
            .join(".claude")
            .join("projects");
        let mut files = Vec::new();
        collect_jsonl(&root, &mut files, "");
        merge_jsonl(&files, &mut st.cc, &mut sources[0], &ctx, &mut rep, Kind::Cc);
        prune(&mut st.cc, &files, retention);
    }

    if cfg.sources[1] {
        let root = PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default())
            .join(".codex")
            .join("sessions");
        let mut files = Vec::new();
        collect_jsonl(&root, &mut files, "rollout-");
        merge_jsonl(&files, &mut st.cx, &mut sources[1], &ctx, &mut rep, Kind::Cx);
        prune(&mut st.cx, &files, retention);
    }

    let mut hs = String::from("ok");
    if cfg.sources[2] {
        let mut days = ByDay::new();
        match hermes_usage(&mut days, &ctx, &mut rep) {
            Ok(n) => {
                for (k, a) in days {
                    sources[2].entry(k).or_default().add(&a);
                }
                hs = format!("{} rows", n);
            }
            Err(e) => hs = e,
        }
    }

    rep.parse_ms = t0.elapsed().as_millis();
    Snapshot {
        today,
        sources,
        report: rep,
        hermes_status: hs,
    }
}

fn prune(cache: &mut Cache, keep: &[PathBuf], _retention: u32) {
    let keep: HashSet<PathBuf> = keep.iter().cloned().collect();
    cache.retain(|k, _| keep.contains(k));
}

fn load_state(dir: &Path, st: &mut State) {
    if let Ok(t) = fs::read_to_string(dir.join("state-cc.json")) {
        st.cc = parse_cache(&t);
    }
    if let Ok(t) = fs::read_to_string(dir.join("state-cx.json")) {
        st.cx = parse_cache(&t);
    }
}

fn save_state(dir: &Path, st: &State) {
    let _ = fs::write(dir.join("state-cc.json"), serialize_cache(&st.cc));
    let _ = fs::write(dir.join("state-cx.json"), serialize_cache(&st.cx));
}

// ============================================================ Discord IPC

struct Ipc {
    h: isize,
    buf: Vec<u8>,
    started: u64,
}

impl Ipc {
    fn connect(app_id: &str) -> Option<Ipc> {
        let pid = unsafe { GetCurrentProcessId() };
        for n in 0..5u32 {
            let name = format!("\\\\.\\pipe\\discord-ipc-{}", n);
            let wname = to_wide(&name);
            let h = unsafe {
                CreateFileW(
                    wname.as_ptr(),
                    0xC0000000,
                    0,
                    std::ptr::null_mut(),
                    3,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if h == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut ipc = Ipc {
                h: h as isize,
                buf: Vec::new(),
                started: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            };
            let hello = json!({"v": 1, "client_id": app_id, "process_id": pid});
            let bytes = serde_json::to_vec(&hello).unwrap();
            if ipc.send(0, &bytes) && ipc.expect_ready() {
                return Some(ipc);
            }
            ipc.close();
        }
        None
    }

    fn send(&mut self, op: u32, payload: &[u8]) -> bool {
        let mut frame = Vec::with_capacity(8 + payload.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload);
        let mut off = 0usize;
        while off < frame.len() {
            let mut n: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    self.h as HANDLE,
                    frame[off..].as_ptr(),
                    (frame.len() - off) as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || n == 0 {
                return false;
            }
            off += n as usize;
        }
        true
    }

    fn poll(&mut self) -> bool {
        let mut avail: u32 = 0;
        let ok = unsafe {
            PeekNamedPipe(
                self.h as HANDLE,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut avail,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            self.buf.clear();
            return false; // pipe broken
        }
        self.buf.resize(avail as usize, 0);
        if self.buf.is_empty() {
            return true;
        }
        let mut n: u32 = 0;
        unsafe {
            ReadFile(
                self.h as HANDLE,
                self.buf.as_mut_ptr(),
                self.buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            );
        }
        self.buf.truncate(n as usize);
        true
    }

    fn expect_ready(&mut self) -> bool {
        for _ in 0..20 {
            let _ = self.poll();
            let mut i = 0usize;
            while i + 8 <= self.buf.len() {
                let op = u32::from_le_bytes([
                    self.buf[i],
                    self.buf[i + 1],
                    self.buf[i + 2],
                    self.buf[i + 3],
                ]);
                let len = u32::from_le_bytes([
                    self.buf[i + 4],
                    self.buf[i + 5],
                    self.buf[i + 6],
                    self.buf[i + 7],
                ]) as usize;
                if i + 8 + len > self.buf.len() {
                    break;
                }
                if op == 1 {
                    self.buf.clear();
                    return true;
                }
                i += 8 + len;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    fn set_activity(&mut self, details: &str, state: &str) -> bool {
        let pid = unsafe { GetCurrentProcessId() };
        // SDK-shaped activity: name+type+timestamps+party render in mini popouts,
        // not just the full profile modal (triminimal payloads gate to modal only).
        let payload = json!({
            "cmd": "SET_ACTIVITY",
            "args": {"pid": pid, "activity": {
                "name": "TokenBar",
                "type": 0,
                "details": details,
                "state": state,
                "timestamps": {"start": self.started },
                "assets": {
                    "large_image": "app_icon",
                    "large_text": "TokenBar · all agents · local"
                },
                "party": {"id": "tokenbar-tray", "size": [1, 1]}
            }},
            "nonce": format!("{:016x}", nonce()),
        });
        self.send(1, payload.to_string().as_bytes())
    }

    /// Scan incoming frames for the SET_ACTIVITY response (proves Discord accepted it).
    fn acked(&mut self, ms: u64) -> bool {
        let t0 = Instant::now();
        loop {
            if !self.poll() {
                PIPE_DEAD.store(true, Ordering::Relaxed);
                return false;
            }
            let mut i = 0usize;
            while i + 8 <= self.buf.len() {
                let op = u32::from_le_bytes([
                    self.buf[i],
                    self.buf[i + 1],
                    self.buf[i + 2],
                    self.buf[i + 3],
                ]);
                let len = u32::from_le_bytes([
                    self.buf[i + 4],
                    self.buf[i + 5],
                    self.buf[i + 6],
                    self.buf[i + 7],
                ]) as usize;
                if i + 8 + len > self.buf.len() {
                    break;
                }
                if op == 1 {
                    let p = &self.buf[i + 8..i + 8 + len];
                    if p.windows(12).any(|w| w == b"SET_ACTIVITY") {
                        self.buf.clear();
                        return true;
                    }
                }
                i += 8 + len;
            }
            if t0.elapsed().as_millis() >= ms as u128 {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }

    fn clear_activity(&mut self) {
        let pid = unsafe { GetCurrentProcessId() };
        let payload = json!({
            "cmd": "SET_ACTIVITY",
            "args": {"pid": pid, "activity": null},
            "nonce": format!("{:016x}", nonce()),
        });
        self.send(1, payload.to_string().as_bytes());
    }

    fn close(&mut self) {
        if self.h != 0 {
            unsafe {
                CloseHandle(self.h as HANDLE);
            }
            self.h = 0;
        }
    }
}

impl Drop for Ipc {
    fn drop(&mut self) {
        self.close();
    }
}

fn nonce() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().write_u64(0x5EED_1234);
    RandomState::new().build_hasher().finish()
}

// ============================================================ print for --once

fn print_report(args: &[String], s: &Snapshot) {
    let r = &s.report;
    let mut out = String::new();
    out.push_str(&format!(
        "TokenBar — {} · sources: {} files, {} events (+{} dupes skipped), {} read, {} ms\n",
        iso_now(),
        r.scanned,
        r.events,
        r.duplicated,
        compact(r.delta_bytes),
        r.parse_ms
    ));
    out.push_str(&format!("Hermes: {}\n", s.hermes_status));
    out.push_str(&block_line(s, "Today", Some(1)));
    out.push_str(&block_line(s, "Last 7 days", Some(7)));
    out.push_str(&block_line(s, "Last 30 days", Some(30)));
    out.push_str(&block_line(s, "All time", None));
    let (det, st) = presence_lines(s);
    out.push_str(&format!("\nDiscord: {}\n         {}\n", det, st));
    print_console(args, &out);
}

fn block_line(s: &Snapshot, label: &str, days: Option<u32>) -> String {
    let names = ["Claude Code", "Codex", "Hermes"];
    let w = win4_all(s, days);
    let mut o = String::new();
    o.push_str(&format!(
        "\n== {} ==  {} tokens · cache hit {:.0}% · {}\n",
        label,
        fmt_t3(w.tokens()),
        w.hit() * 100.0,
        fmt_usd(w.cost)
    ));
    o.push_str(&format!(
        "  Input {} · Cache read {} · Cache write {} · Output {}\n",
        fmt_t3(w.inp),
        fmt_t3(w.cache_r),
        fmt_t3(w.cache_w),
        fmt_t3(w.out)
    ));
    for i in 0..3 {
        let sw = win4(s, i, days);
        if sw.tokens() == 0 {
            continue;
        }
        let share = if w.tokens() > 0 {
            100.0 * sw.tokens() as f64 / w.tokens() as f64
        } else {
            0.0
        };
        let filled = (share / 100.0 * 14.0).round() as usize;
        let bar = format!("{}{}", "▓".repeat(filled), "░".repeat(14 - filled));
        let mut row = format!(
            "  {:<12} {} {:>4.0}%  {} in · {} cache read",
            names[i],
            bar,
            share,
            fmt_t3(sw.inp),
            fmt_t3(sw.cache_r)
        );
        if sw.cache_w > 0 {
            row.push_str(&format!(" · {} cache write", fmt_t3(sw.cache_w)));
        }
        row.push_str(&format!(
            " · {} out · hit {:.0}%",
            fmt_t3(sw.out),
            sw.hit() * 100.0
        ));
        if sw.cost > 0.005 {
            row.push_str(&format!(" · {}", fmt_usd(sw.cost)));
        }
        o.push_str(&row);
        o.push('\n');
    }
    o
}

// ============================================================ main + window

extern "system" fn wndproc(hwnd: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match m {
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, m, w, l),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rc = run(args);
    unsafe {
        // detach any console attached by print_console (--once)
        windows_sys::Win32::System::Console::FreeConsole();
    }
    std::process::exit(rc);
}

fn last_present_note() {}

fn run(args: Vec<String>) -> i32 {
    let one_shot = args
        .iter()
        .any(|a| a == "--once" || a == "--json" || a == "--help" || a == "--version" || a == "--panel-only");
    if !one_shot {
        unsafe {
            CreateMutexW(
                std::ptr::null_mut(),
                1,
                to_wide("Local\\tokenbar-single-instance").as_ptr(),
            );
            if GetLastError() == ERROR_ALREADY_EXISTS {
                print_console(&args, "tokenbar: already running\n");
                return 0;
            }
        }
    }
    let adir = app_dir();
    let mut cfg = load_config(&adir.join("config.toml"));

    // headless dashboard server (test/HEADLESS); no tray, no mutex contention
    {
        let po = args
            .iter()
            .position(|a| a == "--panel-only")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<u16>().ok());
        if po.is_some() || has(&args, "--panel-only") {
            serve_panel(po.unwrap_or(cfg.panel_port));
            return 0;
        }
    }

    if let Some(v) = carg(&args, "--app-id") {
        cfg.app_id = v.trim().to_string();
        save_config(&adir, &cfg);
    }
    if let Some(v) = carg(&args, "--interval") {
        if let Ok(n) = v.parse() {
            cfg.interval_s = n;
            save_config(&adir, &cfg);
        }
    }
    for (flag, idx) in [("--claude", 0usize), ("--codex", 1), ("--hermes", 2)] {
        if let Some(v) = carg(&args, flag) {
            cfg.sources[idx] = v == "1" || v == "true";
            save_config(&adir, &cfg);
        }
    }
    if has(&args, "--verbose") {
        cfg.verbose = true;
        save_config(&adir, &cfg);
    }

    if has(&args, "--install") {
        return do_install(&args);
    }
    if has(&args, "--uninstall") {
        return do_uninstall(&args);
    }

    let once = has(&args, "--once");
    let mut st = State::default();
    load_state(&adir, &mut st);

    if once {
        let s = collect(&cfg, &mut st);
        save_state(&adir, &st);
        save_status(&adir, &s);
        print_report(&args, &s);
        log_line(&format!(
            "oneshot: scanned={} bytes={} ms={}",
            s.report.scanned,
            compact(s.report.delta_bytes),
            s.report.parse_ms
        ));
        return 0;
    }

    // ---- tray ----
    let menu = Menu::new();
    let dash_item = MenuItem::with_id("dash", "Open Dashboard", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit TokenBar", true, None);
    menu.append(&dash_item).ok();
    menu.append(&PredefinedMenuItem::separator()).ok();
    menu.append(&quit_item).ok();

    if cfg.panel {
        PANEL_PORT.store(cfg.panel_port as u32, Ordering::Relaxed);
        let port = cfg.panel_port;
        std::thread::spawn(move || serve_panel(port));
    }

    let mut tray: Option<TrayIcon>;
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(std::ptr::null());
        let cls = to_wide("TokenBarWnd");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: cls.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            0,
            cls.as_ptr(),
            to_wide("TokenBar").as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        tray = TrayIconBuilder::new()
            .with_id(tray_icon::TrayIconId::new("main"))
            .with_tooltip("TokenBar")
            .with_menu(Box::new(menu))
            .with_icon(make_icon())
            .build()
            .ok();
        SetTimer(hwnd, 1, (cfg.interval_s.max(10) * 1000) as u32, None);

        // ---- Discord ----
        let mut ipc: Option<Ipc> = None;
        let mut first = true;
        let mut first_disc = true;
        let mut stop = false;

        let mut msg: MSG = std::mem::zeroed();
        loop {
            drain_menu(&mut tray, &mut ipc, &adir, &cfg);
            let s = collect(&cfg, &mut st);
            save_state(&adir, &st);
            save_status(&adir, &s);
            let (det, state) = presence_lines(&s);
            if first && cfg.verbose {
                log_line(&format!(
                    "cycle: scanned={} bytes={} ms={} hermes={}",
                    s.report.scanned,
                    compact(s.report.delta_bytes),
                    s.report.parse_ms,
                    s.hermes_status
                ));
            }
            first = false;
            if let Some(tt) = &tray {
                tt.set_tooltip(Some(build_tooltip(&s))).ok();
            }
            if !cfg.app_id.is_empty() {
                if ipc.is_none() {
                    ipc = Ipc::connect(&cfg.app_id);
                    log_line(if ipc.is_some() { "discord: connected" } else { "discord: connect failed" });
                }
                if PIPE_DEAD.load(Ordering::Relaxed) {
                    PIPE_DEAD.store(false, Ordering::Relaxed);
                    if let Some(ipc) = ipc.as_mut() {
                        ipc.close();
                    }
                    ipc = None;
                    log_line("discord: server closed link, reconnecting");
                }
                let mut bad = false;
                if let Some(ipc) = ipc.as_mut() {
                    if !ipc.set_activity(&det, &state) {
                        ipc.close();
                        bad = true;
                    } else {
                        let ok = ipc.acked(800);
                        if first_disc {
                            log_line(if ok {
                                "discord: presence acked by client (live)"
                            } else {
                                "discord: presence sent, no ack within 800 ms"
                            });
                        } else if !ok {
                            log_line("discord: presence not acked this cycle");
                        }
                        first_disc = false;
                    }
                }
                if bad {
                    ipc = None;
                    log_line("discord: write failed, will reconnect");
                }
            }
            last_present_note();

            // Pump messages for one interval; drain the Discord pipe every wake
            // (idle RPC connections are closed by the server after ~15 s).
            let period = std::time::Duration::from_secs(cfg.interval_s.max(15));
            let t0 = Instant::now();
            while t0.elapsed() < period && !stop {
                MsgWaitForMultipleObjectsEx(
                    0,
                    std::ptr::null(),
                    2500,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    if msg.message == WM_QUIT {
                        stop = true;
                        break;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                drain_menu(&mut tray, &mut ipc, &adir, &cfg);
                let mut pmdead = false;
                if let Some(ipc) = ipc.as_mut() {
                    if !ipc.poll() {
                        ipc.close();
                        pmdead = true;
                    }
                }
                if pmdead {
                    ipc = None;
                    log_line("discord: pipe dead, will reconnect");
                }
            }
        }
        if let Some(ipc) = ipc.as_mut() {
            ipc.clear_activity();
            std::thread::sleep(std::time::Duration::from_millis(200));
            ipc.poll();
        }
    }
    if let Some(t) = tray {
        t.set_visible(false).ok();
    }
    0
}

fn drain_menu(tray: &mut Option<TrayIcon>, ipc: &mut Option<Ipc>, _adir: &Path, _cfg: &Config) {
    while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
        let _ = ev;
    }
    let mut quit = false;
    while let Ok(ev) = MenuEvent::receiver().try_recv() {
        let id = ev.id.0.as_str();
        if id == "quit" {
            quit = true;
        } else if id == "dash" {
            let port = PANEL_PORT.load(Ordering::Relaxed);
            open_url(&format!("http://127.0.0.1:{}/", port));
        }
    }
    if quit {
        if let Some(ipc) = ipc.as_mut() {
            ipc.clear_activity();
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        if let Some(t) = tray.take() {
            t.set_visible(false).ok();
        }
        unsafe {
            PostQuitMessage(0);
        }
    }
}

// ============================================================ status.json + panel (t3 style)

fn win_json(s: &Snapshot, days: Option<u32>) -> Value {
    let names = ["Claude Code", "Codex", "Hermes"];
    let w = win4_all(s, days);
    let mut by = Vec::new();
    for i in 0..3 {
        let sw = win4(s, i, days);
        by.push(json!({
            "name": names[i],
            "inp": sw.inp, "cr": sw.cache_r, "cw": sw.cache_w, "out": sw.out,
            "cost": (sw.cost * 100.0).round() / 100.0,
            "tok": sw.tokens(),
            "share": if w.tokens() > 0 { (1000.0 * sw.tokens() as f64 / w.tokens() as f64).round() / 10.0 } else { 0.0 },
            "hit": (100.0 * sw.hit()).round()
        }));
    }
    json!({
        "inp": w.inp, "cr": w.cache_r, "cw": w.cache_w, "out": w.out,
        "cost": (w.cost * 100.0).round() / 100.0,
        "tok": w.tokens(),
        "hit": (100.0 * w.hit()).round(),
        "by": by
    })
}

fn save_status(dir: &Path, s: &Snapshot) {
    let root = json!({
        "gen": iso_now(),
        "windows": {
            "today": win_json(s, Some(1)),
            "d7": win_json(s, Some(7)),
            "d30": win_json(s, Some(30)),
            "all": win_json(s, None)
        }
    });
    let _ = fs::write(dir.join("status.json"), root.to_string());
}

fn open_url(url: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            to_wide("open").as_ptr(),
            to_wide(url).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn serve_panel(port: u16) {
    use windows_sys::Win32::Networking::WinSock as w;
    unsafe {
        let mut wsa: w::WSADATA = std::mem::zeroed();
        if w::WSAStartup(0x0202, &mut wsa) != 0 {
            log_line("panel: WSAStartup failed");
            return;
        }
        let sock = w::socket(2, 1, 6);
        if sock == usize::MAX {
            log_line("panel: socket() failed");
            return;
        }
        let one: u32 = 1;
        w::setsockopt(sock, 0xffff, 4, &one as *const u32 as *const u8, 4);
        let mut addr: w::SOCKADDR_IN = std::mem::zeroed();
        addr.sin_family = 2;
        addr.sin_port = port.to_be();
        addr.sin_addr.S_un.S_addr = u32::from_ne_bytes([127, 0, 0, 1]);
        if w::bind(
            sock,
            &addr as *const w::SOCKADDR_IN as *const w::SOCKADDR,
            std::mem::size_of::<w::SOCKADDR_IN>() as i32,
        ) != 0
        {
            log_line(&format!(
                "panel: bind failed on port {} err={}",
                port,
                windows_sys::Win32::Foundation::GetLastError()
            ));
            w::closesocket(sock);
            return;
        }
        w::listen(sock, 16);
        log_line(&format!("panel: listening on 127.0.0.1:{}", port));
        loop {
            let client = w::accept(sock, std::ptr::null_mut(), std::ptr::null_mut());
            if client == usize::MAX {
                std::thread::sleep(std::time::Duration::from_millis(300));
                continue;
            }
            let to = 1500u32;
            w::setsockopt(client, 0xffff, 0x1007, &to as *const u32 as *const u8, 4);
            w::setsockopt(client, 0xffff, 0x1008, &to as *const u32 as *const u8, 4);
            let mut buf = [0u8; 2048];
            let n = w::recv(client, buf.as_mut_ptr(), buf.len() as i32, 0);
            let (status, body, ctype) = if n > 0 {
                let req = String::from_utf8_lossy(&buf[..n as usize]).to_string();
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("/")
                    .to_string();
                if path.starts_with("/status.json") {
                    (
                        200,
                        fs::read_to_string(app_dir().join("status.json"))
                            .unwrap_or_else(|_| "{}".into()),
                        "application/json",
                    )
                } else if path == "/" || path == "/index.html" {
                    (200, panel_html().to_string(), "text/html; charset=utf-8")
                } else {
                    (404, "not found".to_string(), "text/plain")
                }
            } else {
                (400, "bad request".to_string(), "text/plain")
            };
            let head = format!(
                "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
                status,
                ctype,
                body.len()
            );
            let mut msg = head.into_bytes();
            msg.extend_from_slice(body.as_bytes());
            w::send(client, msg.as_ptr(), msg.len() as i32, 0);
            w::closesocket(client);
        }
    }
}

fn panel_html() -> &'static str {
    r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>TokenBar</title>
<style>
:root{
  --bg:#111312; --card:#171a19; --line:#242827; --bd:#1f2322;
  --fg:#edeef0; --mut:#7a817f; --dim:#545a58;
  --c1:#34d399; --c2:#2dd4bf; --c3:#60a5fa; --c4:#fb923c;
}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.45 -apple-system,"Segoe UI",system-ui,sans-serif;display:flex;flex-direction:column;min-height:100vh}
.top{display:flex;align-items:baseline;gap:8px;padding:12px 20px;border-bottom:1px solid var(--bd);flex-wrap:wrap}
.nm{font-weight:600;font-size:15px;color:var(--dim)}
.tok{font-family:ui-monospace,Consolas,monospace;font-variant-numeric:tabular-nums}
.sub{color:var(--dim);font-size:12px;margin-left:auto}
.tabs{display:flex;gap:6px;padding:14px 20px 0;flex-wrap:wrap}
.tab{font:600 13px/1 inherit;color:var(--mut);background:transparent;border:1px solid transparent;border-radius:999px;padding:8px 14px;cursor:pointer}
.tab.on{color:var(--fg);border-color:var(--bd);background:var(--card)}
main{display:grid;grid-template-columns:1fr;gap:12px;padding:14px 20px 28px;width:100%;max-width:1200px;margin:0 auto;flex:1;align-content:start}
.sec{background:var(--card);border:1px solid var(--bd);border-radius:16px;padding:24px;min-height:180px}
.period{font-weight:600;font-size:13px;margin-bottom:18px}
.grid{display:grid;grid-template-columns:repeat(12,1fr);gap:24px}
.col6{grid-column:span 6}.col12{grid-column:span 12}
@media(max-width:760px){.col6{grid-column:span 12}}
.real{display:flex;flex-direction:column;gap:6px}
.rlab{font-size:14px;font-weight:600}
.rBs{display:flex;height:8px;border-radius:999px;overflow:hidden;background:var(--line);gap:2px}
.heat{min-width:2px;border-radius:999px}
.h1{background:var(--c1);flex-basis:55%}.h2{background:var(--c2);flex-basis:22%}
.h3{background:var(--c3);flex-basis:14%}.h4{background:var(--c4);flex-basis:9%}
.rNum{font-size:15px;font-family:ui-monospace,Consolas,monospace;font-variant-numeric:tabular-nums}
.rSup{color:var(--dim);font-size:12px;font-family:ui-monospace,Consolas,monospace}
.metric{display:flex;align-items:baseline;justify-content:space-between;gap:10px}
.val{font-size:28px;font-weight:600;font-family:ui-monospace,Consolas,monospace;font-variant-numeric:tabular-nums}
.rows{display:grid;grid-template-columns:1fr;gap:14px}
.row{display:grid;grid-template-columns:1fr;gap:5px}
.row span{font-size:13px;font-weight:600}
.row b{font-weight:400;color:var(--mut);font-family:ui-monospace,Consolas,monospace;font-size:12px}
.goal{height:6px;background:var(--line);border-radius:999px;overflow:hidden}
.goal i{display:block;height:100%;border-radius:999px;background:var(--mut)}
.ish0{--k:var(--c1)}.ish1{--k:var(--c2)}.ish2{--k:var(--c3)}
.sbar{height:3px;background:var(--line);border-radius:999px}
.sbar i{display:block;height:100%;border-radius:999px;background:var(--k)}
.legend{display:flex;gap:16px;margin-top:18px;font-size:12px;flex-wrap:wrap}
.legend i{width:10px;height:10px;border-radius:3px;display:inline-block;margin-right:6px}
.legend .token{color:var(--fg);font-family:ui-monospace,Consolas,monospace;margin-left:4px}
.empty{color:var(--dim)}
</style>
</head>
<body>
<div class="top">
<span class="nm">TokenBar</span>
<span class="tok" id="hTok"></span>
<span class="sub" id="hSub"></span>
</div>
<div class="tabs" id="tabs"></div>
<main><section class="sec" id="sec"></section></main>
<script>
const CATS=[["Input","inp","h1"],["Cache read","cr","h2"],["Cache write","cw","h3"],["Output","out","h4"]];
const W=[["Today","today"],["Last 7d","d7"],["Last 30d","d30"],["All time","all"]];
function compact(n){n=+n||0;const a=Math.abs(n);
 const f=(v,s)=>{let x=(v).toFixed(a>=1e11?0:a>=1e10?1:2);x=x.replace(/\.?0+$/,"");return x+s};
 if(a>=1e12)return f(n/1e12,"T");if(a>=1e9)return f(n/1e9,"B");
 if(a>=1e6)return f(n/1e6,"M");if(a>=1e3)return f(n/1e3,"K");
 return Math.round(n).toLocaleString("en-US");}const usd=(c)=>"$"+(+c||0).toFixed(2);
const pct=(x)=>(+x).toFixed(0).replace(/\.0+$/,"")+"%";
function real(w){
 const tot=Math.max(1,w.inp+w.cr+w.cw+w.out);
 const segs=CATS.map(([l,k,c])=>{const v=+w[k]||0;return v>0?`<i class="heat ${c}" style="flex-grow:${Math.max(1,Math.round(v/tot*1000))}" title="${l} ${compact(v)}"></i>`:""}).join("");
 return `<div class="col6"><div class="real"><div class="rlab">Realtime tokens</div><div class="rBs">${segs||'<i class="heat h1" style="flex-grow:1;opacity:.15"></i>'}</div><div class="rNum">${compact(w.tok)}</div><div class="rSup">${CATS.map(([l,k])=>l+" "+compact(w[k])).join(" \u00b7 ")} \u00b7 cache hit ${pct(w.hit)} \u00b7 ${usd(w.cost)}</div></div></div>`;
}
function block(w){
 return `<div class="col12"><div class="rows">${w.by.map((b,i)=>`<div class="row ish${i}"><div class="metric"><span>${b.name}</span><b>${compact(b.tok)} \u00b7 ${pct(b.share)}\n</b></div><div class="goal"><i style="width:${b.share}%"></i></div><div class="sbar"><i style="width:${b.share}%"></i></div></div>`).join("")}</div></div>`;
}
function render(sec,w,label){
 sec.innerHTML = `<div class="period">${label}</div><div class="grid">${real(w)}${block(w)}</div>
 <div class="legend">${CATS.map(([l,k,c])=>`<span><i class="${c}"></i>${l}<span class="token">${compact(w[k])}</span></span>`).join("")}</div>`;
}
let D=null,cur="today";
function draw(){
 if(!D||!D.windows)return;
 const w=D.windows[cur]||{tok:0,inp:0,cr:0,cw:0,out:0,cost:0,hit:0,by:[]};
 const g=D.windows.all;
 document.getElementById("hTok").textContent=compact(g.inp+g.cr+g.cw+g.out)+" Total tokens";
 document.getElementById("hSub").textContent="updated "+(D.gen||"");
 document.getElementById("tabs").innerHTML=W.map(([l,k])=>`<button class="tab ${k===cur?"on":""}" onclick="pick('${k}')">${l}</button>`).join("");
 const sec=document.getElementById("sec");
 const Wl=W.find(x=>x[1]===cur)[0];
 if(!w.tok){sec.innerHTML=`<div class="period">${Wl}</div><div class="empty">No token usage recorded for this window.</div>`;return}
 render(sec,w,Wl);
}
function pick(k){cur=k;draw()}
function tick(){fetch("/status.json",{cache:"no-store"}).then(r=>r.json()).then(d=>{D=d;draw()}).catch(()=>{})}
tick();setInterval(tick,30000);
</script>
</body>
</html>
"##
}

// ============================================================ install / uninstall

fn do_install(args: &[String]) -> i32 {
    let exe = std::env::current_exe().map_err(|_| ()).expect("exe");
    let dest = app_dir().join("tokenbar.exe");
    let run_path = dest.clone();
    if exe != dest {
        if let Err(e) = fs::copy(&exe, &dest) {
            print_console(args, &format!("tokenbar: copy failed: {}\n", e));
            return 1;
        }
    }
    let out = std::process::Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "TokenBar",
            "/t",
            "REG_SZ",
            "/d",
            &format!("\"{}\"", run_path.display()),
            "/f",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            print_console(
                args,
                &format!("tokenbar: installed to {} + autorun (HKCU Run)\n", dest.display()),
            );
            0
        }
        Ok(o) => {
            print_console(args, &format!("tokenbar: reg add failed: {}\n", String::from_utf8_lossy(&o.stderr)));
            1
        }
        Err(e) => {
            print_console(args, &format!("tokenbar: reg failed: {}\n", e));
            1
        }
    }
}

fn do_uninstall(args: &[String]) -> i32 {
    let _ = std::process::Command::new("reg")
        .args([
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "TokenBar",
            "/f",
        ])
        .output();
    print_console(args, "tokenbar: autorun removed\n");
    0
}

// ============================================================ tray icon (generated bars)

fn make_icon() -> Icon {
    let (w, h) = (32u32, 32u32);
    let mut img = vec![0u8; (w * h * 4) as usize];
    let bars: [(u32, u32, (u8, u8, u8)); 3] = [
        (6, 10, (235, 235, 245)),
        (13, 16, (150, 190, 255)),
        (20, 24, (120, 255, 170)),
    ];
    for y in 0..h {
        for x in 0..w {
            let mut px: Option<(u8, u8, u8, u8)> = None;
            for (bx, bh, col) in bars {
                let top = 28i32 - bh as i32;
                if x >= bx && x < bx + 5 && y as i32 >= top {
                    px = Some((col.0, col.1, col.2, 255));
                }
            }
            if let Some((r, g, b, a)) = px {
                let i = ((y * w + x) * 4) as usize;
                img[i] = r;
                img[i + 1] = g;
                img[i + 2] = b;
                img[i + 3] = a;
            }
        }
    }
    Icon::from_rgba(img, w, h).expect("icon")
}

// ============================================================ (unused helpers)
