//! Read what `VICTORY_RAW_SAVE` captured.
//!
//! The capture is read-only evidence from a game run: every call of the
//! two console-only pipeline methods (0x1c8 and 0x218), with each word of
//! the description classified by content, the bytes it points at, the
//! root signatures the title created and the shader containers it passed
//! through the normal methods (format: the header of each `.tsv`, written
//! by `victory-gpu`'s `device::capture`). This tool never makes a pipeline
//! and never changes the runtime; it reports what is in the capture and
//! what the existing parsers can already see in it, so that the next format
//! decision is made from the actual bytes rather than from a guess. An
//! older dump (`.bin` files and `manifest.txt`) is read too.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// What one saved blob looks like to the existing readers.
#[derive(Debug, PartialEq)]
struct Report {
    name: String,
    size: usize,
    magic: String,
    rts0: Option<usize>,
    programs: usize,
    translated: usize,
    stages: Vec<String>,
    wave64: usize,
    reasons: Vec<String>,
}

/// The native programs of a shader container: they are in its `DXIL`
/// part, after the part's program header (`native_programs` reads that
/// part's data, not a whole container).
fn programs_of(bytes: &[u8]) -> Vec<victory_shader::NativeProgram> {
    use victory_shader::{Container, FourCC};
    Container::parse(bytes)
        .ok()
        .and_then(|c| c.part(FourCC::DXIL).map(|p| victory_shader::native_programs(p.data).unwrap_or_default()))
        .unwrap_or_default()
}

fn inspect(name: String, bytes: &[u8]) -> Report {
    use victory_shader::{Container, FourCC};
    let magic = String::from_utf8_lossy(&bytes[..bytes.len().min(4)]).into_owned();
    let rts0 = Container::parse(bytes)
        .ok()
        .and_then(|c| c.part(FourCC::RTS0).map(|p| p.data.len()));
    let native = programs_of(bytes);
    let programs = native.len();
    let mut stages = Vec::new();
    let mut wave64 = 0;
    let mut translated = 0;
    let mut reasons = Vec::new();
    for program in native {
        let stage = format!("{:?}", program.stage);
        if !stages.contains(&stage) {
            stages.push(stage);
        }
        if program.wave64 {
            wave64 += 1;
        }
        match victory_shader::emit::translate_native_program(program.clone()) {
            Ok(_) => translated += 1,
            Err(e) => {
                let text = format!("{:?}: {e}", program.stage);
                if !reasons.contains(&text) {
                    reasons.push(text);
                }
            }
        }
    }
    Report { name, size: bytes.len(), magic, rts0, programs, translated, stages, wave64, reasons }
}

/// Reads a capture directory and prints what it holds.
pub fn run(dir: &Path, list: bool) -> Result<()> {
    if !dir.is_dir() {
        bail!("not a directory: {}", dir.display());
    }
    if dir.join("calls.tsv").is_file() {
        let analysis = analyse(dir)?;
        print!("{}", analysis.text(list));
        return Ok(());
    }
    legacy(dir, list)
}

/// A dump of the first format: one `.bin` per blob.
fn legacy(dir: &Path, list: bool) -> Result<()> {
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "bin"))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no calls.tsv and no .bin files in {}", dir.display());
    }
    println!("{} has no calls.tsv: read as a dump of the first format, one blob per file", dir.display());
    println!("{:>28} {:>9} {:>6} {:>7} {:>5} {:>5}", "file", "bytes", "magic", "rts0", "prog", "done");
    let mut total = Report { name: String::new(), size: 0, magic: String::new(), rts0: None, programs: 0, translated: 0, stages: Vec::new(), wave64: 0, reasons: Vec::new() };
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_owned();
        let report = inspect(name, &bytes);
        if list {
            println!("{report:?}");
        }
        println!(
            "{:>28} {:>9} {:>6} {:>7} {:>5} {:>5}",
            report.name,
            report.size,
            report.magic,
            report.rts0.map_or("-".to_owned(), |n| n.to_string()),
            report.programs,
            report.translated
        );
        total.size += report.size;
        total.programs += report.programs;
        total.translated += report.translated;
        total.wave64 += report.wave64;
        for reason in report.reasons {
            if !total.reasons.contains(&reason) {
                total.reasons.push(reason);
            }
        }
        for stage in report.stages {
            if !total.stages.contains(&stage) {
                total.stages.push(stage);
            }
        }
    }
    println!(
        "{} blobs, {} bytes, {} native programs, {} translated, wave64 {}",
        files.len(),
        total.size,
        total.programs,
        total.translated,
        total.wave64
    );
    println!("stages seen: {:?}", total.stages);
    if !total.reasons.is_empty() {
        println!("reasons (one of each):");
        for reason in &total.reasons {
            println!("  {reason}");
        }
    }
    Ok(())
}

/// A `.tsv` of the capture: comment lines (`#`), then a line of column
/// names, then rows.
struct Table {
    columns: HashMap<String, usize>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn read(path: &Path) -> Result<Option<Table>> {
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut lines = text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty());
        let Some(header) = lines.next() else {
            return Ok(Some(Table { columns: HashMap::new(), rows: Vec::new() }));
        };
        let columns = header.split('\t').enumerate().map(|(i, c)| (c.to_owned(), i)).collect();
        let rows = lines.map(|l| l.split('\t').map(str::to_owned).collect()).collect();
        Ok(Some(Table { columns, rows }))
    }

    fn get<'a>(&self, row: &'a [String], column: &str) -> Option<&'a str> {
        self.columns.get(column).and_then(|&i| row.get(i)).map(String::as_str)
    }
}

/// One word token of `calls.tsv`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Null,
    Small,
    Other,
    Len,
    Root(Option<String>),
    Device,
    Ours { hash: String, saved: u64 },
    Pair { hash: String, len: u64, saved: u64 },
    Bare { hash: String, saved: u64 },
    Unknown(String),
}

fn hex(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

impl Token {
    fn parse(text: &str) -> Token {
        let parts: Vec<&str> = text.split(':').collect();
        let unknown = || Token::Unknown(text.to_owned());
        match parts.as_slice() {
            ["0"] => Token::Null,
            ["i"] => Token::Small,
            ["x"] => Token::Other,
            ["L"] => Token::Len,
            ["R", "?"] => Token::Root(None),
            ["R", h] => Token::Root(Some((*h).to_owned())),
            ["O", "device"] => Token::Device,
            ["O", "ours", h, n] => hex(n).map_or_else(unknown, |saved| Token::Ours { hash: (*h).to_owned(), saved }),
            ["P", h, len] => hex(len).map_or_else(unknown, |len| Token::Pair { hash: (*h).to_owned(), len, saved: len }),
            ["P", h, len, saved] => match (hex(len), hex(saved)) {
                (Some(len), Some(saved)) => Token::Pair { hash: (*h).to_owned(), len, saved },
                _ => unknown(),
            },
            ["B", h, n] => hex(n).map_or_else(unknown, |saved| Token::Bare { hash: (*h).to_owned(), saved }),
            _ => unknown(),
        }
    }

    /// One letter per class, as in the patterns printed.
    fn class(&self) -> char {
        match self {
            Token::Null => '0',
            Token::Small => 'i',
            Token::Other => 'x',
            Token::Len => 'L',
            Token::Root(_) => 'R',
            Token::Device | Token::Ours { .. } => 'O',
            Token::Pair { .. } => 'P',
            Token::Bare { .. } => 'B',
            Token::Unknown(_) => '?',
        }
    }

    /// The saved file this token names, if any.
    fn file(&self) -> Option<String> {
        match self {
            Token::Pair { hash, .. } => Some(format!("blob_{hash}.bin")),
            Token::Bare { hash, .. } | Token::Ours { hash, .. } => Some(format!("win_{hash}.bin")),
            _ => None,
        }
    }
}

/// Plain words for a class pattern: "root object, 3 address+length pairs".
fn describe(classes: &[char]) -> String {
    let count = |c: char| classes.iter().filter(|&&x| x == c).count();
    let mut parts = Vec::new();
    for (c, one, many) in [
        ('R', "root signature object", "root signature objects"),
        ('O', "other object of ours", "other objects of ours"),
        ('P', "address+length pair", "address+length pairs"),
        ('B', "bare address", "bare addresses"),
        ('x', "other value", "other values"),
        ('i', "small integer", "small integers"),
        ('0', "zero", "zeros"),
        ('?', "unreadable token", "unreadable tokens"),
    ] {
        match count(c) {
            0 => {}
            1 => parts.push(format!("1 {one}")),
            n => parts.push(format!("{n} {many}")),
        }
    }
    if parts.is_empty() { "nothing".to_owned() } else { parts.join(", ") }
}

/// What the words at one index of one method's descriptions were.
#[derive(Default, Debug)]
struct WordStats {
    classes: BTreeMap<char, usize>,
    /// Lengths of address+length pairs, smallest and largest.
    lens: Option<(u64, u64)>,
    /// Distinct blobs named (pairs, bare addresses).
    blobs: BTreeSet<String>,
    /// Of those, how many are saved, DXBC, root signatures, with native
    /// programs, the code of a known container's program (by stage), and
    /// how the match was made.
    saved: usize,
    dxbc: usize,
    rts0: usize,
    native: usize,
    code: BTreeMap<String, usize>,
    /// Distinct descriptions where this word equals one half of a known
    /// container's XDAT shader hash.
    xdat_hits: usize,
}

#[derive(Default, Debug)]
struct Method {
    calls: usize,
    descs: BTreeSet<String>,
    iids: BTreeMap<String, usize>,
    callers: BTreeMap<String, usize>,
    /// Class pattern up to the next root object (or the last non-zero
    /// word), with counts of calls.
    patterns: BTreeMap<String, usize>,
    /// Calls whose description holds another root object, by its word
    /// index: a sign of an array of descriptions.
    next_root: BTreeMap<usize, usize>,
    /// Root signatures named, with counts of calls; `?` unregistered.
    roots: BTreeMap<String, usize>,
    words: Vec<WordStats>,
    truncated: usize,
    short_reads: usize,
}

#[derive(Debug)]
struct Analysis {
    dir: String,
    methods: BTreeMap<String, Method>,
    known_roots: BTreeSet<String>,
    root_files: usize,
    containers: usize,
    container_files: usize,
    programs: usize,
    /// Files the tables name but that are not there.
    missing: BTreeSet<String>,
    bad_rows: usize,
    summary: Vec<(String, String)>,
}

/// A known native program: its stage and code.
struct Program {
    stage: String,
    container: String,
    code: Vec<u8>,
}

#[cfg(test)]
fn short(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_owned()
}

/// Which known program a raw blob is: the same bytes, or one is the other
/// with something after it (code padded to an alignment, say).
fn match_code<'a>(blob: &[u8], programs: &'a [Program], by_start: &HashMap<[u8; 16], Vec<usize>>) -> Option<(&'a Program, &'static str)> {
    let start: [u8; 16] = blob.get(..16)?.try_into().ok()?;
    by_start.get(&start)?.iter().map(|&i| &programs[i]).find_map(|p| {
        if p.code == blob {
            Some((p, "same bytes"))
        } else if blob.starts_with(&p.code) || p.code.starts_with(blob) {
            Some((p, "one starts with the other"))
        } else {
            None
        }
    })
}

/// Reads a capture of the second format.
fn analyse(dir: &Path) -> Result<Analysis> {
    let calls = Table::read(&dir.join("calls.tsv"))?.context("no calls.tsv")?;
    for column in ["method", "desc_hash", "words"] {
        if !calls.columns.contains_key(column) {
            bail!("calls.tsv has no {column} column: not a capture of the format this tool reads");
        }
    }
    let roots = Table::read(&dir.join("roots.tsv"))?;
    let containers = Table::read(&dir.join("containers.tsv"))?;
    let mut analysis = Analysis {
        dir: dir.display().to_string(),
        methods: BTreeMap::new(),
        known_roots: BTreeSet::new(),
        root_files: 0,
        containers: 0,
        container_files: 0,
        programs: 0,
        missing: BTreeSet::new(),
        bad_rows: 0,
        summary: Vec::new(),
    };
    if let Some(roots) = &roots {
        for row in &roots.rows {
            if let Some(hash) = roots.get(row, "hash") {
                analysis.known_roots.insert(hash.to_owned());
                if dir.join(format!("root_{hash}.bin")).is_file() {
                    analysis.root_files += 1;
                } else {
                    analysis.missing.insert(format!("root_{hash}.bin"));
                }
            }
        }
    }
    // Known programs, from the saved containers; XDAT shader hashes, from
    // the table.
    let mut programs = Vec::new();
    let mut xdat_halves: HashSet<u64> = HashSet::new();
    if let Some(table) = &containers {
        for row in &table.rows {
            analysis.containers += 1;
            let Some(hash) = table.get(row, "hash") else { continue };
            if let Some(x) = table.get(row, "xdat_hash").filter(|x| x.len() == 32) {
                let bytes: Vec<u8> = (0..16).filter_map(|i| u8::from_str_radix(&x[2 * i..2 * i + 2], 16).ok()).collect();
                if let Ok(b) = <[u8; 16]>::try_from(bytes.as_slice()) {
                    xdat_halves.insert(u64::from_le_bytes(b[..8].try_into().unwrap_or_default()));
                    xdat_halves.insert(u64::from_le_bytes(b[8..].try_into().unwrap_or_default()));
                }
            }
            let name = format!("cont_{hash}.bin");
            let Ok(bytes) = std::fs::read(dir.join(&name)) else {
                analysis.missing.insert(name);
                continue;
            };
            analysis.container_files += 1;
            for p in programs_of(&bytes) {
                programs.push(Program { stage: format!("{:?}", p.stage), container: hash.to_owned(), code: p.code });
            }
        }
    }
    analysis.programs = programs.len();
    let mut by_start: HashMap<[u8; 16], Vec<usize>> = HashMap::new();
    for (i, p) in programs.iter().enumerate() {
        if let Some(start) = p.code.get(..16).and_then(|s| <[u8; 16]>::try_from(s).ok()) {
            by_start.entry(start).or_default().push(i);
        }
    }
    // Each distinct blob is looked at once.
    let mut looked: HashMap<String, Option<(bool, bool, bool, Option<String>)>> = HashMap::new();
    let mut desc_words: HashMap<String, Option<Vec<u64>>> = HashMap::new();
    let mut xdat_seen: HashSet<(String, String, usize)> = HashSet::new();
    for row in &calls.rows {
        let (Some(method), Some(desc_hash), Some(words)) =
            (calls.get(row, "method"), calls.get(row, "desc_hash"), calls.get(row, "words"))
        else {
            analysis.bad_rows += 1;
            continue;
        };
        let tokens: Vec<Token> = words.split(' ').filter(|t| !t.is_empty()).map(Token::parse).collect();
        let m = analysis.methods.entry(method.to_owned()).or_default();
        m.calls += 1;
        m.descs.insert(desc_hash.to_owned());
        *m.iids.entry(calls.get(row, "iid").unwrap_or("-").to_owned()).or_default() += 1;
        *m.callers.entry(calls.get(row, "caller_module").unwrap_or("-").to_owned()).or_default() += 1;
        if calls.get(row, "desc_len").and_then(hex).is_some_and(|n| n < 0x200) {
            m.short_reads += 1;
        }
        let classes: Vec<char> = tokens.iter().map(Token::class).collect();
        let next_root = classes.iter().skip(1).position(|&c| c == 'R').map(|i| i + 1);
        if let Some(at) = next_root {
            *m.next_root.entry(at).or_default() += 1;
        }
        let end = next_root.unwrap_or_else(|| classes.iter().rposition(|&c| c != '0').map_or(0, |i| i + 1));
        let pattern: String = classes[..end].iter().map(|c| c.to_string()).collect::<Vec<_>>().join(" ");
        *m.patterns.entry(pattern).or_default() += 1;
        if m.words.len() < tokens.len() {
            m.words.resize_with(tokens.len(), WordStats::default);
        }
        let desc = desc_words.entry(desc_hash.to_owned()).or_insert_with(|| {
            std::fs::read(dir.join(format!("desc_{desc_hash}.bin")))
                .ok()
                .map(|b| b.as_chunks::<8>().0.iter().map(|w| u64::from_le_bytes(*w)).collect())
        });
        if desc.is_none() {
            analysis.missing.insert(format!("desc_{desc_hash}.bin"));
        }
        for (i, token) in tokens.iter().enumerate() {
            let stats = &mut m.words[i];
            *stats.classes.entry(token.class()).or_default() += 1;
            match token {
                Token::Root(hash) => {
                    *m.roots.entry(hash.clone().unwrap_or_else(|| "?".to_owned())).or_default() += 1;
                }
                Token::Pair { len, saved, .. } => {
                    stats.lens = Some(stats.lens.map_or((*len, *len), |(a, b)| (a.min(*len), b.max(*len))));
                    if saved < len {
                        m.truncated += 1;
                    }
                }
                Token::Other | Token::Small => {
                    if let Some(values) = desc
                        && values.get(i).is_some_and(|w| xdat_halves.contains(w))
                        && xdat_seen.insert((method.to_owned(), desc_hash.to_owned(), i))
                    {
                        stats.xdat_hits += 1;
                    }
                }
                _ => {}
            }
            let Some(name) = token.file() else { continue };
            if !stats.blobs.insert(name.clone()) {
                continue;
            }
            let facts = looked.entry(name.clone()).or_insert_with(|| {
                let bytes = std::fs::read(dir.join(&name)).ok()?;
                let report = inspect(name.clone(), &bytes);
                let code = match_code(&bytes, &programs, &by_start)
                    .map(|(p, how)| format!("{} ({how}; e.g. container {})", p.stage, p.container));
                Some((report.magic == "DXBC", report.rts0.is_some(), report.programs > 0, code))
            });
            match facts {
                None => {
                    analysis.missing.insert(name);
                }
                Some((dxbc, rts0, native, code)) => {
                    stats.saved += 1;
                    stats.dxbc += usize::from(*dxbc);
                    stats.rts0 += usize::from(*rts0);
                    stats.native += usize::from(*native);
                    if let Some(code) = code {
                        let stage = code.split(' ').next().unwrap_or("?").to_owned();
                        *stats.code.entry(stage).or_default() += 1;
                    }
                }
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("summary.txt")) {
        analysis.summary = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
    }
    Ok(analysis)
}

fn percent(part: usize, whole: usize) -> String {
    (part * 100).checked_div(whole).map_or_else(|| "0%".to_owned(), |p| format!("{p}%"))
}

impl Analysis {
    fn summary_value(&self, key: &str) -> Option<u64> {
        self.summary.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.parse().ok())
    }

    /// The report, ending in a plain-language account.
    fn text(&self, list: bool) -> String {
        let mut out = String::new();
        let total: usize = self.methods.values().map(|m| m.calls).sum();
        let _ = writeln!(out, "capture {}: {total} calls", self.dir);
        let _ = writeln!(
            out,
            "roots.tsv: {} root signatures the title created ({} saved); containers.tsv: {} containers ({} saved, {} native programs in them)",
            self.known_roots.len(),
            self.root_files,
            self.containers,
            self.container_files,
            self.programs
        );
        if self.summary.is_empty() {
            let _ = writeln!(out, "no summary.txt: the run's own counts (drops) are not known");
        } else {
            let line: Vec<String> = self.summary.iter().map(|(k, v)| format!("{k}={v}")).collect();
            let _ = writeln!(out, "the run's counts: {}", line.join(" "));
        }
        for (name, m) in &self.methods {
            let _ = writeln!(out, "\nmethod {name}: {} calls, {} distinct descriptions", m.calls, m.descs.len());
            let _ = writeln!(out, "  interface ids asked for: {:?}", m.iids);
            let _ = writeln!(out, "  callers: {:?}", m.callers);
            if m.short_reads > 0 {
                let _ = writeln!(out, "  {} calls had fewer than 0x200 description bytes readable", m.short_reads);
            }
            let _ = writeln!(out, "  word patterns up to the next root object (or the last non-zero word), by calls:");
            let mut patterns: Vec<_> = m.patterns.iter().collect();
            patterns.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let shown = if list { patterns.len() } else { patterns.len().min(12) };
            for (pattern, n) in &patterns[..shown] {
                let classes: Vec<char> = pattern.chars().filter(|c| !c.is_whitespace()).collect();
                let _ = writeln!(out, "    {n:>7}  {}  ({})", describe(&classes), pattern);
            }
            if shown < patterns.len() {
                let _ = writeln!(out, "    ... {} more patterns (--list shows all)", patterns.len() - shown);
            }
            if !m.next_root.is_empty() {
                let _ = writeln!(out, "  another root object in the same bytes, at word (calls): {:?}", m.next_root);
            }
            let known = m.roots.keys().filter(|h| self.known_roots.contains(*h)).count();
            let unregistered = m.roots.get("?").copied().unwrap_or(0);
            let _ = writeln!(
                out,
                "  root signatures named: {} distinct; {known} created by the title (in roots.tsv), {} not in roots.tsv{}",
                m.roots.len() - usize::from(unregistered > 0),
                m.roots.keys().filter(|h| *h != "?" && !self.known_roots.contains(*h)).count(),
                if unregistered > 0 { format!(", {unregistered} words naming an unregistered one") } else { String::new() }
            );
            let _ = writeln!(out, "  per word (class letters as in the patterns; % of calls):");
            for (i, w) in m.words.iter().enumerate() {
                let total: usize = w.classes.values().sum();
                if total == 0 || w.classes.keys().all(|&c| c == '0') {
                    continue;
                }
                let classes: Vec<String> = w.classes.iter().map(|(c, n)| format!("{c} {}", percent(*n, m.calls))).collect();
                let mut line = format!("    word {i:>2}: {}", classes.join(", "));
                if let Some((a, b)) = w.lens {
                    let _ = write!(line, "; lengths {a:#x}..{b:#x}");
                }
                if !w.blobs.is_empty() {
                    let _ = write!(line, "; {} distinct blobs, {} saved", w.blobs.len(), w.saved);
                    if w.dxbc > 0 {
                        let _ = write!(line, ", {} DXBC ({} root signatures, {} with native programs)", w.dxbc, w.rts0, w.native);
                    }
                    if !w.code.is_empty() {
                        let _ = write!(line, ", code of known programs: {:?}", w.code);
                    }
                }
                if w.xdat_hits > 0 {
                    let _ = write!(line, "; equals half of a known XDAT shader hash in {} descriptions", w.xdat_hits);
                }
                let _ = writeln!(out, "{line}");
            }
        }
        if !self.missing.is_empty() {
            let _ = writeln!(out, "\n{} files the tables name are not in the directory (dropped, or the run ended first)", self.missing.len());
            if list {
                for name in &self.missing {
                    let _ = writeln!(out, "  {name}");
                }
            }
        }
        if self.bad_rows > 0 {
            let _ = writeln!(out, "{} lines of calls.tsv could not be read", self.bad_rows);
        }
        let _ = writeln!(out, "\nsummary");
        for line in self.findings() {
            let _ = writeln!(out, "- {line}");
        }
        out
    }

    /// What the capture shows and does not show yet, in plain words.
    fn findings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.methods.is_empty() {
            out.push("No calls were captured: the title did not call 0x1c8 or 0x218 while VICTORY_RAW_SAVE was set.".to_owned());
        }
        for (name, m) in &self.methods {
            let first_root = m.words.first().and_then(|w| w.classes.get(&'R')).copied().unwrap_or(0);
            out.push(format!(
                "{name}: {} calls, {} distinct descriptions; word 0 is one of our root signature objects in {} of the calls.",
                m.calls,
                m.descs.len(),
                percent(first_root, m.calls)
            ));
            let unknown: Vec<&String> = m.roots.keys().filter(|h| *h != "?" && !self.known_roots.contains(*h)).collect();
            if m.roots.is_empty() {
                out.push(format!("{name}: no word names a root signature of ours."));
            } else if unknown.is_empty() && !m.roots.contains_key("?") {
                out.push(format!("{name}: every root signature named is one the title created; its bytes are saved (root_<hash>.bin)."));
            } else {
                out.push(format!("{name}: {} root signatures named are not among those the title created through CreateRootSignature.", unknown.len() + usize::from(m.roots.contains_key("?"))));
            }
            let indices = |c: char| -> Vec<usize> {
                m.words.iter().enumerate().filter(|(_, w)| w.classes.get(&c).is_some_and(|&n| n * 2 >= m.calls)).map(|(i, _)| i).collect()
            };
            let pairs = indices('P');
            if !pairs.is_empty() {
                let matched: Vec<String> = pairs
                    .iter()
                    .filter(|&&i| !m.words[i].code.is_empty())
                    .map(|&i| format!("word {i} ({:?})", m.words[i].code))
                    .collect();
                let unmatched: Vec<usize> = pairs.iter().copied().filter(|&i| m.words[i].code.is_empty()).collect();
                let (named, saved) = pairs.iter().fold((0, 0), |(n, s), &i| (n + m.words[i].blobs.len(), s + m.words[i].saved));
                out.push(format!(
                    "{name}: in most calls, address+length pairs start at words {pairs:?}; {saved} of the {named} distinct blobs they name are in the directory."
                ));
                if self.programs == 0 {
                    out.push(format!(
                        "{name}: no known native programs to compare with (containers.tsv is empty or its containers were not saved), so raw code cannot be tied to understood records yet."
                    ));
                } else if matched.is_empty() {
                    out.push(format!("{name}: no blob is the code of a known container's program; the blobs are not tied to understood records yet."));
                } else {
                    out.push(format!("{name}: blobs that are the code of a known container's program: {}. That container's FSSM record is the ground truth for those programs.", matched.join(", ")));
                }
                if !unmatched.is_empty() {
                    out.push(format!("{name}: not understood yet: what the blobs at words {unmatched:?} hold (stage data, register state, root layout or resource tables are candidates, none is shown)."));
                }
            }
            let bare = indices('B');
            if !bare.is_empty() {
                out.push(format!("{name}: words {bare:?} are bare addresses in most calls; only 0x200 bytes at each were saved, and what they point at, and how long it is, is not known."));
            }
            let other = indices('x');
            let hits: Vec<usize> = m.words.iter().enumerate().filter(|(_, w)| w.xdat_hits > 0).map(|(i, _)| i).collect();
            if !other.is_empty() {
                out.push(format!("{name}: words {other:?} hold values that are neither small nor readable addresses (hashes or flags)."));
            }
            if !hits.is_empty() {
                out.push(format!("{name}: words {hits:?} equal half of a known container's XDAT shader hash in some descriptions: those descriptions name a program the title also passed as a container."));
            }
            if m.truncated > 0 {
                out.push(format!("{name}: {} address+length blobs were saved only in part (an unreadable page, or over 1 MiB).", m.truncated));
            }
            if m.next_root.len() == 1 {
                let at = m.next_root.keys().next().copied().unwrap_or(0);
                out.push(format!("{name}: another root object follows at word {at} in {} calls: the descriptions may lie in an array of {:#x}-byte elements (observed, not proven).", m.next_root[&at], at * 8));
            }
        }
        out.push("Descriptions are read as 0x200 bytes: words past a description's real end belong to whatever follows it, and nothing here says where a description ends.".to_owned());
        let dropped: u64 = ["dropped_queue_full", "dropped_size_cap", "lines_dropped", "write_errors", "probe_panics"]
            .iter()
            .filter_map(|k| self.summary_value(k))
            .sum();
        if self.summary.is_empty() {
            out.push("Whether anything was dropped is not known (no summary.txt).".to_owned());
        } else if dropped > 0 || !self.missing.is_empty() {
            out.push(format!(
                "The capture is not complete: {dropped} drops or errors were counted by the run and {} named files are missing.",
                self.missing.len()
            ));
        } else {
            out.push("Nothing was dropped: every named file is present.".to_owned());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_is_reported_without_panicking() {
        let r = inspect("bad.bin".into(), b"not a container");
        assert_eq!(r.name, "bad.bin");
        assert_eq!(r.size, 15);
        assert_eq!(r.magic, "not ");
        assert_eq!(r.rts0, None);
        assert_eq!(r.programs, 0);
        assert_eq!(r.translated, 0);
    }

    #[test]
    fn an_empty_dxbc_container_has_no_rts0() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"DXBC");
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0x20u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let r = inspect("empty.bin".into(), &bytes);
        assert_eq!(r.magic, "DXBC");
        assert_eq!(r.rts0, None);
    }

    #[test]
    fn raw_tokens_parse() {
        assert_eq!(Token::parse("0"), Token::Null);
        assert_eq!(Token::parse("R:?"), Token::Root(None));
        assert_eq!(Token::parse("R:00ff"), Token::Root(Some("00ff".into())));
        assert_eq!(Token::parse("P:ab:180"), Token::Pair { hash: "ab".into(), len: 0x180, saved: 0x180 });
        assert_eq!(Token::parse("P:ab:400:100"), Token::Pair { hash: "ab".into(), len: 0x400, saved: 0x100 });
        assert_eq!(Token::parse("B:cd:200"), Token::Bare { hash: "cd".into(), saved: 0x200 });
        assert_eq!(Token::parse("O:ours:ef:40"), Token::Ours { hash: "ef".into(), saved: 0x40 });
        assert_eq!(Token::parse("O:device"), Token::Device);
        assert_eq!(Token::parse("P:ab:zz").class(), '?');
        assert_eq!(describe(&['R', 'P', 'L', 'P', 'L', 'B', 'x', '0']), "1 root signature object, 2 address+length pairs, 1 bare address, 1 other value, 1 zero");
    }

    /// A stored (uncompressed) zlib stream.
    fn zlib_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01, 0x01];
        let len = data.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(data);
        let (mut a, mut b) = (1u32, 0u32);
        for &x in data {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    /// A console container with one compute program whose code is `code`,
    /// and `hash` as its XDAT shader hash.
    fn container(code: &[u8], hash: [u8; 16]) -> Vec<u8> {
        const ABSENT: u64 = 0xDEAD_BEEF_DEAD_BEEF;
        let mut payload = vec![0u8; 0x110];
        payload[..8].copy_from_slice(&6u64.to_le_bytes());
        for at in (0x08..=0x60).step_by(8) {
            payload[at..at + 8].copy_from_slice(&ABSENT.to_le_bytes());
        }
        payload[0x60..0x68].copy_from_slice(&0x80u64.to_le_bytes());
        // The record at 0x80: code at 0x110, no tables, 4 VGPRs; stage
        // data after it (64 threads).
        payload[0x80..0x88].copy_from_slice(&0x110u64.to_le_bytes());
        payload[0x88..0x90].copy_from_slice(&(code.len() as u64).to_le_bytes());
        payload[0xd0..0xd4].copy_from_slice(&4u32.to_le_bytes());
        payload[0xd4 + 0x28..0xd4 + 0x2c].copy_from_slice(&64u32.to_le_bytes());
        payload.extend_from_slice(code);
        let mut fssm = b"FSSM".to_vec();
        fssm.extend_from_slice(&0x101u32.to_le_bytes());
        fssm.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        fssm.extend_from_slice(&payload);
        let stream = zlib_stored(&fssm);
        let mut xbch = b"XBCH".to_vec();
        xbch.extend_from_slice(&3u32.to_le_bytes());
        xbch.extend_from_slice(&(stream.len() as u64).to_le_bytes());
        xbch.extend_from_slice(&(fssm.len() as u64).to_le_bytes());
        xbch.extend_from_slice(&stream);
        let mut part = vec![0u8; 0x18];
        part.extend_from_slice(b"XDAT");
        part.extend_from_slice(&2u32.to_le_bytes());
        part.extend_from_slice(&0x24u32.to_le_bytes());
        let first = 0x24 + 24;
        for (kind, offset, size) in [(0x0du32, first + 16, xbch.len()), (0x10, first, 16)] {
            for v in [kind, offset as u32, size as u32] {
                part.extend_from_slice(&v.to_le_bytes());
            }
        }
        part.extend_from_slice(&hash);
        part.extend_from_slice(&xbch);
        // A DXBC container with that one part.
        let total = 0x20 + 4 + 8 + part.len();
        let mut out = b"DXBC".to_vec();
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&0x24u32.to_le_bytes());
        out.extend_from_slice(b"DXIL");
        out.extend_from_slice(&(part.len() as u32).to_le_bytes());
        out.extend_from_slice(&part);
        out
    }

    #[test]
    fn raw_synthetic_capture_is_read_end_to_end() {
        let dir = std::env::temp_dir().join(format!(
            "victory-raw-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, bytes: &[u8]| std::fs::write(dir.join(name), bytes).unwrap();

        // A known program, its container and its XDAT hash.
        let code: Vec<u8> = (0..0x40u8).map(|i| i.wrapping_mul(7) ^ 0x5a).collect();
        let hash: [u8; 16] = std::array::from_fn(|i| 0x10 + i as u8);
        let cont = container(&code, hash);
        assert_eq!(programs_of(&cont).len(), 1, "the synthetic container must read");
        let cont_hash = short(&cont);
        write(&format!("cont_{cont_hash}.bin"), &cont);
        let xdat_hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        write(
            "containers.tsv",
            format!("# comment\nhash\tstage\tsize\txdat_hash\txdat\tprograms\n{cont_hash}\tcompute\t{:x}\t{xdat_hex}\t-\t-\n", cont.len()).as_bytes(),
        );
        // One root signature the title created.
        write("roots.tsv", b"# comment\nhash\trts0\tblob\tsize\tframe\naaaaaaaaaaaaaaaa\taaaaaaaaaaaaaaaa\tbbbbbbbbbbbbbbbb\t20\t0\n");
        write("root_aaaaaaaaaaaaaaaa.bin", &[0; 0x20]);
        // Raw code: the program's code padded to 0x80; a small blob; a window.
        let mut padded = code.clone();
        padded.resize(0x80, 0);
        let (raw, small, window) = (short(&padded), short(b"small blob"), short(&[1; 0x200]));
        write(&format!("blob_{raw}.bin"), &padded);
        write(&format!("blob_{small}.bin"), b"small blob");
        write(&format!("win_{window}.bin"), &[1; 0x200]);
        // Descriptions: word 11 holds the first half of the XDAT hash.
        let mut words = [0u64; 64];
        words[11] = u64::from_le_bytes(hash[..8].try_into().unwrap());
        let desc: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let (d1, d2) = (short(&desc), short(b"other"));
        write(&format!("desc_{d1}.bin"), &desc);
        write(&format!("desc_{d2}.bin"), &desc);
        let tokens = format!("R:aaaaaaaaaaaaaaaa P:{small}:a L 0 0 P:{raw}:80 L 0 0 B:{window}:200 0 x R:cccccccccccccccc");
        let mut calls = String::from("# comment\nseq\tmethod\tcall\tframe\tthread\trun\tdesc\tdesc_hash\tdesc_len\tnew\tiid\tout\tcaller\tcaller_module\twords\n");
        for (seq, (method, desc)) in [("0x218", &d1), ("0x218", &d1), ("0x218", &d2), ("0x1c8", &d1)].into_iter().enumerate() {
            let _ = writeln!(calls, "{seq}\t{method}\t1\t0\t1\t1\t0x1000\t{desc}\t0x200\t1\t-\t0x2000\t0x3000\t0x0+0x3000\t{tokens}");
        }
        // A call naming a blob that is not there.
        let _ = writeln!(calls, "4\t0x1c8\t2\t0\t1\t1\t0x1000\t{d1}\t0x200\t0\t-\t0x2000\t-\t-\tR:? P:ffffffffffffffff:10 L");
        write("calls.tsv", calls.as_bytes());
        write("summary.txt", b"calls_1c8=2\ncalls_218=3\ndropped_queue_full=0\ndropped_size_cap=0\nlines_dropped=0\n");

        let a = analyse(&dir).unwrap();
        let m = &a.methods["0x218"];
        assert_eq!((m.calls, m.descs.len()), (3, 2));
        assert_eq!(m.patterns.len(), 1);
        assert_eq!(m.next_root.get(&12), Some(&3));
        assert_eq!(m.roots.get("aaaaaaaaaaaaaaaa"), Some(&3));
        assert_eq!(m.roots.get("cccccccccccccccc"), Some(&3));
        assert_eq!(m.words[5].code.get("Cs"), Some(&1));
        assert!(m.words[1].code.is_empty());
        assert_eq!(m.words[1].lens, Some((0xa, 0xa)));
        assert_eq!(m.words[11].xdat_hits, 2, "one per distinct description");
        assert_eq!(a.programs, 1);
        let c = &a.methods["0x1c8"];
        assert_eq!((c.calls, c.descs.len()), (2, 1));
        assert_eq!(c.roots.get("?"), Some(&1));
        assert!(a.missing.contains("blob_ffffffffffffffff.bin"));
        let text = a.text(false);
        assert!(text.contains("1 root signature object, 2 address+length pairs, 1 bare address, 1 other value, 5 zeros"), "{text}");
        assert!(text.contains("code of known programs"), "{text}");
        assert!(text.contains("not among those the title created"), "{text}");
        assert!(text.contains("The capture is not complete"), "{text}");
        assert!(text.contains("array of 0x60-byte elements"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
