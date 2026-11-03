//! Opt-in comparison with WordCraft (https://github.com/storytold/wordcraft): converts every
//! fixture with docxide-pdf and with `wordcraft-cli convert`, scores both against the Word reference
//! with the suite's own Jaccard and SSIM, and writes per-page scores, page counts, PDF sizes and
//! timings to tests/output/wordcraft_comparison.json.
//!
//! Run: `WORDCRAFT_CLI=/path/to/wordcraft-cli cargo test --release --test suite wordcraft_comparison -- --nocapture`
//! `WC_LIST=<file>` scores only the `group/case` keys listed in it, one per line; `DOCXIDE_CASE` and
//! `DOCXIDE_GROUP` filter as in the other suites.
use crate::common;

use image::DynamicImage;
use rayon::prelude::*;
use serde::Serialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Serialize, Default, Clone)]
struct Engine {
    ok: bool,
    err: Option<String>,
    ms: f64,
    pages: Option<usize>,
    /// Mean over the reference pages the engine also produced (the visual suite's method).
    jaccard: Option<f64>,
    ssim: Option<f64>,
    /// Same, but a missing page counts as 0.
    jaccard_strict: Option<f64>,
    ssim_strict: Option<f64>,
    page_jaccard: Vec<Option<f64>>,
    page_ssim: Vec<Option<f64>>,
    pdf_bytes: Option<u64>,
}

#[derive(Serialize)]
struct Row {
    name: String,
    group: String,
    case: String,
    ref_pages: usize,
    docxide: Engine,
    wordcraft: Engine,
}

fn wordcraft_convert(cli: &Path, input: &Path, out: &Path) -> Result<f64, String> {
    let t = Instant::now();
    let mut child = Command::new(cli)
        .arg("convert")
        .arg(input)
        .arg(out)
        .env("WORDCRAFT_NO_PREFS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    // Drained on its own thread: a full stderr pipe would block the converter until the timeout.
    let stderr = child.stderr.take();
    let stderr = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            let _ = e.read_to_string(&mut s);
        }
        s
    });
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if t.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timeout {}s", TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    if !status.success() {
        let stderr = stderr.join().unwrap_or_default();
        let tail: String = stderr
            .chars()
            .rev()
            .take(400)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(format!(
            "exit {}: {}",
            status.code().unwrap_or(-1),
            tail.trim()
        ));
    }
    if !out.exists() {
        return Err("no output file".into());
    }
    Ok(ms)
}

fn docxide_convert(input: &Path, out: &Path) -> Result<f64, String> {
    let t = Instant::now();
    common::convert(input, out)?;
    Ok(t.elapsed().as_secs_f64() * 1000.0)
}

fn score(ref_pages: &[PathBuf], gen_pdf: &Path, gen_dir: &Path, diff_dir: &Path, e: &mut Engine) {
    e.pdf_bytes = fs::metadata(gen_pdf).ok().map(|m| m.len());
    e.pages = common::pdf_page_count(gen_pdf).ok();
    if let Err(err) = common::ensure_screenshots(gen_pdf, gen_dir) {
        e.err = Some(format!("raster: {err}"));
    }
    let gen_pages = common::collect_page_pngs(gen_dir).unwrap_or_default();
    let _ = fs::create_dir_all(diff_dir);
    let per: Vec<(Option<f64>, Option<f64>)> = (0..ref_pages.len())
        .into_par_iter()
        .map(|i| {
            let (Ok(r), Some(Ok(g))) = (
                image::open(&ref_pages[i]),
                gen_pages.get(i).map(image::open),
            ) else {
                return (None, None);
            };
            let j = common::compare_and_diff(&r, &g).ok().map(|pr| {
                if let Some(stem) = ref_pages[i].file_stem().and_then(|s| s.to_str()) {
                    let _ = DynamicImage::ImageRgba8(pr.diff_img)
                        .save(diff_dir.join(format!("{stem}.png")));
                }
                pr.jaccard
            });
            (j, common::ssim_score(&r, &g).ok())
        })
        .collect();
    e.page_jaccard = per.iter().map(|p| p.0).collect();
    e.page_ssim = per.iter().map(|p| p.1).collect();
    e.jaccard = mean(per.iter().filter_map(|p| p.0));
    e.ssim = mean(per.iter().filter_map(|p| p.1));
    e.jaccard_strict = mean(per.iter().map(|p| p.0.unwrap_or(0.0)));
    e.ssim_strict = mean(per.iter().map(|p| p.1.unwrap_or(0.0)));
}

fn mean(v: impl Iterator<Item = f64>) -> Option<f64> {
    let v: Vec<f64> = v.collect();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

#[test]
fn wordcraft_comparison() {
    let Some(cli) = std::env::var_os("WORDCRAFT_CLI").map(PathBuf::from) else {
        println!("wordcraft_comparison skipped — opt in with WORDCRAFT_CLI=/path/to/wordcraft-cli");
        return;
    };
    let mut fixtures = common::discover_fixtures().expect("Failed to read tests/fixtures");
    if let Some(list) = std::env::var_os("WC_LIST") {
        let keep: std::collections::HashSet<String> = fs::read_to_string(list)
            .expect("WC_LIST")
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        fixtures.retain(|f| keep.contains(&common::case_key(f)));
    }
    println!("{} fixtures", fixtures.len());

    // Conversions run one at a time, so the timings aren't distorted by contention.
    let mut conv: Vec<(PathBuf, Engine, Engine)> = Vec::new();
    for (n, f) in fixtures.iter().enumerate() {
        let input = f.join("input.docx");
        if !f.join("reference.pdf").exists() || !input.exists() {
            continue;
        }
        let out = common::output_dir(f);
        let _ = fs::create_dir_all(&out);
        let mut dx = Engine::default();
        match docxide_convert(&input, &out.join("docxide.pdf")) {
            Ok(ms) => (dx.ok, dx.ms) = (true, ms),
            Err(e) => dx.err = Some(e),
        }
        let mut wc = Engine::default();
        match wordcraft_convert(&cli, &input, &out.join("wordcraft.pdf")) {
            Ok(ms) => (wc.ok, wc.ms) = (true, ms),
            Err(e) => wc.err = Some(e),
        }
        eprintln!(
            "[{n}/{}] {} docxide={:.0}ms wordcraft={:.0}ms {}",
            fixtures.len(),
            common::case_key(f),
            dx.ms,
            wc.ms,
            wc.err.clone().unwrap_or_default()
        );
        conv.push((f.clone(), dx, wc));
    }

    // Rasterising and scoring run in parallel.
    let mut rows: Vec<Row> = conv
        .into_par_iter()
        .filter_map(|(f, mut dx, mut wc)| {
            let out = common::output_dir(&f);
            let ref_dir = out.join("reference");
            common::ensure_screenshots(&f.join("reference.pdf"), &ref_dir).ok()?;
            let ref_pages = common::collect_page_pngs(&ref_dir).ok()?;
            if dx.ok {
                score(
                    &ref_pages,
                    &out.join("docxide.pdf"),
                    &out.join("docxide"),
                    &out.join("docxide_diff"),
                    &mut dx,
                );
            }
            if wc.ok {
                score(
                    &ref_pages,
                    &out.join("wordcraft.pdf"),
                    &out.join("wordcraft"),
                    &out.join("wordcraft_diff"),
                    &mut wc,
                );
            }
            Some(Row {
                name: common::case_key(&f),
                group: common::group_name(&f),
                case: f.file_name()?.to_string_lossy().into_owned(),
                ref_pages: ref_pages.len(),
                docxide: dx,
                wordcraft: wc,
            })
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    let pct = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{:.1}%", v * 100.0));
    let engines: [(&str, fn(&Row) -> &Engine); 2] = [
        ("docxide-pdf", |r| &r.docxide),
        ("WordCraft", |r| &r.wordcraft),
    ];
    for (label, e) in engines {
        let pages_ok = rows
            .iter()
            .filter(|r| e(r).pages == Some(r.ref_pages))
            .count();
        println!(
            "  {label:<12} Jaccard {:>6}  SSIM {:>6}  pages matching {pages_ok}/{}  failed {}",
            pct(mean(rows.iter().filter_map(|r| e(r).jaccard))),
            pct(mean(rows.iter().filter_map(|r| e(r).ssim))),
            rows.len(),
            rows.iter().filter(|r| !e(r).ok).count(),
        );
    }

    let json = serde_json::to_string_pretty(&rows).expect("json");
    fs::write("tests/output/wordcraft_comparison.json", json).expect("write");
    println!(
        "wrote tests/output/wordcraft_comparison.json ({} rows)",
        rows.len()
    );
}
