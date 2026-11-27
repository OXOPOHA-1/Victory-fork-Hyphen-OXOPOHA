//! `victory report`: one run's log as a small text report that can be
//! shared.
//!
//! It reads the run's whole log (stderr, as `run-improved.ps1` and
//! `collect-ab.ps1` keep it in `stderr.txt`) and writes
//! what a first look needs: first what the layer did not do and why (the
//! end-of-run summary of `victory_core::causes`, or its periodic lines
//! when the run ended without one) and the start-up facts, then the host
//! device's facts and the `VICTORY_*`
//! variables set (the "host device facts" line), the render scale, the
//! frame times over single frames, the counts of dropped draws, refused
//! pipelines and descriptor tables, every warning and error grouped by its
//! message, and the first lines of any panic or crash. It copies no game
//! data: only counts and the layer's own message texts, and every path
//! under a user profile (and any `--scrub` path, such as the game's folder)
//! is replaced by a placeholder. The report changes no runtime behavior.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::ab;
use crate::logline::{clean, field, parse};

/// Replaces user-profile paths, this user's and machine's names in paths,
/// and the given folders, with placeholders.
pub(crate) struct Scrub {
    /// (text to find in ASCII lower case; placeholder)
    exact: Vec<(String, &'static str)>,
}

impl Scrub {
    pub(crate) fn new(extra: &[PathBuf]) -> Scrub {
        let mut exact = Vec::new();
        for p in extra {
            let text = p.to_string_lossy().trim_end_matches(['\\', '/']).to_owned();
            if text.len() >= 3 {
                exact.push((text.to_ascii_lowercase(), "<game folder>"));
                exact.push((text.replace('\\', "/").to_ascii_lowercase(), "<game folder>"));
            }
        }
        if let Some(home) = std::env::var_os("USERPROFILE").filter(|h| h.len() >= 3) {
            let home = home.to_string_lossy().to_ascii_lowercase();
            exact.push((home.replace('\\', "/"), "<user profile>"));
            exact.push((home, "<user profile>"));
        }
        if let Some(machine) = std::env::var_os("COMPUTERNAME").filter(|m| m.len() >= 3) {
            exact.push((machine.to_string_lossy().to_ascii_lowercase(), "<machine>"));
        }
        // The longest first, so a folder inside the profile is named as the folder.
        exact.sort_by_key(|(text, _)| std::cmp::Reverse(text.len()));
        Scrub { exact }
    }

    pub(crate) fn line(&self, line: &str) -> String {
        let mut out = line.to_owned();
        for (find, with) in &self.exact {
            out = replace_caseless(&out, find, with);
        }
        users(&out)
    }
}

/// `text` with every case-insensitive (ASCII) occurrence of `find` (lower
/// case) replaced.
fn replace_caseless(text: &str, find: &str, with: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if find.is_empty() || !lower.contains(find) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(i) = lower[at..].find(find) {
        out.push_str(&text[at..at + i]);
        out.push_str(with);
        at += i + find.len();
    }
    out.push_str(&text[at..]);
    out
}

/// `text` with `X:\Users\<name>` (either slash, doubled backslashes as in
/// quoted fields, any case) replaced by `<user profile>`.
fn users(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    let mut from = 0;
    while let Some(i) = lower[from..].find("users") {
        let i = from + i;
        from = i + 5;
        // Before "users": one or more slashes, then a drive "X:".
        let mut s = i;
        while s > 0 && matches!(bytes[s - 1], b'\\' | b'/') {
            s -= 1;
        }
        if s == i || s < 2 || bytes[s - 1] != b':' || !bytes[s - 2].is_ascii_alphabetic() || s - 2 < at {
            continue;
        }
        // After it: slashes, then the name up to the next slash, quote or space.
        let mut e = i + 5;
        let slashes = e;
        while e < bytes.len() && matches!(bytes[e], b'\\' | b'/') {
            e += 1;
        }
        if e == slashes {
            continue;
        }
        // A name may hold spaces ("Иван Петров"): a space ends it only
        // before a field (` name=`) or at the end of the line.
        while e < bytes.len() && !matches!(bytes[e], b'\\' | b'/' | b'"' | b'\'' | b';' | b',') {
            if bytes[e] == b' ' && message(&text[e..]).is_empty() {
                break;
            }
            e += 1;
        }
        out.push_str(&text[at..s - 2]);
        out.push_str("<user profile>");
        at = e;
        from = e;
    }
    out.push_str(&text[at..]);
    out
}

/// The message of a log line's `rest`: the text before its first field
/// (` name=` with a lower-case name).
fn message(rest: &str) -> &str {
    let bytes = rest.as_bytes();
    let mut i = 0;
    while let Some(at) = rest[i..].find(' ') {
        let start = i + at + 1;
        let mut e = start;
        while e < bytes.len() && (bytes[e].is_ascii_lowercase() || bytes[e].is_ascii_digit() || bytes[e] == b'_') {
            e += 1;
        }
        if e > start && e < bytes.len() && bytes[e] == b'=' && bytes[start].is_ascii_lowercase() {
            return &rest[..start - 1];
        }
        i = start;
    }
    rest
}

/// The number just before `tail` in `text` (`"12 asked more"` -> 12).
fn number_before(text: &str, tail: &str) -> Option<f64> {
    let before = &text[..text.find(tail)?];
    before[before.rfind(' ').map_or(0, |at| at + 1)..].parse().ok()
}

/// What one log says.
#[derive(Debug, Default)]
struct Facts {
    lines: usize,
    log_lines: usize,
    first_time: Option<String>,
    last_time: Option<String>,
    device: Option<String>,
    device_ready: bool,
    no_device: Option<String>,
    render_scale: Option<String>,
    /// (message, level) -> (count, first line, scrubbed)
    by_message: BTreeMap<(String, String), (usize, String)>,
    /// Guest draws and dispatches, and those run on the host, summed over
    /// the "frame presented" lines.
    presented: [u64; 4],
    late_drops: u64,
    late_lines: usize,
    /// Tables refused per frame: the readings, the readings with any, the most.
    tables: (usize, usize, f64),
    pipelines_refused: usize,
    not_implemented: usize,
    /// The first panic or crash and the lines after it.
    crash: Vec<String>,
    crash_more: usize,
    /// What was not done, by cause, from the end-of-run summary.
    causes: Vec<CauseRow>,
    /// The summary's header was found (it may say nothing was counted).
    summary: bool,
    /// Without a summary: each cause's total from the periodic lines, with
    /// the frames of the first and last line that named it.
    periodic: BTreeMap<String, (u64, u64, u64)>,
    /// The last frame a periodic cause line was written in.
    periodic_frame: u64,
    /// The "start-up:" and "start-up (later):" lines, in order.
    start_up: Vec<String>,
    /// Compute translations refused for wave size (the line is cumulative).
    wave_refused: Option<u64>,
    /// Summed over the "mesh launch over" lines: draws through the
    /// amplification stage, through the clamp pass; records clamped, left
    /// empty by the budget, charged out of turn; and the number of lines.
    mesh_launch: [u64; 6],
}

/// One row of the end-of-run summary.
#[derive(Debug, Clone, PartialEq)]
struct CauseRow {
    name: String,
    count: u64,
    first_frame: u64,
    last_frame: u64,
    area: String,
    what: String,
}

/// A summary row (`not done: <name> count=.. first_frame=.. last_frame=..
/// area=".." what=".."`), as `victory_core::causes::summary_row` writes it.
fn cause_row(rest: &str) -> Option<CauseRow> {
    let tail = rest.strip_prefix(victory_core::causes::ROW_HEAD)?.trim_start();
    let name = tail.split(' ').next()?.to_owned();
    let number = |key: &str| field(rest, key).and_then(|v| v.parse::<u64>().ok());
    Some(CauseRow {
        name,
        count: number("count")?,
        first_frame: number("first_frame").unwrap_or(0),
        last_frame: number("last_frame").unwrap_or(0),
        area: field(rest, "area").unwrap_or_default(),
        what: field(rest, "what").unwrap_or_default(),
    })
}

/// A periodic cause line (`not done in the last 5 s, by cause (frame F):
/// name +d (total t), ...`): the frame and each (name, total).
fn periodic_causes(rest: &str) -> Option<(u64, Vec<(String, u64)>)> {
    let tail = rest.strip_prefix("not done in the last ")?;
    let (head, list) = tail.split_once("): ")?;
    let frame = head.rsplit_once("(frame ")?.1.parse().ok()?;
    let mut out = Vec::new();
    for item in list.split(", ") {
        let Some((name, rest)) = item.split_once(' ') else { continue };
        let Some(total) = rest.split_once("(total ").and_then(|(_, t)| t.trim_end_matches(')').parse().ok()) else {
            continue;
        };
        out.push((name.to_owned(), total));
    }
    Some((frame, out))
}

const CRASH_LINES: usize = 12;

fn read(lines: &[String], scrub: &Scrub) -> Facts {
    let mut f = Facts { lines: lines.len(), ..Facts::default() };
    let mut crash_left = 0;
    for raw in lines {
        if crash_left > 0 {
            f.crash.push(scrub.line(raw));
            crash_left -= 1;
        }
        let crashed = raw.contains("panicked at")
            || raw.contains("VIctory: panic on thread")
            || raw.contains("unhandled exception")
            || raw.contains("VIctory: host GPU failure");
        if crashed {
            if f.crash.is_empty() {
                f.crash.push(scrub.line(raw));
                crash_left = CRASH_LINES;
            } else if crash_left == 0 {
                f.crash_more += 1;
            }
        }
        let Some(line) = parse(raw) else { continue };
        f.log_lines += 1;
        if f.first_time.is_none() {
            f.first_time = Some(line.time.to_owned());
        }
        f.last_time = Some(line.time.to_owned());
        let rest = line.rest;
        if let Some(row) = cause_row(rest) {
            f.causes.push(row);
        } else if rest.starts_with(victory_core::causes::SUMMARY_HEAD) {
            f.summary = true;
        } else if let Some((frame, list)) = periodic_causes(rest) {
            f.periodic_frame = frame;
            for (name, total) in list {
                let e = f.periodic.entry(name).or_insert((0, frame, frame));
                e.0 = e.0.max(total);
                e.2 = frame;
            }
        } else if rest.starts_with("start-up: ") || rest.starts_with("start-up (later): ") {
            f.start_up.push(scrub.line(rest));
        } else if rest.starts_with("mesh launch over ") {
            let tails = [
                " indirect mesh draws launched through",
                " through the clamp pass",
                " records clamped",
                " left empty by the list's budget",
                " charged out of turn",
            ];
            for (i, tail) in tails.iter().enumerate() {
                f.mesh_launch[i] += number_before(rest, tail).unwrap_or(0.0) as u64;
            }
            f.mesh_launch[5] += 1;
        } else if let Some(tail) = rest.strip_prefix("translations refused for wave size: ")
            && let Some(n) = tail.split(' ').next().and_then(|t| t.parse::<u64>().ok())
        {
            // Cumulative: the largest is the run's.
            f.wave_refused = Some(f.wave_refused.unwrap_or(0).max(n));
        }
        if rest.starts_with("host device facts") {
            f.device.get_or_insert_with(|| scrub.line(rest));
        } else if rest.starts_with("host D3D12 device ready") {
            f.device_ready = true;
        } else if rest.starts_with("NO HOST D3D12 DEVICE") || rest.starts_with("no host D3D12 device") {
            f.no_device.get_or_insert_with(|| scrub.line(rest));
        } else if rest.starts_with("render scale: ") {
            let scale = field(rest, "scale").unwrap_or_else(|| "?".into());
            let source = field(rest, "source").unwrap_or_else(|| "?".into());
            f.render_scale.get_or_insert_with(|| format!("{scale}% (from {source})"));
        } else if rest.starts_with("frame presented") {
            for (i, key) in ["draws", "drawn", "dispatches", "dispatched"].iter().enumerate() {
                f.presented[i] += field(rest, key).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            }
        } else if rest.starts_with("draws and dispatches dropped because their host pipeline was not made yet") {
            // The count follows the parenthesis that names the switch.
            if let Some(n) = rest.rsplit_once("): ").and_then(|(_, t)| t.split(' ').next()?.parse::<u64>().ok()) {
                f.late_drops += n;
            }
            f.late_lines += 1;
        } else if rest.starts_with("dynamic slots per frame, tables refused by cause: ") {
            let refused = ["asked more slots", " had room but", " were searched for"]
                .iter()
                .filter_map(|tail| number_before(rest, &format!(" {}", tail.trim_start())))
                .sum::<f64>();
            f.tables.0 += 1;
            if refused > 0.0 {
                f.tables.1 += 1;
            }
            f.tables.2 = f.tables.2.max(refused);
        }
        let text = message(rest);
        if text.starts_with("host pipeline refused")
            || text.starts_with("host root signature refused")
            || text.starts_with("host mesh pipeline refused")
        {
            f.pipelines_refused += 1;
        }
        if text.starts_with("a pipeline creation method that is not implemented") {
            f.not_implemented += 1;
        }
        if line.level == "WARN" || line.level == "ERROR" {
            let key = (line.level.to_owned(), format!("{}: {}", line.target, scrub.line(&cut(text, 240))));
            f.by_message.entry(key).or_insert((0, scrub.line(&cut(rest, 400)))).0 += 1;
        }
    }
    f
}

/// `text` cut to at most `most` characters.
fn cut(text: &str, most: usize) -> String {
    match text.char_indices().nth(most) {
        Some((at, _)) => format!("{} ...", &text[..at]),
        None => text.to_owned(),
    }
}

fn frames_section(out: &mut String, lines: &[String]) {
    let readings = ab::readings(lines);
    if readings.is_empty() {
        let _ = writeln!(out, "No \"frame times\" lines: the run was made without VICTORY_FRAME_LOG=1, or ended before five seconds of frames.");
        return;
    }
    let all = ab::count(&readings, 0, None);
    let s = ab::summarise(&all.ms);
    let _ = writeln!(out, "Single frames from the frame log's \"each\" lists (whole ms as logged), over the whole run, menus and loading included:");
    let _ = writeln!(out, "  frames {}, in {} five-second readings", all.ms.len(), readings.len());
    let _ = writeln!(out, "  p50 {:.0} ms, p95 {:.0} ms, p99 {:.0} ms (nearest rank), longest {:.0} ms, mean {:.1} ms", s.p50, s.p95, s.p99, s.max, s.mean);
    let over = |ms: f64| all.ms.iter().filter(|&&v| v > ms).count();
    let _ = writeln!(out, "  frames over 50 ms: {}, over 100 ms: {}, over 500 ms: {}", over(50.0), over(100.0), over(500.0));
    let with_draws = readings.iter().filter(|r| r.draws.is_some()).count();
    if with_draws > 0 {
        let heavy = ab::count(&readings, 3000, None);
        if !heavy.ms.is_empty() {
            let h = ab::summarise(&heavy.ms);
            let _ = writeln!(
                out,
                "  frames with at least 3000 host draws: {}, p50 {:.0} ms, p95 {:.0} ms, longest {:.0} ms, mean {:.1} ms",
                heavy.ms.len(),
                h.p50,
                h.p95,
                h.max,
                h.mean
            );
        } else {
            let _ = writeln!(out, "  no frame had 3000 host draws or more");
        }
    } else {
        let _ = writeln!(out, "  (no \"frame draws\" lines: a log from an older build; frames cannot be split by draws)");
    }
}

/// The first section: what the layer did not do, most frequent first, in
/// plain words, then the start-up facts.
fn causes_section(out: &mut String, f: &Facts) {
    let _ = writeln!(out, "== What did not work in this run, and why");
    let rows: Vec<CauseRow> = if f.summary || !f.causes.is_empty() {
        let _ = writeln!(out, "  (from the end-of-run summary; counts are occurrences, frames are frames presented)");
        let mut rows = f.causes.clone();
        rows.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
        rows
    } else if !f.periodic.is_empty() {
        let _ = writeln!(
            out,
            "  (the run ended without its summary: totals from the periodic lines, up to frame {}; frames are those of the lines)",
            f.periodic_frame
        );
        let mut rows: Vec<CauseRow> = f
            .periodic
            .iter()
            .map(|(name, &(count, first, last))| {
                let info = victory_core::causes::Cause::ALL.iter().map(|c| c.info()).find(|i| i.name == name);
                CauseRow {
                    name: name.clone(),
                    count,
                    first_frame: first,
                    last_frame: last,
                    area: info.map(|i| i.area.name().to_owned()).unwrap_or_default(),
                    what: info.map(|i| i.what.to_owned()).unwrap_or_else(|| "(a cause this build of the report does not know)".into()),
                }
            })
            .collect();
        rows.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
        rows
    } else {
        let _ = writeln!(out, "  no cause lines in this log (a build from before them, or a log cut before the first 5 seconds)");
        Vec::new()
    };
    if rows.is_empty() && (f.summary || !f.periodic.is_empty()) {
        let _ = writeln!(out, "  nothing was counted: the layer did everything the title asked that it counts");
    }
    for r in &rows {
        let frames = if r.first_frame == r.last_frame {
            format!("frame {}", r.first_frame)
        } else {
            format!("frames {} to {}", r.first_frame, r.last_frame)
        };
        let _ = writeln!(out, "  {:>9}x  {} ({frames}; {}; {})", r.count, r.what, r.area, r.name);
    }
    if let Some(n) = f.wave_refused {
        let _ = writeln!(
            out,
            "  translations refused for wave size: {n} (compute shaders needing a wave size this card lacks; see the \"host wave lanes\" line)"
        );
    }
    if let [amplified, clamp_pass, clamped, emptied, late, lines] = f.mesh_launch
        && lines > 0
    {
        let _ = writeln!(
            out,
            "  mesh launch ({lines} \"mesh launch over\" lines): {amplified} indirect mesh draws through the amplification stage, {clamp_pass} through the clamp pass; on the card {clamped} records clamped, {emptied} left empty by the list's budget, {late} charged out of turn (0: in list order, as the clamp pass charges)"
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "== Start-up facts");
    if f.start_up.is_empty() {
        let _ = writeln!(out, "  no \"start-up:\" lines (a build from before them, or no host device was made)");
    }
    for l in &f.start_up {
        let _ = writeln!(out, "  {}", l.strip_prefix("start-up: ").unwrap_or(l));
    }
}

/// Builds the report of log `path`.
fn build(path: &Path, lines: &[String], scrub: &Scrub) -> String {
    let f = read(lines, scrub);
    let mut out = String::new();
    let name = path.file_name().map_or_else(|| "?".into(), |n| n.to_string_lossy().into_owned());
    let _ = writeln!(out, "VIctory run report (made by `victory report` from the run's log; no game data, user paths replaced)");
    let _ = writeln!(out, "log file: {}", scrub.line(&name));
    let _ = writeln!(
        out,
        "lines: {} ({} log lines), from {} to {}",
        f.lines,
        f.log_lines,
        f.first_time.as_deref().unwrap_or("?"),
        f.last_time.as_deref().unwrap_or("?")
    );
    let _ = writeln!(out);
    causes_section(&mut out, &f);
    let _ = writeln!(out);
    let _ = writeln!(out, "== Host device");
    match (&f.device, &f.no_device) {
        (_, Some(no)) => {
            let _ = writeln!(out, "  FAILED: {no}");
        }
        (Some(facts), None) => {
            for key in [
                "adapter",
                "vendor",
                "device",
                "driver",
                "vram_mb",
                "budget_mb",
                "usage_mb",
                "feature_level",
                "shader_model",
                "binding_tier",
                "tiled_tier",
                "mesh_tier",
                "enhanced_barriers",
                "gpu_upload_heaps",
                "wave_min",
                "wave_max",
                "render_scale",
            ] {
                let _ = writeln!(out, "  {key:<18} {}", field(facts, key).unwrap_or_else(|| "?".into()));
            }
        }
        (None, None) if f.device_ready => {
            let _ = writeln!(out, "  made, but no \"host device facts\" line (a build from before that line)");
        }
        (None, None) => {
            let _ = writeln!(out, "  no line about the host device: the title never made one, or the log is cut");
        }
    }
    let _ = writeln!(out, "  render scale       {}", f.render_scale.as_deref().unwrap_or("no line (not logged by this build or not reached)"));
    let _ = writeln!(out);
    let _ = writeln!(out, "== VICTORY_ variables set (from the device line)");
    match f.device.as_deref().and_then(|d| field(d, "switches")) {
        Some(s) if s.is_empty() => {
            let _ = writeln!(out, "  none");
        }
        Some(s) => {
            // Values never hold a space but paths may: split before each name.
            let mut names: Vec<String> = Vec::new();
            for word in s.split(' ') {
                match names.last_mut() {
                    Some(last) if !word.starts_with("VICTORY_") => {
                        last.push(' ');
                        last.push_str(word);
                    }
                    _ => names.push(word.to_owned()),
                }
            }
            for n in names {
                let _ = writeln!(out, "  {n}");
            }
        }
        None => {
            let _ = writeln!(out, "  unknown (no device line)");
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "== Frames");
    frames_section(&mut out, lines);
    let _ = writeln!(out);
    let _ = writeln!(out, "== Work left out");
    let [draws, drawn, dispatches, dispatched] = f.presented;
    let _ = writeln!(
        out,
        "  \"frame presented\" lines (at frames 1, 2, 4, 8, ...): guest draws {draws}, run on the host {drawn}; guest dispatches {dispatches}, run on the host {dispatched}"
    );
    let _ = writeln!(
        out,
        "  (the host counts include work of the layer's own, so the difference is a hint, not a count of dropped draws)"
    );
    if f.late_lines > 0 {
        let _ = writeln!(out, "  draws and dispatches dropped because their pipeline was not made yet: {} (from {} lines)", f.late_drops, f.late_lines);
    } else {
        let _ = writeln!(out, "  draws dropped because their pipeline was not made yet: no line (none, or a build that does not log it)");
    }
    let _ = writeln!(out, "  host pipelines or root signatures refused: {} warnings", f.pipelines_refused);
    let _ = writeln!(out, "  pipelines asked for through creation methods not implemented: {} warnings (only the first calls are logged)", f.not_implemented);
    if f.tables.0 > 0 {
        let _ = writeln!(
            out,
            "  descriptor tables refused: {} of {} readings had some; the most in one reading: {:.0} per frame",
            f.tables.1,
            f.tables.0,
            f.tables.2
        );
    } else {
        let _ = writeln!(out, "  descriptor tables refused: no line (logged only by some builds)");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "== Panics and crashes");
    if f.crash.is_empty() {
        let _ = writeln!(out, "  none in the log");
    } else {
        for l in &f.crash {
            let _ = writeln!(out, "  {}", cut(l, 400));
        }
        if f.crash_more > 0 {
            let _ = writeln!(out, "  ({} more panic or crash lines later in the log)", f.crash_more);
        }
    }
    let _ = writeln!(out);
    let (errors, warnings): (Vec<_>, Vec<_>) = f.by_message.iter().partition(|((level, _), _)| level == "ERROR");
    for (title, list) in [("Errors", errors), ("Warnings", warnings)] {
        let total: usize = list.iter().map(|(_, (n, _))| n).sum();
        let _ = writeln!(out, "== {title} by message: {total} lines, {} different messages, most frequent first", list.len());
        let mut list = list;
        list.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
        for ((_, text), (n, first)) in list.iter().take(60) {
            let _ = writeln!(out, "  {n:>7}  {text}");
            if first.len() > text.len() + 2 {
                let _ = writeln!(out, "           first: {first}");
            }
        }
        if list.len() > 60 {
            let _ = writeln!(out, "  ... {} more messages", list.len() - 60);
        }
        let _ = writeln!(out);
    }
    out
}

/// `victory report <log> [--out <dir>] [--scrub <path>...]`.
pub fn run(log: &Path, out: Option<&Path>, scrub: &[PathBuf]) -> Result<()> {
    let bytes = std::fs::read(log).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", log.display()))?;
    let lines: Vec<String> = String::from_utf8_lossy(&bytes).lines().map(|l| clean(l).into_owned()).collect();
    let text = build(log, &lines, &Scrub::new(scrub));
    match out {
        Some(dir) => {
            victory_core::ensure_outside_repo(dir)?;
            std::fs::create_dir_all(dir)?;
            let file = dir.join("report.txt");
            std::fs::write(&file, &text)?;
            print!("{text}");
            println!("written to {}", file.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Scrub {
        Scrub { exact: Vec::new() }
    }

    const FACTS: &str = r#"2026-11-26T04:32:09.300000Z  INFO victory_gpu::host: host device facts (adapter, driver, the card's memory in MB, features, the VICTORY_ variables set, the render scale in percent) adapter="Intel(R) Arc(TM) B580 Graphics" vendor=0x8086 device=0xe20b driver=32.0.101.6299 vram_mb=12086 budget_mb=11400 usage_mb=120 feature_level=12_2 shader_model=6_8 binding_tier=3 tiled_tier=4 mesh_tier=1 enhanced_barriers=true gpu_upload_heaps=true wave_min=16 wave_max=32 render_scale=100 switches="VICTORY_FRAME_LOG=1 VICTORY_RAW_SAVE=C:\\Users\\someone\\raw dir VICTORY_INPLACE_PROBE=1""#;

    fn lines(list: &[&str]) -> Vec<String> {
        list.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn user_paths_are_replaced() {
        assert_eq!(users(r"path=C:\Users\someone\AppData\Local\victory"), r"path=<user profile>\AppData\Local\victory");
        assert_eq!(users(r#"x="d:\\users\\Иван Петров\\x" next=1"#), r#"x="<user profile>\\x" next=1"#);
        assert_eq!(users(r"at C:\Users\Иван Петров next=1"), r"at <user profile> next=1");
        assert_eq!(users(r"at C:\Users\Иван Петров"), r"at <user profile>");
        assert_eq!(users("C:/Users/a/b and users of it"), "<user profile>/b and users of it");
        assert_eq!(users("no profile here: /users/x"), "no profile here: /users/x");
        let scrub = Scrub { exact: vec![(r"e:\games\the title".into(), "<game folder>")] };
        assert_eq!(scrub.line(r"opened E:\Games\The Title\data.pak"), r"opened <game folder>\data.pak");
    }

    #[test]
    fn a_message_ends_where_its_fields_begin() {
        assert_eq!(message("host wave lanes min=16 max=32"), "host wave lanes");
        assert_eq!(
            message("GPU upload heaps (the card's memory; VICTORY_WINDOW_UPLOAD=1 puts windows there) supported=true"),
            "GPU upload heaps (the card's memory; VICTORY_WINDOW_UPLOAD=1 puts windows there)"
        );
        assert_eq!(message("no fields at all"), "no fields at all");
    }

    #[test]
    fn a_report_of_real_format_lines() {
        let log = lines(&[
            "build          unknown (b99f876b)",
            FACTS,
            "2026-11-26T04:32:09.300100Z  INFO victory_gpu::host: host D3D12 device ready",
            "2026-11-26T04:32:09.300200Z  INFO victory_gpu::resource: render scale: unscaled scale=100 source=\"VICTORY_RES\"",
            "2026-11-26T04:32:09.247345Z  WARN victory_hle::modules::kernel: a host DLL outside the host policy: refused dll=\"rpcrt4.dll\"",
            "2026-11-26T04:32:09.247346Z  WARN victory_hle::modules::kernel: a host DLL outside the host policy: refused dll=\"other.dll\"",
            "2026-11-26T04:32:10.224904Z  WARN victory_gpu::device: a pipeline creation method that is not implemented: the title goes without this pipeline slot=0x218 calls=1",
            "2026-11-26T04:32:10.300000Z  WARN victory_gpu::pipeline::host: host pipeline refused error=0x80070057 shaders=[1]",
            "2026-11-26T04:32:10.400000Z ERROR victory_gpu::host: something failed path=C:\\Users\\someone\\x",
            "2026-11-26T04:32:20.285083Z  INFO victory_gpu::framelog: frame times: 3 frames in 5.0 s: shortest 33 ms, median 34, longest 120; 1 long (over 73 ms, the usual frame of the last 64 being 33); each: 34 33 120",
            "2026-11-26T04:32:20.285090Z  INFO victory_gpu::framelog: frame draws: the host draws recorded in each of the same 3 frames, in order; each: 3500 1200 4000",
            "2026-11-26T04:32:29.017484Z  INFO victory_gpu::queue: frame presented (since the last line: draws and dispatches, and those run on the host) frame=512 draws=1928 drawn=1900 dispatches=258 dispatched=258",
            "2026-11-26T04:32:30.000000Z  INFO victory_gpu::host: draws and dispatches dropped because their host pipeline was not made yet within the frame's wait (VICTORY_PIPELINE_WAIT_MS, 24 ms by default): 17 in 150 frames, 0.1 per frame",
            "2026-11-26T04:32:31.000000Z  INFO victory_gpu::draw: dynamic slots per frame, tables refused by cause: 12 asked more slots than were free (the ring was full), 3 had room but a search for a table no longer had failed since the last release (not searched), 0 were searched for and no run of free slots was that long (the free slots lay apart); 0 tables got a run lying across the place the search starts from (missed by the search as it was before); the fewest free slots seen since the last line: 0",
            "thread 'render' panicked at crates/victory-gpu/src/host.rs:10:5:",
            "the reason",
        ]);
        let text = build(Path::new("stderr.txt"), &log, &none());
        assert!(text.contains("adapter            Intel(R) Arc(TM) B580 Graphics"), "{text}");
        assert!(text.contains("wave_min           16"), "{text}");
        assert!(text.contains("render scale       100% (from VICTORY_RES)"), "{text}");
        assert!(text.contains("  VICTORY_RAW_SAVE=<user profile>\\raw dir\n"), "{text}");
        assert!(text.contains("  VICTORY_INPLACE_PROBE=1\n"), "{text}");
        assert!(text.contains("frames 3, in 1 five-second readings"), "{text}");
        assert!(text.contains("p50 34 ms"), "{text}");
        assert!(text.contains("frames with at least 3000 host draws: 2"), "{text}");
        assert!(text.contains("guest draws 1928, run on the host 1900"), "{text}");
        assert!(text.contains("not made yet: 17 (from 1 lines)"), "{text}");
        assert!(text.contains("refused: 1 warnings"), "{text}");
        assert!(text.contains("not implemented: 1 warnings"), "{text}");
        assert!(text.contains("1 of 1 readings had some; the most in one reading: 15 per frame"), "{text}");
        assert!(text.contains("thread 'render' panicked at"), "{text}");
        assert!(text.contains("  the reason"), "{text}");
        assert!(text.contains("      2  victory_hle::modules::kernel: a host DLL outside the host policy: refused"), "{text}");
        assert!(text.contains("== Errors by message: 1 lines"), "{text}");
        assert!(!text.contains("someone"), "{text}");
    }

    const CAUSE_LINES: &[&str] = &[
        "2026-11-27T10:00:00.000000Z  INFO victory_gpu::host: start-up: adapter Intel(R) Arc(TM) B580 Graphics (vendor 0x8086, device 0xe20b), driver 32.0.101.9150, 12118 MB of its own memory (11500 MB budget), feature level 12_2, shader model 6.8",
        "2026-11-27T10:00:00.000100Z  INFO victory_gpu::host: start-up: host wave lanes: min 16 max 32 - translated shaders need 32 or 64: 32 OK, 64 NOT OK",
        "2026-11-27T10:00:00.000200Z  INFO victory_gpu::host: start-up: set VICTORY_FRAME_LOG=1",
        "2026-11-27T10:00:00.000300Z  INFO victory_gpu::host: start-up: 1 VICTORY_ variables set; every other one is at the fork's default",
        "2026-11-27T10:00:01.000000Z  INFO victory_gpu::inplace: start-up (later): title memory in place: level 2 (asked 2): the default",
        "2026-11-27T10:00:05.000000Z  INFO victory_gpu::host: not done in the last 5 s, by cause (frame 120): scale.copy_left_out +40 (total 40), draw.dropped +7 (total 7)",
        "2026-11-27T10:00:06.000000Z  WARN victory_gpu::host: translations refused for wave size: 3 since the start (compute: 0 need waves of 32, 3 need waves of 64; pixel, vertex and mesh: 0, never pinned) - the card offers waves of 16 to 32 lanes; the passes of those shaders are not drawn",
        "2026-11-27T10:00:10.000000Z  INFO victory_gpu::host: not done in the last 5 s, by cause (frame 300): draw.dropped +5 (total 12)",
        "2026-11-27T10:00:10.100000Z  INFO victory_gpu::indirect: mesh launch over 5.0 s: 7200 indirect mesh draws launched through the amplification stage (3 waits for earlier shader writes in front of them), 12 through the clamp pass (12 for the pipeline has no form with the stage); on the card: 4 records clamped, 0 left empty by the list's budget, 0 charged out of turn (0: the budget was charged in list order, as the clamp pass charges it); since the start: mesh pipelines without a form with the stage: 1 whose mesh shader does not translate for it, 0 whose root signature is refused, 0 refused, 0 for want of the stage; title pipelines with an amplification stage (kind 23): 0 made by the title, 0 of them refused here (not translated)",
        "2026-11-27T10:00:15.100000Z  INFO victory_gpu::indirect: mesh launch over 5.0 s: 7000 indirect mesh draws launched through the amplification stage (0 waits for earlier shader writes in front of them), 0 through the clamp pass; on the card: 1 records clamped, 2 left empty by the list's budget, 0 charged out of turn (0: the budget was charged in list order, as the clamp pass charges it); since the start: mesh pipelines without a form with the stage: 1 whose mesh shader does not translate for it, 0 whose root signature is refused, 0 refused, 0 for want of the stage; title pipelines with an amplification stage (kind 23): 0 made by the title, 0 of them refused here (not translated)",
    ];

    #[test]
    fn the_causes_come_first_from_the_summary() {
        let mut list: Vec<&str> = CAUSE_LINES.to_vec();
        list.extend([
            "2026-11-27T10:01:00.000000Z  INFO victory_core::causes: what the layer did not do in this run: 2 causes over 640 frames, most frequent first",
            "2026-11-27T10:01:00.000100Z  INFO victory_core::causes: not done: scale.copy_left_out count=41 first_frame=5 last_frame=600 area=\"surfaces\" what=\"a copy between a render-scaled and an unscaled texture left out\"",
            "2026-11-27T10:01:00.000200Z  INFO victory_core::causes: not done: draw.dropped count=120 first_frame=3 last_frame=640 area=\"geometry\" what=\"direct draws and dispatches left out, all causes together (the draw.* rows name the causes)\"",
        ]);
        let text = build(Path::new("stderr.txt"), &lines(&list), &none());
        let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{needle} missing in
{text}"));
        assert!(at("== What did not work in this run, and why") < at("== Host device"), "{text}");
        assert!(text.contains("(from the end-of-run summary"), "{text}");
        let dropped = at("      120x  direct draws and dispatches left out, all causes together (the draw.* rows name the causes) (frames 3 to 640; geometry; draw.dropped)");
        let copies = at("       41x  a copy between a render-scaled and an unscaled texture left out (frames 5 to 600; surfaces; scale.copy_left_out)");
        assert!(dropped < copies, "most frequent first:
{text}");
        assert!(text.contains("translations refused for wave size: 3 (compute shaders"), "{text}");
        assert!(
            text.contains(
                "mesh launch (2 \"mesh launch over\" lines): 14200 indirect mesh draws through the amplification stage, 12 through the clamp pass; on the card 5 records clamped, 2 left empty by the list's budget, 0 charged out of turn"
            ),
            "{text}"
        );
        assert!(at("== Start-up facts") < at("== Host device"), "{text}");
        assert!(text.contains("
  adapter Intel(R) Arc(TM) B580 Graphics (vendor 0x8086"), "{text}");
        assert!(text.contains("
  host wave lanes: min 16 max 32"), "{text}");
        assert!(text.contains("
  set VICTORY_FRAME_LOG=1
"), "{text}");
        assert!(text.contains("
  start-up (later): title memory in place: level 2"), "{text}");
        // What the report printed before is still there.
        assert!(text.contains("== Work left out"), "{text}");
    }

    #[test]
    fn without_a_summary_the_periodic_totals_are_used() {
        let text = build(Path::new("stderr.txt"), &lines(CAUSE_LINES), &none());
        assert!(text.contains("the run ended without its summary: totals from the periodic lines, up to frame 300"), "{text}");
        assert!(text.contains("       40x  a copy between a render-scaled and an unscaled texture left out (frame 120; surfaces; scale.copy_left_out)"), "{text}");
        assert!(text.contains("       12x  direct draws and dispatches left out, all causes together (the draw.* rows name the causes) (frames 120 to 300; geometry; draw.dropped)"), "{text}");
    }

    #[test]
    fn a_summary_of_nothing_says_so() {
        let log = lines(&["2026-11-27T10:01:00.000000Z  INFO victory_core::causes: what the layer did not do in this run: nothing counted"]);
        let text = build(Path::new("stderr.txt"), &log, &none());
        assert!(text.contains("  nothing was counted"), "{text}");
        let old = build(Path::new("stderr.txt"), &lines(&[FACTS]), &none());
        assert!(old.contains("no cause lines in this log"), "{old}");
        assert!(old.contains("no \"start-up:\" lines"), "{old}");
    }

    #[test]
    fn summary_rows_read_back_what_the_registry_writes() {
        use victory_core::causes::{Cause, Row, summary_row};
        let row = Row { cause: Cause::DescriptorViewRefused, count: 9, first_frame: 2, last_frame: 77 };
        let text = format!("2026-11-27T10:01:00.000100Z  INFO victory_core::causes: {}", summary_row(&row));
        let line = parse(&text).expect("a log line");
        let read = cause_row(line.rest).expect("a summary row");
        assert_eq!(read.name, "descriptor.view_refused");
        assert_eq!((read.count, read.first_frame, read.last_frame), (9, 2, 77));
        assert_eq!(read.area, "surfaces");
        assert_eq!(read.what, Cause::DescriptorViewRefused.info().what);
        // Every cause's row reads back whole.
        for &c in Cause::ALL {
            let r = Row { cause: c, count: 1, first_frame: 0, last_frame: 1 };
            let read = cause_row(&summary_row(&r)).expect("a row");
            assert_eq!((read.name.as_str(), read.what.as_str()), (c.info().name, c.info().what));
        }
    }

    #[test]
    fn a_failed_device_is_said_first() {
        let log = lines(&[
            "2026-11-26T04:32:09.300000Z ERROR victory_gpu::host: NO HOST D3D12 DEVICE: 0x887A0004. The default adapter could not make a feature level 12_0 device",
        ]);
        let text = build(Path::new("stderr.txt"), &log, &none());
        assert!(text.contains("FAILED: NO HOST D3D12 DEVICE: 0x887A0004."), "{text}");
        assert!(text.contains("No \"frame times\" lines"), "{text}");
    }
}
