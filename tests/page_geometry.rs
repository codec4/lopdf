//! `PageGeometry` reads a page's visible box and rotation as pdfium does, and measures where a
//! destination's view puts the top of the window on the displayed page.

use lopdf::{DestinationView, Dictionary, Document, Object, ObjectId, PageGeometry, PageRect, dictionary};

#[test]
fn the_visible_box_is_the_crop_box_clipped_to_the_media_box() {
    let (doc, pages) = with_pages(
        dictionary! { "MediaBox" => rect(0, 0, 600, 800) },
        vec![
            dictionary! {},
            dictionary! { "CropBox" => rect(550, 700, 50, 100) },
            dictionary! { "CropBox" => rect(-100, -100, 1000, 1000) },
            dictionary! { "CropBox" => rect(700, 900, 800, 1000) },
            dictionary! { "MediaBox" => Object::Null },
            dictionary! { "MediaBox" => rect(0, 0, 0, 100) },
        ],
    );

    let visible: Vec<PageRect> = pages
        .iter()
        .map(|page| PageGeometry::read(&doc, *page).unwrap().visible)
        .collect();

    assert_eq!(
        visible,
        [
            page_rect(0.0, 0.0, 600.0, 800.0),
            page_rect(50.0, 100.0, 550.0, 700.0),
            page_rect(0.0, 0.0, 600.0, 800.0),
            PageRect::default(),
            PageRect::LETTER,
            PageRect::LETTER,
        ]
    );
}

#[test]
fn rotation_is_inherited_and_counted_in_whole_quarter_turns() {
    let (doc, pages) = with_pages(
        dictionary! { "MediaBox" => rect(0, 0, 600, 800), "Rotate" => 90 },
        vec![
            dictionary! {},
            dictionary! { "Rotate" => 180 },
            dictionary! { "Rotate" => -90 },
            dictionary! { "Rotate" => 450 },
            dictionary! { "Rotate" => Object::Real(135.0) },
        ],
    );

    let turns: Vec<(u8, (f32, f32))> = pages
        .iter()
        .map(|page| {
            let geometry = PageGeometry::read(&doc, *page).unwrap();
            (geometry.quarter_turns, geometry.displayed_size())
        })
        .collect();

    assert_eq!(
        turns,
        [
            (1, (800.0, 600.0)),
            (2, (600.0, 800.0)),
            (3, (800.0, 600.0)),
            (1, (800.0, 600.0)),
            (1, (800.0, 600.0)),
        ]
    );
}

#[test]
fn views_are_measured_from_the_edge_displayed_on_top() {
    let upright = geometry(0);
    assert_eq!(upright.top_fraction(xyz(Some(72.0), Some(700.0))), Some(0.125));
    assert_eq!(
        upright.top_fraction(DestinationView::FitH { top: Some(400.0) }),
        Some(0.5)
    );
    assert_eq!(
        upright.top_fraction(DestinationView::FitR {
            left: 0.0,
            bottom: 0.0,
            right: 100.0,
            top: 600.0
        }),
        Some(0.25)
    );
    assert_eq!(upright.top_fraction(xyz(None, Some(900.0))), Some(-0.125));
    assert_eq!(upright.top_fraction(xyz(None, Some(-80.0))), Some(1.1));
    assert_eq!(upright.top_fraction(xyz(Some(72.0), None)), None);
    assert_eq!(upright.top_fraction(DestinationView::Fit), None);

    assert_eq!(geometry(1).top_fraction(xyz(Some(150.0), Some(700.0))), Some(0.25));
    assert_eq!(
        geometry(1).top_fraction(DestinationView::FitH { top: Some(400.0) }),
        None
    );
    assert_eq!(
        geometry(1).top_fraction(DestinationView::FitV { left: Some(300.0) }),
        Some(0.5)
    );
    assert_eq!(geometry(2).top_fraction(xyz(None, Some(200.0))), Some(0.25));
    assert_eq!(geometry(3).top_fraction(xyz(Some(150.0), None)), Some(0.75));

    let empty = PageGeometry {
        visible: PageRect::default(),
        quarter_turns: 0,
    };
    assert_eq!(empty.top_fraction(xyz(None, Some(10.0))), None);
}

fn geometry(quarter_turns: u8) -> PageGeometry {
    PageGeometry {
        visible: page_rect(0.0, 0.0, 600.0, 800.0),
        quarter_turns,
    }
}

fn xyz(left: Option<f32>, top: Option<f32>) -> DestinationView {
    DestinationView::Xyz { left, top, zoom: None }
}

fn page_rect(left: f32, bottom: f32, right: f32, top: f32) -> PageRect {
    PageRect {
        left,
        bottom,
        right,
        top,
    }
}

fn rect(left: i64, bottom: i64, right: i64, top: i64) -> Object {
    Object::Array(vec![left.into(), bottom.into(), right.into(), top.into()])
}

/// A document whose page tree node has `tree` attributes and one page per entry of `pages`.
fn with_pages(mut tree: Dictionary, pages: Vec<Dictionary>) -> (Document, Vec<ObjectId>) {
    let mut doc = Document::with_version("1.7");
    let tree_id = doc.new_object_id();
    let page_ids: Vec<ObjectId> = pages
        .into_iter()
        .map(|mut page| {
            page.set("Type", "Page");
            page.set("Parent", tree_id);
            doc.add_object(page)
        })
        .collect();
    tree.set("Type", "Pages");
    tree.set("Count", page_ids.len() as i64);
    tree.set(
        "Kids",
        page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
    );
    doc.objects.insert(tree_id, Object::Dictionary(tree));
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree_id });
    doc.trailer.set("Root", catalog_id);
    (doc, page_ids)
}
