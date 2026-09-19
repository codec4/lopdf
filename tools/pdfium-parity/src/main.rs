//! Compares this lopdf fork with pdfium, the library behind the Android PDF renderer, on generated
//! fixtures, the fork's sample PDFs, and any PDFs or folders given on the command line or listed
//! in `QNN_PDF_CORPUS` (separated by `:`).
//!
//! For each PDF it checks that both read the same page count, the same displayed page sizes and
//! rotations, the same page labels, and the same outline titles, depths, and pages, and that each destination's view
//! puts the same position at the top of the window. Where an entry has a position on its page,
//! it also finds the heading's text there and reports how far below the position it starts: a
//! PDF whose destinations miss their headings is listed, but only generated fixtures fail on it.
//!
//! Usage: `pdfium-parity --pdfium <libpdfium> [PDF or folder]...`

mod compare;
mod fixtures;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use compare::{KnownDifference, Report};
use pdfium_render::prelude::Pdfium;

/// Heading offsets allowed in the generated fixtures, whose headings sit just below their
/// positions.
const FIXTURE_HEADING_OFFSETS: std::ops::RangeInclusive<f64> = 0.0..=0.05;

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let mut library = None;
    let mut inputs = Vec::new();
    while let Some(argument) = arguments.next() {
        if argument == "--pdfium" {
            library = arguments.next();
        } else {
            inputs.push(PathBuf::from(argument));
        }
    }
    let Some(library) = library else {
        eprintln!("usage: pdfium-parity --pdfium <libpdfium> [PDF or folder]...");
        return ExitCode::from(2);
    };
    let pdfium = match Pdfium::bind_to_library(&library) {
        Ok(bindings) => Pdfium::new(bindings),
        Err(error) => {
            eprintln!("cannot load pdfium from {library}: {error}");
            return ExitCode::from(2);
        }
    };
    if let Ok(corpus) = std::env::var("QNN_PDF_CORPUS") {
        inputs.extend(corpus.split(':').filter(|path| !path.is_empty()).map(PathBuf::from));
    }

    let fixtures_directory = tempfile::tempdir().expect("create a temporary folder");
    let fixtures = fixtures::write_all(fixtures_directory.path());
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    let mut corpus = Vec::new();
    collect_pdfs(&assets, &mut corpus);
    for input in &inputs {
        collect_pdfs(input, &mut corpus);
    }

    let mut failed = 0;
    let mut unreadable = 0;
    for (path, is_fixture) in fixtures
        .iter()
        .map(|path| (path, true))
        .chain(corpus.iter().map(|path| (path, false)))
    {
        let mut report = compare::compare(&pdfium, path);
        if is_fixture {
            check_fixture(path, &mut report);
        }
        print_report(path, &report);
        if report.unreadable.is_some() {
            unreadable += 1;
        } else if !report.mismatches.is_empty() {
            failed += 1;
        }
    }
    let total = fixtures.len() + corpus.len();
    println!(
        "\n{total} PDFs: {} match pdfium, {failed} differ, {unreadable} unreadable by both",
        total - failed - unreadable
    );
    if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Holds a generated fixture to what it was built to contain: headings just below their
/// positions, and one of each known difference in the positions fixture.
fn check_fixture(path: &Path, report: &mut Report) {
    let headings = &report.headings;
    if headings.not_found > 0 {
        report
            .mismatches
            .push(format!("{} headings not found", headings.not_found));
    }
    for offset in &headings.offsets {
        if !FIXTURE_HEADING_OFFSETS.contains(offset) {
            report
                .mismatches
                .push(format!("a heading is {offset:.3} of the page from its position"));
        }
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if name.contains("positions") {
        for difference in [KnownDifference::UntitledItem, KnownDifference::RemoteDestination] {
            if report.known.get(&difference) != Some(&1) {
                report
                    .mismatches
                    .push(format!("expected one of the {}", difference.describe()));
            }
        }
    }
}

fn print_report(path: &Path, report: &Report) {
    let name: String = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .replace('\n', " ")
        .chars()
        .take(44)
        .collect();
    if let Some(reason) = &report.unreadable {
        println!("{name:44} unreadable by both ({reason})");
        return;
    }
    let verdict = if report.mismatches.is_empty() {
        "match"
    } else {
        "DIFFER"
    };
    let mut line = format!(
        "{name:44} {verdict:6} pages {:>4}{}  outline {:>4}  positioned {:>4}",
        report.pages,
        if report.labelled { " labelled" } else { "         " },
        report.outline_items,
        report.positioned
    );
    let headings = &report.headings;
    if headings.checked > 0 {
        let mut offsets = headings.offsets.clone();
        offsets.sort_by(f64::total_cmp);
        let median = offsets.get(offsets.len() / 2).copied().unwrap_or(f64::NAN);
        line.push_str(&format!(
            "  headings {}/{} found, median {median:+.3}, {} off",
            headings.offsets.len(),
            headings.checked,
            headings.far.len()
        ));
    }
    for (difference, count) in &report.known {
        line.push_str(&format!("  [{count} {}]", difference.describe()));
    }
    println!("{line}");
    for mismatch in report.mismatches.iter().take(10) {
        println!("    differs: {mismatch}");
    }
    if report.mismatches.len() > 10 {
        println!("    ... and {} more differences", report.mismatches.len() - 10);
    }
    for far in headings.far.iter().take(3) {
        println!("    heading off its position: {far}");
    }
}

/// Adds `path` if it is a PDF, or the PDFs under it if it is a folder, in name order.
fn collect_pdfs(path: &Path, pdfs: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        let mut children: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        children.sort();
        for child in children {
            collect_pdfs(&child, pdfs);
        }
    } else if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        pdfs.push(path.to_path_buf());
    }
}
