//! `PageLinkReader` reads a page's link annotations: where each sits, one rectangle per line of a
//! link that wraps, and where it leads, through the same destinations the outline resolves. What
//! a reader cannot follow is skipped and counted, and a long `/Annots` is bounded.

use lopdf::{
    DestinationView, Dictionary, Document, LinkTarget, Object, ObjectId, OutlineTarget, PageLink, PageLinkLimits,
    PageLinkReader, PageLinks, PageRect, dictionary,
};

#[test]
fn goto_links_lead_to_their_page_and_view() {
    let (doc, _) = with_links(3, dictionary! {}, |pages| {
        vec![
            link(rect(10, 700, 200, 720)).with(
                "Dest",
                explicit(pages[1], "XYZ", &[Object::Null, 600.into(), Object::Null]),
            ),
            link(rect(10, 650, 200, 670)).with(
                "A",
                dictionary! { "S" => "GoTo", "D" => explicit(pages[2], "FitH", &[400.into()]) },
            ),
            // Some producers write the page as an index counted from zero.
            link(rect(10, 600, 200, 620)).with("Dest", Object::Array(vec![1.into(), "Fit".into()])),
        ]
    });

    let links = read(&doc, 1);

    assert_eq!(
        links.links,
        vec![
            to_page(
                rect_of(10.0, 700.0, 200.0, 720.0),
                2,
                DestinationView::Xyz {
                    left: None,
                    top: Some(600.0),
                    zoom: None
                }
            ),
            to_page(
                rect_of(10.0, 650.0, 200.0, 670.0),
                3,
                DestinationView::FitH { top: Some(400.0) }
            ),
            to_page(rect_of(10.0, 600.0, 200.0, 620.0), 2, DestinationView::Fit),
        ]
    );
    assert_eq!(links.skipped, 0);
    assert!(!links.truncated);
}

#[test]
fn named_destinations_resolve_through_the_catalog_and_its_name_tree() {
    // The contents page of a long book names its chapters, and a long book keeps its names in a
    // tree with kids rather than one flat list.
    let (doc, _) = with_links(3, dictionary! {}, |_| {
        vec![
            link(rect(10, 700, 200, 720)).with(
                "A",
                dictionary! { "S" => "GoTo", "D" => Object::string_literal("chapter.two") },
            ),
            link(rect(10, 650, 200, 670)).with("Dest", Object::Name(b"old.style".to_vec())),
        ]
    });
    let doc = with_catalog(doc, |doc, pages| {
        let leaf = doc.add_object(dictionary! {
            "Limits" => vec![Object::string_literal("chapter.one"), Object::string_literal("chapter.two")],
            "Names" => vec![
                Object::string_literal("chapter.one"), explicit(pages[1], "Fit", &[]),
                Object::string_literal("chapter.two"), explicit(pages[2], "Fit", &[]),
            ],
        });
        let tree = doc.add_object(dictionary! { "Kids" => vec![Object::Reference(leaf)] });
        dictionary! {
            "Names" => dictionary! { "Dests" => tree },
            // PDF 1.1's catalog dictionary of names, which some producers still write.
            "Dests" => dictionary! { "old.style" => dictionary! { "D" => explicit(pages[1], "Fit", &[]) } },
        }
    });

    let links = read(&doc, 1);

    assert_eq!(
        links.links,
        vec![
            to_page(rect_of(10.0, 700.0, 200.0, 720.0), 3, DestinationView::Fit),
            to_page(rect_of(10.0, 650.0, 200.0, 670.0), 2, DestinationView::Fit),
        ]
    );
}

#[test]
fn web_links_keep_their_address() {
    let (doc, _) = with_links(1, dictionary! {}, |_| {
        vec![link(rect(10, 700, 200, 720)).with(
            "A",
            dictionary! { "S" => "URI", "URI" => Object::string_literal(" http://www.hoddereducation.co.uk ") },
        )]
    });

    assert_eq!(
        read(&doc, 1).links,
        vec![PageLink {
            rects: vec![rect_of(10.0, 700.0, 200.0, 720.0)],
            target: LinkTarget::Uri("http://www.hoddereducation.co.uk".to_owned()),
        }]
    );
}

#[test]
fn a_link_that_wraps_has_a_rectangle_for_each_line() {
    let (doc, _) = with_links(2, dictionary! {}, |pages| {
        let to_two = explicit(pages[1], "Fit", &[]);
        vec![
            // Two lines, each written as a quadrilateral whose corners come in any order.
            link(rect(10, 600, 300, 720)).with("Dest", to_two.clone()).with(
                "QuadPoints",
                numbers(&[
                    200, 720, 300, 720, 200, 700, 300, 700, 10, 690, 120, 690, 10, 670, 120, 670,
                ]),
            ),
            // A quadrilateral with a hole in it is no rectangle at all, so the link's /Rect stands.
            link(rect(10, 600, 300, 620)).with("Dest", to_two).with(
                "QuadPoints",
                Object::Array(vec![
                    10.into(),
                    "x".into(),
                    3.into(),
                    4.into(),
                    5.into(),
                    6.into(),
                    7.into(),
                    8.into(),
                ]),
            ),
        ]
    });

    let rects: Vec<Vec<PageRect>> = read(&doc, 1).links.into_iter().map(|link| link.rects).collect();

    assert_eq!(
        rects,
        vec![
            vec![rect_of(200.0, 700.0, 300.0, 720.0), rect_of(10.0, 670.0, 120.0, 690.0)],
            vec![rect_of(10.0, 600.0, 300.0, 620.0)],
        ]
    );
}

#[test]
fn links_a_reader_cannot_follow_are_skipped_and_counted() {
    let (doc, _) = with_links(1, dictionary! {}, |pages| {
        let to_one = explicit(pages[0], "Fit", &[]);
        vec![
            link(rect(10, 700, 200, 720)).with("Dest", to_one.clone()).with("F", 2),
            link(rect(10, 700, 200, 720)).with("Dest", to_one.clone()).with("F", 32),
            link(rect(10, 700, 200, 720)).with(
                "A",
                dictionary! { "S" => "GoToR", "F" => Object::string_literal("other.pdf") },
            ),
            link(rect(10, 700, 200, 720)).with("Dest", Object::Array(vec![40.into(), "Fit".into()])),
            link(rect(10, 700, 10, 720)).with("Dest", to_one.clone()),
            Object::Dictionary(dictionary! { "Type" => "Annot", "Subtype" => "Link", "Dest" => to_one }),
            // Not a link, so neither read nor counted.
            Object::Dictionary(dictionary! { "Type" => "Annot", "Subtype" => "Text", "Rect" => rect(0, 0, 10, 10) }),
        ]
    });

    let links = read(&doc, 1);

    assert!(links.links.is_empty());
    assert_eq!(links.skipped, 6);
}

#[test]
fn annots_may_be_referenced_and_hold_annotations_in_place() {
    let (mut doc, pages) = with_links(2, dictionary! {}, |_| Vec::new());
    let to_two = explicit(pages[1], "Fit", &[]);
    let referenced = doc.add_object(link(rect(10, 650, 200, 670)).with("Dest", to_two.clone()));
    let annots = doc.add_object(Object::Array(vec![
        Object::Reference(referenced),
        link(rect(10, 700, 200, 720)).with("Dest", to_two),
    ]));
    doc.get_object_mut(pages[0])
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Annots", annots);

    assert_eq!(read(&doc, 1).links.len(), 2);
}

#[test]
fn a_page_without_links_and_a_page_outside_the_document_have_none() {
    let (doc, _) = with_links(2, dictionary! {}, |_| Vec::new());
    let mut reader = PageLinkReader::new(&doc).unwrap();

    assert_eq!(reader.page_count(), 2);
    assert_eq!(reader.read(2).unwrap(), PageLinks::default());
    assert_eq!(reader.read(0).unwrap(), PageLinks::default());
    assert_eq!(reader.read(3).unwrap(), PageLinks::default());
}

#[test]
fn a_long_annots_is_read_up_to_the_limit() {
    let (doc, _) = with_links(1, dictionary! {}, |pages| {
        (0..3)
            .map(|_| link(rect(10, 700, 200, 720)).with("Dest", explicit(pages[0], "Fit", &[])))
            .collect()
    });
    let limits = PageLinkLimits {
        max_annotations: 2,
        ..PageLinkLimits::default()
    };

    let links = PageLinkReader::with_limits(&doc, limits).unwrap().read(1).unwrap();

    assert_eq!(links.links.len(), 2);
    assert!(links.truncated);
}

#[test]
fn the_lazy_and_eager_documents_read_the_same_links() {
    use lopdf::lazy::LazyDocument;
    use lopdf::xref::XrefType;
    use lopdf::{LoadOptions, SaveOptions};

    for (object_streams, xref_streams) in [(false, false), (true, true)] {
        let (mut doc, _) = with_links(3, dictionary! {}, |pages| {
            vec![
                link(rect(10, 700, 200, 720))
                    .with("Dest", explicit(pages[2], "XYZ", &[0.into(), 500.into(), Object::Null])),
                link(rect(10, 650, 200, 670)).with(
                    "A",
                    dictionary! { "S" => "URI", "URI" => Object::string_literal("https://example.com") },
                ),
            ]
        });
        doc.reference_table.cross_reference_type = if xref_streams {
            XrefType::CrossReferenceStream
        } else {
            XrefType::CrossReferenceTable
        };
        let options = SaveOptions::builder()
            .use_object_streams(object_streams)
            .use_xref_streams(xref_streams)
            .build();
        let mut bytes = Vec::new();
        doc.save_with_options(&mut bytes, options).unwrap();

        let eager_doc = Document::load_mem(&bytes).unwrap();
        let lazy_doc = LazyDocument::from_source(bytes.as_slice(), LoadOptions::default()).unwrap();
        let eager = PageLinkReader::new(&eager_doc).unwrap().read(1).unwrap();
        let lazy = PageLinkReader::new(&lazy_doc).unwrap().read(1).unwrap();

        assert_eq!(eager.links.len(), 2);
        assert_eq!(lazy, eager);
    }
}

fn read(doc: &Document, page: u32) -> PageLinks {
    PageLinkReader::new(doc).unwrap().read(page).unwrap()
}

/// A link annotation over `rect`, written in place in `/Annots`.
fn link(rect: Object) -> Object {
    Object::Dictionary(dictionary! { "Type" => "Annot", "Subtype" => "Link", "Rect" => rect })
}

trait With {
    /// The annotation with `key` set to `value`.
    fn with(self, key: &str, value: impl Into<Object>) -> Self;
}

impl With for Object {
    fn with(mut self, key: &str, value: impl Into<Object>) -> Self {
        self.as_dict_mut().unwrap().set(key, value);
        self
    }
}

fn to_page(rect: PageRect, page: u32, view: DestinationView) -> PageLink {
    PageLink {
        rects: vec![rect],
        target: LinkTarget::Page(OutlineTarget { page, view }),
    }
}

fn explicit(page: ObjectId, view: &str, args: &[Object]) -> Object {
    let mut array = vec![Object::Reference(page), Object::Name(view.as_bytes().to_vec())];
    array.extend_from_slice(args);
    Object::Array(array)
}

fn rect(left: i64, bottom: i64, right: i64, top: i64) -> Object {
    Object::Array(vec![left.into(), bottom.into(), right.into(), top.into()])
}

fn numbers(values: &[i64]) -> Object {
    Object::Array(values.iter().map(|value| (*value).into()).collect())
}

fn rect_of(left: f32, bottom: f32, right: f32, top: f32) -> PageRect {
    PageRect {
        left,
        bottom,
        right,
        top,
    }
}

/// A document of `page_count` pages whose first page carries the annotations `annots` builds from
/// the page ids, with `tree` attributes on the page tree node.
fn with_links(
    page_count: usize, tree: Dictionary, annots: impl FnOnce(&[ObjectId]) -> Vec<Object>,
) -> (Document, Vec<ObjectId>) {
    let mut doc = Document::with_version("1.7");
    let tree_id = doc.new_object_id();
    let pages: Vec<ObjectId> = (0..page_count)
        .map(|_| {
            doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => tree_id,
                "MediaBox" => rect(0, 0, 600, 800),
            })
        })
        .collect();
    let mut tree = tree;
    tree.set("Type", "Pages");
    tree.set("Count", page_count as i64);
    tree.set("Kids", pages.iter().copied().map(Object::Reference).collect::<Vec<_>>());
    doc.objects.insert(tree_id, Object::Dictionary(tree));
    let annots = annots(&pages);
    if !annots.is_empty() {
        doc.get_object_mut(pages[0])
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Annots", annots);
    }
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree_id });
    doc.trailer.set("Root", catalog);
    (doc, pages)
}

/// `doc` with the entries `entries` builds added to its catalog.
fn with_catalog(mut doc: Document, entries: impl FnOnce(&mut Document, &[ObjectId]) -> Dictionary) -> Document {
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let entries = entries(&mut doc, &pages);
    let catalog = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    let catalog = doc.get_object_mut(catalog).unwrap().as_dict_mut().unwrap();
    for (key, value) in entries.into_iter() {
        catalog.set(key, value);
    }
    doc
}
