//! `DocumentOutline` reads an outline as a flat list with each item's page and view, skips what it
//! cannot read, and bounds every walk.

use lopdf::{
    DestinationView, Dictionary, Document, DocumentOutline, Object, ObjectId, OutlineItem, OutlineLimits,
    OutlineTarget, StringFormat, dictionary,
};

#[test]
fn nested_items_read_in_order_with_their_depth_and_view() {
    let (doc, _) = with_outline(3, |pages| {
        vec![
            item("A")
                .dest(explicit(pages[0], "XYZ", &[72.into(), 700.into(), Object::Null]))
                .kids(vec![
                    item("A.1").dest(explicit(pages[1], "FitH", &[500.into()])),
                    item("A.2")
                        .dest(explicit(pages[1], "Fit", &[]))
                        .kids(vec![item("A.2.a").dest(explicit(
                            pages[2],
                            "XYZ",
                            &[Object::Null, Object::Null, Object::Null],
                        ))]),
                ]),
            item("B").dest(explicit(
                pages[2],
                "FitR",
                &[0.into(), 10.into(), 100.into(), 110.into()],
            )),
        ]
    });

    let outline = DocumentOutline::read(&doc).unwrap();

    assert_eq!(
        outline.items,
        vec![
            entry(
                "A",
                0,
                Some(target(
                    1,
                    DestinationView::Xyz {
                        left: Some(72.0),
                        top: Some(700.0),
                        zoom: None
                    }
                ))
            ),
            entry("A.1", 1, Some(target(2, DestinationView::FitH { top: Some(500.0) }))),
            entry("A.2", 1, Some(target(2, DestinationView::Fit))),
            entry(
                "A.2.a",
                2,
                Some(target(
                    3,
                    DestinationView::Xyz {
                        left: None,
                        top: None,
                        zoom: None
                    }
                ))
            ),
            entry(
                "B",
                0,
                Some(target(
                    3,
                    DestinationView::FitR {
                        left: 0.0,
                        bottom: 10.0,
                        right: 100.0,
                        top: 110.0
                    }
                ))
            ),
        ]
    );
    assert_eq!(outline.skipped, 0);
    assert!(!outline.truncated);
}

#[test]
fn goto_actions_and_named_destinations_resolve_and_other_actions_do_not() {
    let (mut doc, pages) = with_outline(3, |pages| {
        vec![
            item("goto").action(dictionary! { "S" => "GoTo", "D" => explicit(pages[1], "Fit", &[]) }),
            item("uri").action(dictionary! { "S" => "URI", "URI" => Object::string_literal("https://example.com") }),
            item("remote").action(dictionary! {
                "S" => "GoToR",
                "F" => Object::string_literal("other.pdf"),
                "D" => vec![0.into(), "Fit".into()],
            }),
            item("by name").dest(Object::Name(b"Chapter1".to_vec())),
            item("by string").dest(Object::string_literal("sec2")),
            item("unknown name").dest(Object::string_literal("missing")),
        ]
    });
    let leaf_low = doc.add_object(dictionary! {
        "Limits" => vec![Object::string_literal("a"), Object::string_literal("m")],
        "Names" => vec![Object::string_literal("apple"), explicit(pages[0], "Fit", &[])],
    });
    let section = doc.add_object(dictionary! { "D" => explicit(pages[1], "FitH", &[300.into()]) });
    let leaf_high = doc.add_object(dictionary! {
        "Limits" => vec![Object::string_literal("n"), Object::string_literal("z")],
        "Names" => vec![Object::string_literal("sec2"), Object::Reference(section)],
    });
    let dests_tree = doc.add_object(dictionary! {
        "Kids" => vec![Object::Reference(leaf_low), Object::Reference(leaf_high)],
    });
    let catalog = doc.catalog_mut().unwrap();
    catalog.set(
        "Dests",
        dictionary! { "Chapter1" => explicit(pages[2], "XYZ", &[0.into(), 600.into(), 0.into()]) },
    );
    catalog.set("Names", dictionary! { "Dests" => Object::Reference(dests_tree) });

    let outline = DocumentOutline::read(&doc).unwrap();

    let targets: Vec<(&str, Option<OutlineTarget>)> = outline
        .items
        .iter()
        .map(|item| (item.title.as_str(), item.target))
        .collect();
    assert_eq!(
        targets,
        vec![
            ("goto", Some(target(2, DestinationView::Fit))),
            ("uri", None),
            ("remote", None),
            (
                "by name",
                Some(target(
                    3,
                    DestinationView::Xyz {
                        left: Some(0.0),
                        top: Some(600.0),
                        zoom: Some(0.0)
                    }
                ))
            ),
            ("by string", Some(target(2, DestinationView::FitH { top: Some(300.0) }))),
            ("unknown name", None),
        ]
    );
}

#[test]
fn titles_decode_from_every_text_encoding() {
    let (doc, _) = with_outline(1, |_| {
        vec![
            titled(Object::String(
                b"\xFE\xFF\x00C\x00a\x00f\x00\xE9".to_vec(),
                StringFormat::Hexadecimal,
            )),
            titled(Object::String(
                b"\xFF\xFEC\x00a\x00f\x00\xE9\x00".to_vec(),
                StringFormat::Hexadecimal,
            )),
            titled(Object::String(
                b"\xEF\xBB\xBFCaf\xC3\xA9".to_vec(),
                StringFormat::Literal,
            )),
            titled(Object::String(b"Caf\xE9 \x84 dash".to_vec(), StringFormat::Literal)),
            titled(Object::string_literal("  padded\r\n")),
            titled(Object::String(b"Two-line\rheading".to_vec(), StringFormat::Literal)),
            titled(Object::String(
                b"\xFE\xFF\x00A\x00\n\x00\n\x00B".to_vec(),
                StringFormat::Hexadecimal,
            )),
        ]
    });

    let titles: Vec<String> = DocumentOutline::read(&doc)
        .unwrap()
        .items
        .into_iter()
        .map(|item| item.title)
        .collect();

    assert_eq!(
        titles,
        [
            "Café",
            "Café",
            "Café",
            "Café \u{2014} dash",
            "padded",
            "Two-line heading",
            "A B"
        ]
    );
}

#[test]
fn page_indices_count_from_zero_and_other_targets_leave_the_item_without_one() {
    let (mut doc, _) = with_outline(3, |pages| {
        vec![
            item("page index").dest(vec![1.into(), "Fit".into()].into()),
            item("index past the end").dest(vec![99.into(), "Fit".into()].into()),
            item("no view").dest(vec![Object::Reference(pages[0])].into()),
            item("not a page"),
        ]
    });
    let not_a_page = doc.add_object(dictionary! { "Type" => "Font" });
    let last_item = top_level_item_ids(&doc)[3];
    doc.get_dictionary_mut(last_item)
        .unwrap()
        .set("Dest", vec![Object::Reference(not_a_page), "Fit".into()]);

    let outline = DocumentOutline::read(&doc).unwrap();

    let targets: Vec<Option<OutlineTarget>> = outline.items.iter().map(|item| item.target).collect();
    assert_eq!(
        targets,
        [
            Some(target(2, DestinationView::Fit)),
            None,
            Some(target(1, DestinationView::Unknown)),
            None
        ]
    );
}

#[test]
fn cycles_end_the_walk_and_are_counted() {
    let (mut doc, _) = with_outline(1, |_| vec![item("A"), item("B").kids(vec![item("B.1")])]);
    let first = first_item_id(&doc);
    let second = doc
        .get_dictionary(first)
        .unwrap()
        .get(b"Next")
        .unwrap()
        .as_reference()
        .unwrap();
    let child = doc
        .get_dictionary(second)
        .unwrap()
        .get(b"First")
        .unwrap()
        .as_reference()
        .unwrap();
    // B.1 leads back to A as its next sibling, and to B as its child.
    let child_node = doc.get_dictionary_mut(child).unwrap();
    child_node.set("Next", Object::Reference(first));
    child_node.set("First", Object::Reference(second));

    let outline = DocumentOutline::read(&doc).unwrap();

    let titles: Vec<&str> = outline.items.iter().map(|item| item.title.as_str()).collect();
    assert_eq!(titles, ["A", "B", "B.1"]);
    assert_eq!(outline.skipped, 2);
}

#[test]
fn a_cyclic_name_tree_ends_the_lookup() {
    let (mut doc, _) = with_outline(1, |_| vec![item("named").dest(Object::string_literal("x"))]);
    let tree = doc.new_object_id();
    doc.objects.insert(
        tree,
        Object::Dictionary(dictionary! { "Kids" => vec![Object::Reference(tree)] }),
    );
    doc.catalog_mut()
        .unwrap()
        .set("Names", dictionary! { "Dests" => Object::Reference(tree) });

    let outline = DocumentOutline::read(&doc).unwrap();

    assert_eq!(outline.items.len(), 1);
    assert_eq!(outline.items[0].target, None);
}

#[test]
fn limits_truncate_the_walk() {
    let (deep, _) = with_outline(1, |_| {
        vec![item("0").kids(vec![item("1").kids(vec![item("2").kids(vec![item("3")])])])]
    });
    let shallow = DocumentOutline::read_with_limits(
        &deep,
        OutlineLimits {
            max_depth: 2,
            max_items: 100,
        },
    )
    .unwrap();
    assert_eq!(shallow.items.iter().map(|item| item.depth).collect::<Vec<_>>(), [0, 1]);
    assert!(shallow.truncated);

    let (wide, _) = with_outline(1, |_| (0..5).map(|index| item(&index.to_string())).collect());
    let short = DocumentOutline::read_with_limits(
        &wide,
        OutlineLimits {
            max_depth: 32,
            max_items: 3,
        },
    )
    .unwrap();
    assert_eq!(short.items.len(), 3);
    assert!(short.truncated);
}

#[test]
fn a_document_without_an_outline_has_no_items() {
    let (mut doc, _) = with_outline(1, |_| Vec::new());
    assert_eq!(DocumentOutline::read(&doc).unwrap(), DocumentOutline::default());

    doc.catalog_mut().unwrap().remove(b"Outlines");
    assert_eq!(DocumentOutline::read(&doc).unwrap(), DocumentOutline::default());
}

#[test]
fn an_item_without_a_title_is_skipped_but_its_children_are_read() {
    let (mut doc, _) = with_outline(1, |_| vec![item("parent").kids(vec![item("child")]), item("next")]);
    let first = first_item_id(&doc);
    doc.get_dictionary_mut(first).unwrap().remove(b"Title");

    let outline = DocumentOutline::read(&doc).unwrap();

    let read: Vec<(&str, usize)> = outline
        .items
        .iter()
        .map(|item| (item.title.as_str(), item.depth))
        .collect();
    assert_eq!(read, [("child", 1), ("next", 0)]);
    assert_eq!(outline.skipped, 1);
}

#[cfg(all(feature = "lazy-reader", not(feature = "async")))]
#[test]
fn the_lazy_and_eager_documents_read_the_same_outline() {
    use lopdf::lazy::LazyDocument;
    use lopdf::xref::XrefType;
    use lopdf::{LoadOptions, SaveOptions};

    for (object_streams, xref_streams) in [(false, false), (true, true)] {
        let (mut doc, _) = with_outline(4, |pages| {
            vec![
                item("One")
                    .dest(explicit(pages[0], "XYZ", &[0.into(), 720.into(), Object::Null]))
                    .kids(vec![item("One.a").dest(explicit(pages[1], "FitH", &[400.into()]))]),
                item("Two").action(dictionary! { "S" => "GoTo", "D" => explicit(pages[3], "Fit", &[]) }),
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

        let eager = DocumentOutline::read(&Document::load_mem(&bytes).unwrap()).unwrap();
        let lazy = DocumentOutline::read(&LazyDocument::from_source(bytes.as_slice(), LoadOptions::default()).unwrap())
            .unwrap();

        assert_eq!(eager.items.len(), 3);
        assert_eq!(lazy, eager);
    }
}

struct Item {
    title: Option<Object>,
    dest: Option<Object>,
    action: Option<Dictionary>,
    kids: Vec<Item>,
}

fn item(title: &str) -> Item {
    titled(Object::string_literal(title))
}

fn titled(title: Object) -> Item {
    Item {
        title: Some(title),
        dest: None,
        action: None,
        kids: Vec::new(),
    }
}

impl Item {
    fn dest(mut self, dest: Object) -> Self {
        self.dest = Some(dest);
        self
    }

    fn action(mut self, action: Dictionary) -> Self {
        self.action = Some(action);
        self
    }

    fn kids(mut self, kids: Vec<Item>) -> Self {
        self.kids = kids;
        self
    }
}

fn explicit(page: ObjectId, view: &str, args: &[Object]) -> Object {
    let mut array = vec![Object::Reference(page), Object::Name(view.as_bytes().to_vec())];
    array.extend_from_slice(args);
    Object::Array(array)
}

fn entry(title: &str, depth: usize, target: Option<OutlineTarget>) -> OutlineItem {
    OutlineItem {
        title: title.to_owned(),
        depth,
        target,
    }
}

fn target(page: u32, view: DestinationView) -> OutlineTarget {
    OutlineTarget { page, view }
}

/// A document of `page_count` pages whose outline holds the items `items` builds from the page
/// ids.
fn with_outline(page_count: usize, items: impl FnOnce(&[ObjectId]) -> Vec<Item>) -> (Document, Vec<ObjectId>) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let page_ids: Vec<ObjectId> = (0..page_count)
        .map(|_| doc.add_object(dictionary! { "Type" => "Page", "Parent" => Object::Reference(pages_id) }))
        .collect();
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => page_count as i64,
            "Kids" => page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
        }),
    );
    let outlines_id = doc.new_object_id();
    let mut outlines = dictionary! { "Type" => "Outlines" };
    if let Some((first, last)) = add_items(&mut doc, outlines_id, items(&page_ids)) {
        outlines.set("First", Object::Reference(first));
        outlines.set("Last", Object::Reference(last));
    }
    doc.objects.insert(outlines_id, Object::Dictionary(outlines));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
        "Outlines" => Object::Reference(outlines_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    (doc, page_ids)
}

/// Adds `items` as the children of `parent`, and returns the first and last child.
fn add_items(doc: &mut Document, parent: ObjectId, items: Vec<Item>) -> Option<(ObjectId, ObjectId)> {
    let ids: Vec<ObjectId> = items.iter().map(|_| doc.new_object_id()).collect();
    for (index, item) in items.into_iter().enumerate() {
        let mut node = dictionary! { "Parent" => Object::Reference(parent) };
        if let Some(title) = item.title {
            node.set("Title", title);
        }
        if index > 0 {
            node.set("Prev", Object::Reference(ids[index - 1]));
        }
        if let Some(next) = ids.get(index + 1) {
            node.set("Next", Object::Reference(*next));
        }
        if let Some(dest) = item.dest {
            node.set("Dest", dest);
        }
        if let Some(action) = item.action {
            node.set("A", action);
        }
        if let Some((first, last)) = add_items(doc, ids[index], item.kids) {
            node.set("First", Object::Reference(first));
            node.set("Last", Object::Reference(last));
        }
        doc.objects.insert(ids[index], Object::Dictionary(node));
    }
    Some((*ids.first()?, *ids.last()?))
}

fn first_item_id(doc: &Document) -> ObjectId {
    let outlines = doc.catalog().unwrap().get(b"Outlines").unwrap().as_reference().unwrap();
    doc.get_dictionary(outlines)
        .unwrap()
        .get(b"First")
        .unwrap()
        .as_reference()
        .unwrap()
}

fn top_level_item_ids(doc: &Document) -> Vec<ObjectId> {
    let mut ids = vec![first_item_id(doc)];
    while let Ok(next) = doc
        .get_dictionary(*ids.last().unwrap())
        .unwrap()
        .get(b"Next")
        .and_then(Object::as_reference)
    {
        ids.push(next);
    }
    ids
}
