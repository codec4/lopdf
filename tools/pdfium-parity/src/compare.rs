//! Compares one PDF as lopdf reads it with the same PDF as pdfium reads it.

use std::collections::BTreeMap;
use std::path::Path;

use lopdf::lazy::LazyDocument;
use lopdf::{DocumentOutline, ObjectResolver, OutlineLimits, PageGeometry, PageLabels};
use pdfium_render::prelude::*;

/// Points of difference allowed between two page sizes.
const SIZE_TOLERANCE: f32 = 0.01;
/// Fraction of a page's height allowed between two positions.
const POSITION_TOLERANCE: f64 = 0.001;
/// Device pixels per point used to turn pdfium's integer device coordinates into positions.
const DEVICE_SCALE: f32 = 100.0;

/// Differences between lopdf and pdfium that are intended, counted instead of failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KnownDifference {
    /// pdfium lists an outline item without a `/Title`, and lopdf leaves it out.
    UntitledItem,
    /// pdfium resolves a `/GoToR` action's page number against this file, and lopdf gives such
    /// an item no page, because it leads to another file.
    RemoteDestination,
}

impl KnownDifference {
    pub fn describe(self) -> &'static str {
        match self {
            Self::UntitledItem => "untitled outline items that lopdf leaves out",
            Self::RemoteDestination => "GoToR items that lopdf gives no page",
        }
    }
}

/// Where a heading's text sits compared to the position its outline entry leads to.
#[derive(Debug, Default)]
pub struct Headings {
    pub checked: usize,
    pub not_found: usize,
    /// Heading top minus entry position, as fractions of the page height.
    pub offsets: Vec<f64>,
    /// Entries whose heading is above their position, or far below it.
    pub far: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub pages: usize,
    /// Whether the document has page labels.
    pub labelled: bool,
    pub outline_items: usize,
    pub positioned: usize,
    pub mismatches: Vec<String>,
    pub known: BTreeMap<KnownDifference, usize>,
    pub headings: Headings,
    /// Set when neither reader opens the file, which is not a difference.
    pub unreadable: Option<String>,
}

/// One pdfium outline item, flattened in reading order.
struct PdfiumItem {
    title: Option<String>,
    depth: usize,
    page_index: Option<i32>,
    view: Option<PdfDestinationViewSettings>,
    remote: bool,
}

pub fn compare(pdfium: &Pdfium, path: &Path) -> Report {
    let mut report = Report::default();
    let lazy = LazyDocument::open(path);
    let document = pdfium.load_pdf_from_file(path, None);
    let (lazy, document) = match (lazy, document) {
        (Ok(lazy), Ok(document)) => (lazy, document),
        (Err(lopdf_error), Err(pdfium_error)) => {
            report.unreadable = Some(format!("lopdf: {lopdf_error}; pdfium: {pdfium_error}"));
            return report;
        }
        (Err(error), Ok(_)) => {
            report
                .mismatches
                .push(format!("only pdfium opens the file; lopdf: {error}"));
            return report;
        }
        (Ok(_), Err(error)) => {
            report
                .mismatches
                .push(format!("only lopdf opens the file; pdfium: {error}"));
            return report;
        }
    };

    let page_ids = match lazy.page_ids() {
        Ok(page_ids) => page_ids,
        Err(error) => {
            report
                .mismatches
                .push(format!("lopdf cannot read the page tree: {error}"));
            return report;
        }
    };
    let pages = document.pages();
    report.pages = page_ids.len();
    if page_ids.len() as i64 != i64::from(pages.len()) {
        report
            .mismatches
            .push(format!("page count: lopdf {}, pdfium {}", page_ids.len(), pages.len()));
        return report;
    }

    let their_labels: Vec<Option<String>> = (0..pages.len())
        .map(|index| pages.get(index).ok().and_then(|page| page.label().map(str::to_owned)))
        .collect();
    match PageLabels::read(&lazy, page_ids.len()) {
        Ok(None) => {
            if let Some(index) = their_labels.iter().position(Option::is_some) {
                report.mismatches.push(format!(
                    "page labels: lopdf none, pdfium {:?} on page {}",
                    their_labels[index],
                    index + 1
                ));
            }
        }
        Ok(Some(labels)) => {
            report.labelled = true;
            for (index, (ours, theirs)) in labels.labels.iter().zip(&their_labels).enumerate() {
                // pdfium gives no label where it would give an empty one.
                if ours != theirs.as_deref().unwrap_or_default() {
                    report
                        .mismatches
                        .push(format!("page {} label: lopdf {ours:?}, pdfium {theirs:?}", index + 1));
                }
            }
        }
        Err(error) => report
            .mismatches
            .push(format!("lopdf cannot read the page labels: {error}")),
    }

    let mut geometries = Vec::with_capacity(page_ids.len());
    for (index, page_id) in page_ids.iter().enumerate() {
        let geometry = match PageGeometry::read(&lazy, *page_id) {
            Ok(geometry) => geometry,
            Err(error) => {
                report
                    .mismatches
                    .push(format!("page {}: lopdf cannot read it: {error}", index + 1));
                return report;
            }
        };
        let page = match pages.get(index as i32) {
            Ok(page) => page,
            Err(error) => {
                report
                    .mismatches
                    .push(format!("page {}: pdfium cannot load it: {error}", index + 1));
                return report;
            }
        };
        let (width, height) = geometry.displayed_size();
        let turns = match page.rotation() {
            Ok(PdfPageRenderRotation::None) => 0,
            Ok(PdfPageRenderRotation::Degrees90) => 1,
            Ok(PdfPageRenderRotation::Degrees180) => 2,
            Ok(PdfPageRenderRotation::Degrees270) => 3,
            Err(_) => u8::MAX,
        };
        if (width - page.width().value).abs() > SIZE_TOLERANCE
            || (height - page.height().value).abs() > SIZE_TOLERANCE
            || turns != geometry.quarter_turns
        {
            report.mismatches.push(format!(
                "page {}: lopdf {width}x{height} turned {}, pdfium {}x{} turned {turns}",
                index + 1,
                geometry.quarter_turns,
                page.width().value,
                page.height().value,
            ));
        }
        geometries.push(geometry);
    }

    let outline = match DocumentOutline::read(&lazy) {
        Ok(outline) => outline,
        Err(error) => {
            report
                .mismatches
                .push(format!("lopdf cannot read the outline: {error}"));
            return report;
        }
    };
    let pdfium_items = pdfium_outline(&document);
    let mut expected = Vec::with_capacity(pdfium_items.len());
    for item in pdfium_items {
        if item.title.is_none() {
            *report.known.entry(KnownDifference::UntitledItem).or_default() += 1;
        } else {
            expected.push(item);
        }
    }
    report.outline_items = outline.items.len();
    if outline.items.len() != expected.len() {
        report.mismatches.push(format!(
            "outline items: lopdf {}, pdfium {} with a title",
            outline.items.len(),
            expected.len()
        ));
    }

    for (index, (ours, theirs)) in outline.items.iter().zip(&expected).enumerate() {
        let label = format!("outline item {} {:?}", index + 1, ours.title);
        let their_title = theirs.title.as_deref().map(collapse_whitespace).unwrap_or_default();
        if ours.title != their_title {
            report.mismatches.push(format!("{label}: pdfium title {their_title:?}"));
        }
        if ours.depth != theirs.depth {
            report
                .mismatches
                .push(format!("{label}: depth {}, pdfium {}", ours.depth, theirs.depth));
        }
        let our_page = ours.target.map(|target| target.page as usize - 1);
        let their_page = theirs.page_index.and_then(|index| usize::try_from(index).ok());
        if our_page != their_page {
            if our_page.is_none() && theirs.remote {
                *report.known.entry(KnownDifference::RemoteDestination).or_default() += 1;
            } else {
                report
                    .mismatches
                    .push(format!("{label}: page index {our_page:?}, pdfium {their_page:?}"));
            }
            continue;
        }
        let (Some(target), Some(page_index)) = (ours.target, our_page) else {
            continue;
        };
        let page = pages.get(page_index as i32).expect("pdfium loaded this page above");
        let our_position = geometries[page_index].top_fraction(target.view);
        let their_position = theirs.view.as_ref().and_then(|view| pdfium_top_fraction(&page, view));
        match (our_position, their_position) {
            (None, None) => {}
            (Some(ours), Some(theirs)) if (ours - theirs).abs() <= POSITION_TOLERANCE => {}
            _ => report
                .mismatches
                .push(format!("{label}: position {our_position:?}, pdfium {their_position:?}")),
        }
        if let Some(position) = our_position.filter(|position| (0.0..=1.0).contains(position)) {
            report.positioned += 1;
            check_heading(&mut report.headings, &page, &ours.title, position);
        }
    }
    report
}

/// pdfium's outline in reading order, bounded like lopdf's default [`OutlineLimits`] so that a
/// cyclic outline ends.
fn pdfium_outline(document: &PdfDocument<'_>) -> Vec<PdfiumItem> {
    let limits = OutlineLimits::default();
    let mut items = Vec::new();
    let bookmarks = document.bookmarks();
    let mut stack: Vec<(PdfBookmark<'_>, usize)> = Vec::new();
    if let Some(first) = bookmarks.root() {
        stack.push((first, 0));
    }
    while let Some((bookmark, depth)) = stack.pop() {
        if items.len() >= limits.max_items {
            break;
        }
        if let Some(next) = bookmark.next_sibling() {
            stack.push((next, depth));
        }
        if depth + 1 < limits.max_depth
            && let Some(child) = bookmark.first_child()
        {
            stack.push((child, depth + 1));
        }
        let destination = bookmark.destination();
        let remote = bookmark
            .action()
            .is_some_and(|action| action.action_type() == PdfActionType::GoToDestinationInRemoteDocument);
        items.push(PdfiumItem {
            title: bookmark.title(),
            depth,
            page_index: destination
                .as_ref()
                .and_then(|destination| destination.page_index().ok()),
            view: destination
                .as_ref()
                .and_then(|destination| destination.view_settings().ok()),
            remote,
        });
    }
    items
}

/// Where pdfium puts a view's position on the displayed page, as a fraction of its height from
/// the top. pdfium's own page-to-device transform decides which of the view's coordinates moves
/// the position down the displayed page, so this does not share lopdf's rotation logic.
fn pdfium_top_fraction(page: &PdfPage, view: &PdfDestinationViewSettings) -> Option<f64> {
    let (x, y) = match view {
        PdfDestinationViewSettings::SpecificCoordinatesAndZoom(x, y, _) => (*x, *y),
        PdfDestinationViewSettings::FitPageHorizontallyToWindow(y)
        | PdfDestinationViewSettings::FitBoundsHorizontallyToWindow(y) => (None, *y),
        PdfDestinationViewSettings::FitPageVerticallyToWindow(x)
        | PdfDestinationViewSettings::FitBoundsVerticallyToWindow(x) => (*x, None),
        PdfDestinationViewSettings::FitPageToRectangle(rect) => (Some(rect.left()), Some(rect.top())),
        _ => return None,
    };
    let device_y = |x: f32, y: f32| device_point(page, x, y).map(|(_, device_y)| device_y);
    let origin = device_y(0.0, 0.0)?;
    let x_moves_it = (device_y(100.0, 0.0)? - origin).abs() > 1.0;
    let y_moves_it = (device_y(0.0, 100.0)? - origin).abs() > 1.0;
    let coordinate = |value: Option<PdfPoints>, moves_it: bool| match value {
        Some(value) => Some(value.value),
        None if moves_it => None,
        None => Some(0.0),
    };
    let y = device_y(coordinate(x, x_moves_it)?, coordinate(y, y_moves_it)?)?;
    Some(y / f64::from(page.height().value * DEVICE_SCALE))
}

/// A point in user space mapped by pdfium onto the displayed page, scaled by [`DEVICE_SCALE`].
fn device_point(page: &PdfPage, x: f32, y: f32) -> Option<(f64, f64)> {
    let config = PdfRenderConfig::new().scale_page_by_factor(DEVICE_SCALE);
    let (device_x, device_y) = page
        .points_to_pixels(PdfPoints::new(x), PdfPoints::new(y), &config)
        .ok()?;
    Some((f64::from(device_x), f64::from(device_y)))
}

/// Finds the heading's text on the page, the match nearest to `position`, and records how far
/// below the position its top sits.
fn check_heading(headings: &mut Headings, page: &PdfPage, title: &str, position: f64) {
    headings.checked += 1;
    let needle = title_start(title);
    let Ok(text) = page.text() else {
        headings.not_found += 1;
        return;
    };
    let Ok(search) = text.search(&needle, &PdfSearchOptions::new()) else {
        headings.not_found += 1;
        return;
    };
    let height = f64::from(page.height().value * DEVICE_SCALE);
    let mut best: Option<f64> = None;
    while let Some(segments) = search.find_next() {
        let top = segments
            .iter()
            .filter_map(|segment| {
                let bounds = segment.bounds();
                [
                    (bounds.left(), bounds.top()),
                    (bounds.right(), bounds.top()),
                    (bounds.left(), bounds.bottom()),
                    (bounds.right(), bounds.bottom()),
                ]
                .into_iter()
                .filter_map(|(x, y)| device_point(page, x.value, y.value).map(|(_, device_y)| device_y / height))
                .reduce(f64::min)
            })
            .reduce(f64::min);
        if let Some(top) = top
            && best.is_none_or(|best| (top - position).abs() < (best - position).abs())
        {
            best = Some(top);
        }
    }
    match best {
        Some(top) => {
            let offset = top - position;
            headings.offsets.push(offset);
            if !(-0.02..=0.2).contains(&offset) {
                headings.far.push(format!(
                    "{title:?}: heading at {top:.3} of the page, position {position:.3}"
                ));
            }
        }
        None => headings.not_found += 1,
    }
}

/// The start of a title, up to a word boundary, short enough to sit on the heading's first line.
fn title_start(title: &str) -> String {
    let mut start = String::new();
    for word in title.split_whitespace() {
        if !start.is_empty() && start.len() + word.len() > 24 {
            break;
        }
        if !start.is_empty() {
            start.push(' ');
        }
        start.push_str(word);
    }
    start
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
