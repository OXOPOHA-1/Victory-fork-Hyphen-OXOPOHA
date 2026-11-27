//! A small differential harness for frame logs.
//!
//! A run with `VICTORY_FRAME_LOG=1` writes, every five seconds, a "frame
//! times" line with the length of each frame of that reading (whole
//! milliseconds) and a "frame draws" line with the host draws recorded in
//! each of the same frames; besides them a "usual frame" line (the MEAN of
//! the reading's frames that are not long) and up to eight "long frame"
//! lines. It also writes one "first bind" line per shader of each pipeline
//! the first time a list binds it, and "frame presented" lines (at frames
//! 1, 2, 4, 8, ...) with the guest draws and dispatches since the previous
//! one. A geometry/pixel hash pair at first bind identifies a point in the
//! game, so the two runs can be aligned by those pairs instead of by
//! trusting the operator to start in the same scene. The protocol idea comes
//! from `kelvaroq`'s `compare_native_workload`, read as an observation; no
//! code was copied.
//!
//! The tool changes no runtime behavior. Its statistics are over single
//! frames (the "each" lists), never over the five-second means: the
//! nearest-rank p50/p95/p99, the longest and the mean frame among the frames
//! with at least `--min-draws` host draws between the first and last shared
//! anchor. A log from before the "frame draws" line has no per-frame draws;
//! its frames are then taken by their reading's usual-frame draws, and the
//! output says how many were.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};

use crate::logline::{clean, each, field, parse};

/// The lines of a log file, colour codes taken off.
fn read_lines(path: &Path) -> Result<Vec<String>> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.lines().map(|l| clean(l).into_owned()).collect())
}

/// One reading of the frame log: where its "frame times" line is, its
/// frames' lengths, their draws if logged, and its usual frame's draws.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Reading {
    pub(crate) line: usize,
    pub(crate) ms: Vec<f64>,
    pub(crate) draws: Option<Vec<f64>>,
    pub(crate) usual_draws: Option<f64>,
}

/// The host draws of a "usual frame" line: the number of its `draws`
/// section (`| 7129 draws, 2000 with the preceding draw's pipeline |`, or
/// `| 7129 draws |` in older builds).
fn usual_draws(rest: &str) -> Option<f64> {
    if !rest.starts_with("usual frame (the mean of ") {
        return None;
    }
    rest.split(" | ").find_map(|part| {
        let (n, tail) = part.trim().split_once(' ')?;
        (tail == "draws" || tail.starts_with("draws, ")).then(|| n.parse().ok()).flatten()
    })
}

/// The readings of a log in order. The lines of one reading are logged
/// together, after its "frame times" line.
pub(crate) fn readings(lines: &[String]) -> Vec<Reading> {
    let mut out: Vec<Reading> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(line) = parse(line) else { continue };
        if !line.target.ends_with("framelog") {
            continue;
        }
        if let Some(ms) = each(line.rest, "frame times:") {
            out.push(Reading { line: i, ms, ..Reading::default() });
        } else if let Some(draws) = each(line.rest, "frame draws:")
            && let Some(last) = out.last_mut()
            && last.draws.is_none()
            && last.ms.len() == draws.len()
        {
            last.draws = Some(draws);
        } else if let Some(draws) = usual_draws(line.rest)
            && let Some(last) = out.last_mut()
            && last.usual_draws.is_none()
        {
            last.usual_draws = Some(draws);
        }
    }
    out
}

/// The frames counted in `bounds` (line indices) with at least `min_draws`
/// host draws: their lengths, their draws where known, and how many were
/// taken by their reading's usual frame instead of their own draws.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Counted {
    pub(crate) ms: Vec<f64>,
    pub(crate) draws: f64,
    pub(crate) by_reading: usize,
    pub(crate) readings: usize,
}

pub(crate) fn count(readings: &[Reading], min_draws: u64, bounds: Option<(usize, usize)>) -> Counted {
    let min = min_draws as f64;
    let mut out = Counted::default();
    for r in readings {
        if let Some((lo, hi)) = bounds
            && (r.line < lo || r.line > hi)
        {
            continue;
        }
        let before = out.ms.len();
        match &r.draws {
            Some(draws) => {
                for (&ms, &d) in r.ms.iter().zip(draws) {
                    if d >= min {
                        out.ms.push(ms);
                        out.draws += d;
                    }
                }
            }
            None => {
                // No draws per frame in this reading: its frames carry no
                // draw count, so the draws per frame leave them out.
                if min_draws == 0 || r.usual_draws.is_some_and(|d| d >= min) {
                    out.ms.extend(&r.ms);
                    out.by_reading += r.ms.len();
                }
            }
        }
        if out.ms.len() > before {
            out.readings += 1;
        }
    }
    out
}

/// A pipeline's shaders at first bind: the geometry stage's hash (mesh or
/// vertex) and the pixel stage's.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Anchor {
    geometry: String,
    pixel: String,
}

fn parse_anchor_arg(text: &str) -> Result<Anchor> {
    let (geometry, pixel) = text.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("an anchor must be geometry:pixel (two 16-digit hashes, mesh or vertex then pixel), got {text:?}")
    })?;
    if geometry.is_empty() || pixel.is_empty() {
        bail!("both halves of an anchor must be non-empty: {text:?}");
    }
    Ok(Anchor { geometry: geometry.to_owned(), pixel: pixel.to_owned() })
}

/// Anchors in log order: (line index of the pipeline's first line, pair).
/// The shaders of one pipeline are logged together under its `pipeline=`
/// field; lines of other pipelines recorded on other threads may come in
/// between, so they are gathered by that field, and a stage seen again for
/// the same field (an address used again by a later pipeline) starts anew.
fn anchors(lines: &[String]) -> Vec<(usize, Anchor)> {
    let mut open: HashMap<String, (usize, Option<String>, Option<String>)> = HashMap::new();
    let mut out = Vec::new();
    let finish = |entry: (usize, Option<String>, Option<String>), out: &mut Vec<(usize, Anchor)>| {
        if let (i, Some(geometry), Some(pixel)) = entry {
            out.push((i, Anchor { geometry, pixel }));
        }
    };
    for (i, line) in lines.iter().enumerate() {
        let Some(line) = parse(line) else { continue };
        if !line.rest.starts_with("first bind ") {
            continue;
        }
        let (Some(pipeline), Some(stage), Some(hash)) =
            (field(line.rest, "pipeline"), field(line.rest, "stage"), field(line.rest, "hash"))
        else {
            continue;
        };
        let geometry = match stage.as_str() {
            "mesh" | "vertex" => true,
            "pixel" => false,
            _ => continue,
        };
        let entry = open.entry(pipeline).or_insert((i, None, None));
        let taken = if geometry { entry.1.is_some() } else { entry.2.is_some() };
        if taken {
            let old = std::mem::replace(entry, (i, None, None));
            finish(old, &mut out);
        }
        if geometry {
            entry.1 = Some(hash);
        } else {
            entry.2 = Some(hash);
        }
    }
    for entry in open.into_values() {
        finish(entry, &mut out);
    }
    out.sort_by_key(|(i, _)| *i);
    out
}

/// The bounds of the shared part of two logs, in line indices: from the
/// first anchor both logs have to the last one that comes after it in both
/// (each anchor taken where it first appears). `None` when no such range.
fn shared_bounds(a: &[(usize, Anchor)], b: &[(usize, Anchor)]) -> Option<((usize, usize), (usize, usize))> {
    let mut b_by: HashMap<&Anchor, usize> = HashMap::new();
    for (j, anchor) in b {
        b_by.entry(anchor).or_insert(*j);
    }
    let mut seen = std::collections::HashSet::new();
    let common: Vec<(usize, usize)> = a
        .iter()
        .filter(|(_, anchor)| seen.insert(anchor))
        .filter_map(|(i, anchor)| b_by.get(anchor).map(|j| (*i, *j)))
        .collect();
    let first = *common.first()?;
    let last = common.iter().rev().find(|(i, j)| *i > first.0 && *j > first.1)?;
    Some(((first.0, last.0), (first.1, last.1)))
}

/// Bounds named by the operator: each anchor must be found in both logs,
/// and the range must not run backwards in either.
fn named_bounds(
    requested: &[Anchor],
    a: &[(usize, Anchor)],
    b: &[(usize, Anchor)],
) -> Result<((usize, usize), (usize, usize))> {
    let find = |list: &[(usize, Anchor)], want: &Anchor| list.iter().find(|(_, have)| have == want).map(|(i, _)| *i);
    let mut a_lines = Vec::new();
    let mut b_lines = Vec::new();
    for want in requested {
        let (Some(i), Some(j)) = (find(a, want), find(b, want)) else {
            bail!("anchor {}:{} was not found in both logs", want.geometry, want.pixel);
        };
        a_lines.push(i);
        b_lines.push(j);
    }
    let bounds = |v: &[usize]| (*v.iter().min().unwrap(), *v.iter().max().unwrap());
    Ok((bounds(&a_lines), bounds(&b_lines)))
}

/// Guest draws and dispatches of the "frame presented" lines between the
/// bounds. Those lines come at frames 1, 2, 4, 8, ... and each covers the
/// frames since the one before, so this is a coarse check of the scene's
/// contents, not a per-frame count.
/// `drawn` and `dispatched` are those the host ran; the rest were left out
/// (a pipeline not made yet or refused, a table refused).
#[derive(Debug, Default, PartialEq)]
struct Work {
    draws: u64,
    drawn: u64,
    dispatches: u64,
    dispatched: u64,
}

fn work(lines: &[String], bounds: Option<(usize, usize)>) -> Work {
    let mut total = Work::default();
    for (i, line) in lines.iter().enumerate() {
        if let Some((lo, hi)) = bounds
            && (i < lo || i > hi)
        {
            continue;
        }
        let Some(line) = parse(line) else { continue };
        if !line.rest.starts_with("frame presented") {
            continue;
        }
        let n = |key: &str| field(line.rest, key).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        total.draws += n("draws");
        total.drawn += n("drawn");
        total.dispatches += n("dispatches");
        total.dispatched += n("dispatched");
    }
    total
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (p * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

#[derive(Debug, PartialEq)]
pub(crate) struct Summary {
    pub(crate) p50: f64,
    pub(crate) p95: f64,
    pub(crate) p99: f64,
    pub(crate) max: f64,
    pub(crate) mean: f64,
}

pub(crate) fn summarise(ms: &[f64]) -> Summary {
    let mut values = ms.to_vec();
    values.sort_by(f64::total_cmp);
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    Summary {
        p50: percentile(&values, 0.50),
        p95: percentile(&values, 0.95),
        p99: percentile(&values, 0.99),
        max: values.last().copied().unwrap_or(f64::NAN),
        mean,
    }
}

/// Compare two frame logs, aligned by geometry/pixel shader anchors.
pub fn compare(a: &Path, b: &Path, min_draws: u64, anchor_args: &[String]) -> Result<()> {
    let lines_a = read_lines(a)?;
    let lines_b = read_lines(b)?;
    let (readings_a, readings_b) = (readings(&lines_a), readings(&lines_b));
    for (path, r) in [(a, &readings_a), (b, &readings_b)] {
        if r.is_empty() {
            bail!(
                "{} has no \"frame times\" line: was the run made with VICTORY_FRAME_LOG=1, and is this its whole log (stderr)?",
                path.display()
            );
        }
    }
    let anchors_a = anchors(&lines_a);
    let anchors_b = anchors(&lines_b);
    let requested: Vec<Anchor> = anchor_args.iter().map(|s| parse_anchor_arg(s)).collect::<Result<_>>()?;
    let bounds = if requested.is_empty() {
        let found = shared_bounds(&anchors_a, &anchors_b);
        match &found {
            Some(((a0, a1), (b0, b1))) => println!(
                "aligned by shared shader anchors (pipelines first bound in both runs): A lines {a0}..{a1}, B lines {b0}..{b1}"
            ),
            None => println!("no shared shader anchors in order; comparing whole logs"),
        }
        found
    } else {
        let found = named_bounds(&requested, &anchors_a, &anchors_b)?;
        println!("aligned by {} named anchors: A lines {:?}, B lines {:?}", requested.len(), found.0, found.1);
        Some(found)
    };
    let counted_a = count(&readings_a, min_draws, bounds.map(|b| b.0));
    let counted_b = count(&readings_b, min_draws, bounds.map(|b| b.1));
    for (name, path, c) in [("A", a, &counted_a), ("B", b, &counted_b)] {
        if c.ms.is_empty() {
            bail!(
                "no frame of {name} ({}) has at least {min_draws} host draws in the measured range; try a lower --min-draws",
                path.display()
            );
        }
    }
    let (sa, sb) = (summarise(&counted_a.ms), summarise(&counted_b.ms));
    let (work_a, work_b) = (work(&lines_a, bounds.map(|b| b.0)), work(&lines_b, bounds.map(|b| b.1)));
    println!();
    println!("Single frames with at least {min_draws} host draws, lengths as the frame log rounds them (whole ms).");
    println!("p50/p95/p99 are nearest-rank percentiles of those frames; not five-second means.");
    println!("{:>34} {:>12} {:>12} {:>12}", "", "A", "B", "B - A");
    let row = |name: &str, a: f64, b: f64| println!("{name:>34} {a:12.1} {b:12.1} {:12.1}", b - a);
    let whole = |name: &str, a: u64, b: u64| println!("{name:>34} {a:12} {b:12} {:12}", b as i64 - a as i64);
    whole("frames counted", counted_a.ms.len() as u64, counted_b.ms.len() as u64);
    whole("five-second readings they are in", counted_a.readings as u64, counted_b.readings as u64);
    row("p50 frame ms", sa.p50, sb.p50);
    row("p95 frame ms", sa.p95, sb.p95);
    row("p99 frame ms", sa.p99, sb.p99);
    row("longest frame ms", sa.max, sb.max);
    row("mean frame ms", sa.mean, sb.mean);
    let per_frame = |c: &Counted| if c.by_reading == c.ms.len() { f64::NAN } else { c.draws / (c.ms.len() - c.by_reading) as f64 };
    row("host draws per counted frame", per_frame(&counted_a), per_frame(&counted_b));
    whole("guest draws, frame presented", work_a.draws, work_b.draws);
    whole("  of them run on the host", work_a.drawn, work_b.drawn);
    whole("guest dispatches, frame presented", work_a.dispatches, work_b.dispatches);
    whole("  of them run on the host", work_a.dispatched, work_b.dispatched);
    println!("(the \"frame presented\" lines come at frames 1, 2, 4, 8, ...: a coarse check of the scene, not per frame)");
    for (name, c) in [("A", &counted_a), ("B", &counted_b)] {
        if c.by_reading > 0 {
            println!(
                "NOTE: {} of {name}'s {} frames come from readings without per-frame draws (an older log): \
                 they were counted because their reading's usual frame had at least {min_draws} draws",
                c.by_reading,
                c.ms.len()
            );
        }
    }
    if counted_a.ms.len() < 100 || counted_b.ms.len() < 100 {
        println!("NOTE: fewer than 100 frames on a side: p99 is then the longest frame or close to it");
    }
    if work_a.draws != work_b.draws || work_a.dispatches != work_b.dispatches {
        println!("WARNING: the two ranges did not record the same guest workload; compare like with like");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Lines in the format the logger writes (victory_core::logging, framelog.rs, pipeline.rs, queue.rs).
    const TIMES: &str = "2026-11-26T04:32:20.285083Z  INFO victory_gpu::framelog: frame times: 4 frames in 5.0 s: shortest 33 ms, median 34, longest 82; 1 long (over 73 ms, the usual frame of the last 64 being 33); each: 34 33 82 40";
    const DRAWS: &str = "2026-11-26T04:32:20.285090Z  INFO victory_gpu::framelog: frame draws: the host draws recorded in each of the same 4 frames, in order; each: 3500 1200 4000 3100";
    const USUAL: &str = "2026-11-26T04:32:20.285100Z  INFO victory_gpu::framelog: usual frame (the mean of 3 not long): 36 ms | render thread: ran 2 ms (user 1, kernel 0) | pipelines: 0 made, 0 loaded, 0.0 ms waited, 0 draws dropped as not made yet | disk: 0 reads 0.0 MB 0.0 worker ms | 2600 draws, 2000 with the preceding draw's pipeline | 9000 MB free";
    const LONG: &str = "2026-11-26T04:32:20.285110Z  INFO victory_gpu::framelog: long frame 3 at 4.2 s: 82 ms | render thread: ran 2 ms | 4000 draws, 3000 with the preceding draw's pipeline | 9000 MB free";
    const VERTEX: &str = "2026-11-26T04:32:10.590339Z  INFO victory_gpu::pipeline: first bind pipeline=0x222cf05a070 frame=2 stage=\"vertex\" size=992 hash=dfafaf385b2f94e8";
    const PIXEL: &str = "2026-11-26T04:32:10.590353Z  INFO victory_gpu::pipeline: first bind pipeline=0x222cf05a070 frame=2 stage=\"pixel\" size=964 hash=06295fb1d1aefb78";
    const PRESENT: &str = "2026-11-26T04:32:29.017484Z  INFO victory_gpu::queue: frame presented (since the last line: draws and dispatches, and those run on the host) frame=512 draws=1928 drawn=1928 dispatches=258 dispatched=258";

    fn owned(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn a_reading_has_its_frames_and_their_draws() {
        let lines = owned(&[TIMES, DRAWS, USUAL, LONG]);
        let r = readings(&lines);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].ms, vec![34.0, 33.0, 82.0, 40.0]);
        assert_eq!(r[0].draws, Some(vec![3500.0, 1200.0, 4000.0, 3100.0]));
        assert_eq!(r[0].usual_draws, Some(2600.0));
        // Frames are counted one by one by their own draws, not the mean's.
        let c = count(&r, 3000, None);
        assert_eq!(c.ms, vec![34.0, 82.0, 40.0]);
        assert_eq!(c.by_reading, 0);
        assert_eq!(c.draws, 10_600.0);
    }

    #[test]
    fn colour_does_not_hide_a_reading() {
        let coloured = format!("\u{1b}[2m{}\u{1b}[0m", TIMES.replace(" INFO", " \u{1b}[32mINFO\u{1b}[0m"));
        let lines: Vec<String> = [coloured.as_str(), DRAWS].iter().map(|l| clean(l).into_owned()).collect();
        assert_eq!(readings(&lines)[0].ms.len(), 4);
    }

    #[test]
    fn an_older_usual_line_has_its_draws_too() {
        let older = USUAL.replace(" | 2600 draws, 2000 with the preceding draw's pipeline | ", " | 2600 draws | ");
        assert_eq!(usual_draws(parse(&older).unwrap().rest), Some(2600.0));
        assert_eq!(usual_draws(parse(USUAL).unwrap().rest), Some(2600.0));
    }

    #[test]
    fn an_older_log_is_counted_by_its_usual_frame() {
        let lines = owned(&[TIMES, USUAL]);
        let r = readings(&lines);
        assert_eq!(r[0].draws, None);
        let c = count(&r, 2500, None);
        assert_eq!((c.ms.len(), c.by_reading), (4, 4));
        assert!(count(&r, 3000, None).ms.is_empty());
    }

    #[test]
    fn a_long_frame_line_is_not_a_sample() {
        // Only the "each" lists give frames: the usual and long lines are not counted again.
        let lines = owned(&[TIMES, DRAWS, USUAL, LONG]);
        assert_eq!(count(&readings(&lines), 0, None).ms.len(), 4);
    }

    #[test]
    fn a_pipeline_is_an_anchor_by_its_field() {
        let other = VERTEX.replace("0x222cf05a070", "0x1").replace("dfafaf385b2f94e8", "1111111111111111");
        // Another pipeline's line comes between the two of the first.
        let lines = owned(&[VERTEX, &other, PIXEL]);
        let found = anchors(&lines);
        assert_eq!(found, vec![(0, Anchor { geometry: "dfafaf385b2f94e8".into(), pixel: "06295fb1d1aefb78".into() })]);
    }

    #[test]
    fn named_anchors_are_found_in_both_logs() {
        let lines = owned(&[VERTEX, PIXEL]);
        let parsed = anchors(&lines);
        let anchor = parse_anchor_arg("dfafaf385b2f94e8:06295fb1d1aefb78").unwrap();
        assert_eq!(named_bounds(std::slice::from_ref(&anchor), &parsed, &parsed).unwrap(), ((0, 0), (0, 0)));
        let missing = parse_anchor_arg("ffffffffffffffff:ffffffffffffffff").unwrap();
        assert!(named_bounds(&[missing], &parsed, &parsed).is_err());
    }

    #[test]
    fn workload_is_summed_between_bounds() {
        let lines = owned(&[PRESENT, PRESENT]);
        assert_eq!(work(&lines, Some((0, 1))), Work { draws: 3856, drawn: 3856, dispatches: 516, dispatched: 516 });
        assert_eq!(work(&lines, Some((1, 1))), Work { draws: 1928, drawn: 1928, dispatches: 258, dispatched: 258 });
    }

    fn anchor(pixel: &str) -> Anchor {
        Anchor { geometry: "x".into(), pixel: pixel.into() }
    }

    #[test]
    fn shared_bounds_use_the_first_and_last_common_anchor_in_order() {
        let a = [(0, anchor("1")), (5, anchor("2")), (9, anchor("3"))];
        let b = [(0, anchor("1")), (4, anchor("2"))];
        assert_eq!(shared_bounds(&a, &b), Some(((0, 5), (0, 4))));
        // B has them the other way round: no range runs backwards.
        let b = [(0, anchor("2")), (4, anchor("1"))];
        assert_eq!(shared_bounds(&a, &b), None);
    }

    #[test]
    fn a_real_log_is_measured_between_its_anchors() {
        // A log as collect-ab.ps1 keeps it (a byte order mark on line 1), with
        // a line of the title's own output and other pipelines in between.
        let other_v = VERTEX.replace("0x222cf05a070", "0x2").replace("dfafaf385b2f94e8", "2222222222222222");
        let other_p = PIXEL.replace("0x222cf05a070", "0x2").replace("06295fb1d1aefb78", "3333333333333333");
        let early = TIMES.replace("each: 34 33 82 40", "each: 500 600 700 800");
        let text = [
            format!("\u{feff}{early}"),
            DRAWS.to_owned(),
            VERTEX.to_owned(),
            "title output that is not a log line 12 ms | 9000 draws".to_owned(),
            PIXEL.to_owned(),
            TIMES.to_owned(),
            DRAWS.to_owned(),
            USUAL.to_owned(),
            PRESENT.to_owned(),
            other_v,
            other_p,
            early,
            DRAWS.to_owned(),
        ];
        let lines: Vec<String> = text.iter().map(|l| clean(l).into_owned()).collect();
        let found = anchors(&lines);
        assert_eq!(found.len(), 2);
        let bounds = shared_bounds(&found, &found).unwrap();
        assert_eq!(bounds.0, (2, 9));
        let r = readings(&lines);
        assert_eq!(r.len(), 3);
        // Only the reading between the anchors, by its own frames' draws.
        let c = count(&r, 3000, Some(bounds.0));
        assert_eq!(c.ms, vec![34.0, 82.0, 40.0]);
        assert_eq!(c.readings, 1);
        let w = work(&lines, Some(bounds.0));
        assert_eq!((w.draws, w.drawn, w.dispatches), (1928, 1928, 258));
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let v: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(percentile(&v, 0.50), 5.0);
        assert_eq!(percentile(&v, 0.95), 10.0);
        assert_eq!(percentile(&v, 0.99), 10.0);
        let s = summarise(&[3.0, 1.0, 2.0]);
        assert_eq!((s.max, s.mean), (3.0, 2.0));
    }
}
