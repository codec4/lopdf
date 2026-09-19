//! Generated PDFs whose outline entries lead to headings drawn at known places, on pages with every
//! combination of box and rotation that changes where a position appears, and one with page labels.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat, dictionary};

/// A heading drawn at a baseline point in user space, in 20-point Helvetica.
struct Heading {
    text: &'static str,
    x: i64,
    y: i64,
}

struct Page {
    attributes: Dictionary,
    headings: Vec<Heading>,
}

/// Where an outline entry leads.
enum Target {
    /// An explicit destination: page index, view name, and view arguments.
    Explicit(usize, &'static str, Vec<Object>),
    /// A name from the catalog's `/Dests` dictionary.
    DestsName(&'static str),
    /// A string from the `/Names` `/Dests` name tree.
    TreeName(&'static str),
    /// A `/GoTo` action with an explicit destination.
    GoTo(usize, &'static str, Vec<Object>),
    /// A `/GoToR` action, which leads to another file.
    RemoteGoTo,
    /// A `/URI` action.
    Uri,
    None,
}

struct Entry {
    /// `None` leaves out `/Title`.
    title: Option<&'static str>,
    target: Target,
    children: Vec<Entry>,
}

fn entry(title: &'static str, target: Target) -> Entry {
    Entry {
        title: Some(title),
        target,
        children: Vec::new(),
    }
}

/// Writes the fixtures into `directory` and returns their paths. Each is written twice: with
/// classic cross-reference tables, and with object and cross-reference streams.
pub fn write_all(directory: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for (name, pages, named, entries) in [positions(), letter()] {
        for modern in [false, true] {
            let mut document = build(pages(), named(), entries());
            let suffix = if modern { "streams" } else { "classic" };
            let path = directory.join(format!("fixture-{name}-{suffix}.pdf"));
            let mut writer = BufWriter::new(File::create(&path).expect("create a fixture"));
            if modern {
                document.save_modern(&mut writer).expect("write a fixture");
            } else {
                document.save_to(&mut writer).expect("write a fixture");
            }
            paths.push(path);
        }
    }
    for (name, catch_all) in [("labels", false), ("labels-catch-all", true)] {
        let path = directory.join(format!("fixture-{name}.pdf"));
        let mut writer = BufWriter::new(File::create(&path).expect("create a fixture"));
        labelled(catch_all).save_modern(&mut writer).expect("write a fixture");
        paths.push(path);
    }
    paths
}

/// Sixteen blank pages whose labels use every style, a prefix alone, start values, and a UTF-16
/// prefix, with a page before the first range, a range that is not a dictionary, and a tree of
/// kids with limits, one of which lists a key out of order. With `catch_all`, a last kid has
/// limits that leave out its own key, which pdfium reads as giving every page no label.
fn labelled(catch_all: bool) -> Document {
    let pages = (0..16)
        .map(|_| Page {
            attributes: dictionary! {},
            headings: Vec::new(),
        })
        .collect();
    let mut document = build(pages, Vec::new(), Vec::new());
    let range = |style: Option<&str>, prefix: Option<Object>, start: Option<i64>| {
        let mut range = Dictionary::new();
        if let Some(style) = style {
            range.set("S", Object::Name(style.as_bytes().to_vec()));
        }
        if let Some(prefix) = prefix {
            range.set("P", prefix);
        }
        if let Some(start) = start {
            range.set("St", start);
        }
        Object::Dictionary(range)
    };
    let utf16 = Object::String(vec![0xfe, 0xff, 0x00, 0xc9, 0x00, 0x2d], StringFormat::Hexadecimal);
    let first = document.add_object(dictionary! {
        "Limits" => vec![1.into(), 5.into()],
        "Nums" => vec![
            1.into(), range(None, Some(Object::string_literal("Cover")), None),
            2.into(), range(Some("r"), None, None),
            5.into(), range(Some("R"), None, Some(1999)),
        ],
    });
    let second = document.add_object(dictionary! {
        "Limits" => vec![6.into(), 15.into()],
        "Nums" => vec![
            6.into(), range(Some("D"), Some(Object::string_literal("A-")), Some(9)),
            8.into(), range(Some("A"), None, Some(26)),
            10.into(), range(Some("a"), None, None),
            11.into(), Object::Integer(7),
            12.into(), range(Some("D"), Some(utf16), None),
            15.into(), range(Some("r"), None, None),
            14.into(), range(Some("R"), None, None),
        ],
    });
    let mut kids = vec![Object::Reference(first), Object::Reference(second)];
    if catch_all {
        kids.push(Object::Reference(document.add_object(dictionary! {
            "Limits" => vec![0.into(), 0.into()],
            "Nums" => vec![13.into(), range(Some("D"), Some(Object::string_literal("X-")), None)],
        })));
    }
    let labels = document.add_object(dictionary! { "Kids" => kids });
    let catalog = document
        .trailer
        .get(b"Root")
        .and_then(Object::as_reference)
        .expect("a catalog");
    document
        .get_dictionary_mut(catalog)
        .expect("a catalog")
        .set("PageLabels", labels);
    document
}

type Fixture = (
    &'static str,
    fn() -> Vec<Page>,
    fn() -> Vec<(&'static str, usize, &'static str, Vec<Object>)>,
    fn() -> Vec<Entry>,
);

/// Pages of 600 by 800 points: upright, cropped, and turned by 90, 180, and 270 degrees, then a
/// page for the fit views. Each entry's position sits just above its heading's text.
fn positions() -> Fixture {
    fn pages() -> Vec<Page> {
        let page = |attributes: Dictionary, headings: Vec<Heading>| Page { attributes, headings };
        let heading = |text, x, y| Heading { text, x, y };
        vec![
            page(dictionary! {}, vec![heading("Upright heading", 72, 690)]),
            page(
                dictionary! { "CropBox" => rect(50, 100, 550, 700) },
                vec![heading("Cropped heading", 72, 500)],
            ),
            // The displayed top of a turned page is its left edge.
            page(dictionary! { "Rotate" => 90 }, vec![heading("Turn 90", 150, 400)]),
            // Its bottom edge.
            page(dictionary! { "Rotate" => 180 }, vec![heading("Turn 180", 72, 200)]),
            // Its right edge; the heading is about 80 points wide.
            page(dictionary! { "Rotate" => -90 }, vec![heading("Turn 270", 200, 400)]),
            page(
                dictionary! {},
                vec![heading("FitH heading", 72, 500), heading("FitR heading", 72, 300)],
            ),
        ]
    }
    fn named() -> Vec<(&'static str, usize, &'static str, Vec<Object>)> {
        vec![
            ("dests-name", 0, "XYZ", vec![Object::Null, 712.into(), Object::Null]),
            ("tree-name", 5, "FitH", vec![520.into()]),
        ]
    }
    fn entries() -> Vec<Entry> {
        let mut upright = entry(
            "Upright heading",
            Target::Explicit(0, "XYZ", vec![72.into(), 712.into(), Object::Null]),
        );
        upright.children = vec![
            entry(
                "Cropped heading",
                Target::Explicit(1, "XYZ", vec![72.into(), 522.into(), 0.into()]),
            ),
            entry(
                "Turn 90",
                Target::Explicit(2, "XYZ", vec![140.into(), 420.into(), Object::Null]),
            ),
            entry(
                "Turn 180",
                Target::Explicit(3, "XYZ", vec![72.into(), 190.into(), Object::Null]),
            ),
            entry(
                "Turn 270",
                Target::Explicit(4, "XYZ", vec![290.into(), 420.into(), Object::Null]),
            ),
        ];
        let untitled = Entry {
            title: None,
            target: Target::Explicit(5, "Fit", vec![]),
            children: vec![entry("Child of an untitled entry", Target::Explicit(5, "Fit", vec![]))],
        };
        vec![
            upright,
            entry("FitH heading", Target::GoTo(5, "FitH", vec![520.into()])),
            entry(
                "FitR heading",
                Target::Explicit(5, "FitR", vec![72.into(), 250.into(), 400.into(), 322.into()]),
            ),
            entry("FitV view", Target::Explicit(5, "FitV", vec![72.into()])),
            entry("Fit view", Target::Explicit(5, "Fit", vec![])),
            entry("Upright heading", Target::DestsName("dests-name")),
            entry("FitH heading", Target::TreeName("tree-name")),
            entry("Missing name", Target::TreeName("no-such-name")),
            entry("Website", Target::Uri),
            entry("Another file", Target::RemoteGoTo),
            entry("No destination", Target::None),
            untitled,
            entry("Tab\tand  spaces \r\nin a title", Target::Explicit(0, "Fit", vec![])),
        ]
    }
    ("positions", pages, named, entries)
}

/// A page without a `/MediaBox`, which viewers show as US Letter.
fn letter() -> Fixture {
    fn pages() -> Vec<Page> {
        vec![Page {
            attributes: dictionary! {},
            headings: vec![Heading {
                text: "Letter heading",
                x: 72,
                y: 690,
            }],
        }]
    }
    fn named() -> Vec<(&'static str, usize, &'static str, Vec<Object>)> {
        Vec::new()
    }
    fn entries() -> Vec<Entry> {
        vec![entry(
            "Letter heading",
            Target::Explicit(0, "XYZ", vec![Object::Null, 712.into(), Object::Null]),
        )]
    }
    ("letter", pages, named, entries)
}

fn rect(left: i64, bottom: i64, right: i64, top: i64) -> Object {
    Object::Array(vec![left.into(), bottom.into(), right.into(), top.into()])
}

fn build(
    pages: Vec<Page>, named: Vec<(&'static str, usize, &'static str, Vec<Object>)>, entries: Vec<Entry>,
) -> Document {
    let mut document = Document::with_version("1.7");
    let tree_id = document.new_object_id();
    let font_id = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let page_ids: Vec<ObjectId> = pages
        .into_iter()
        .map(|page| {
            let mut operations = Vec::new();
            for heading in page.headings {
                operations.extend([
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 20.into()]),
                    Operation::new("Td", vec![heading.x.into(), heading.y.into()]),
                    Operation::new("Tj", vec![Object::string_literal(heading.text)]),
                    Operation::new("ET", vec![]),
                ]);
            }
            let content = Content { operations }.encode().expect("encode a page");
            let content_id = document.add_object(Stream::new(dictionary! {}, content));
            let mut dictionary = page.attributes;
            dictionary.set("Type", "Page");
            dictionary.set("Parent", tree_id);
            dictionary.set("Contents", content_id);
            dictionary.set("Resources", dictionary! { "Font" => dictionary! { "F1" => font_id } });
            document.add_object(dictionary)
        })
        .collect();
    document.objects.insert(
        tree_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => page_ids.len() as i64,
            "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
            "MediaBox" => rect(0, 0, 600, 800),
        }),
    );

    let explicit = |page: usize, view: &str, arguments: Vec<Object>| {
        let mut array = vec![
            Object::Reference(page_ids[page]),
            Object::Name(view.as_bytes().to_vec()),
        ];
        array.extend(arguments);
        Object::Array(array)
    };
    let mut dests = Dictionary::new();
    let mut tree_names = Vec::new();
    for (name, page, view, arguments) in named {
        let destination = explicit(page, view, arguments);
        if name.starts_with("dests") {
            dests.set(name, destination);
        } else {
            tree_names.push(Object::String(name.as_bytes().to_vec(), StringFormat::Literal));
            tree_names.push(dictionary! { "D" => destination }.into());
        }
    }

    let outlines_id = document.new_object_id();
    let (first, last) = add_entries(&mut document, outlines_id, entries, &explicit);
    let mut outlines = dictionary! { "Type" => "Outlines" };
    if let (Some(first), Some(last)) = (first, last) {
        outlines.set("First", first);
        outlines.set("Last", last);
    }
    document.objects.insert(outlines_id, Object::Dictionary(outlines));

    let mut catalog = dictionary! {
        "Type" => "Catalog",
        "Pages" => tree_id,
        "Outlines" => outlines_id,
    };
    if !dests.is_empty() {
        catalog.set("Dests", dests);
    }
    if !tree_names.is_empty() {
        catalog.set(
            "Names",
            dictionary! { "Dests" => dictionary! { "Names" => tree_names } },
        );
    }
    let catalog_id = document.add_object(catalog);
    document.trailer.set("Root", catalog_id);
    document
}

/// Adds `entries` as the children of `parent`, and returns the first and last child.
fn add_entries(
    document: &mut Document, parent: ObjectId, entries: Vec<Entry>,
    explicit: &dyn Fn(usize, &str, Vec<Object>) -> Object,
) -> (Option<ObjectId>, Option<ObjectId>) {
    let ids: Vec<ObjectId> = entries.iter().map(|_| document.new_object_id()).collect();
    for (index, entry) in entries.into_iter().enumerate() {
        let mut node = dictionary! { "Parent" => parent };
        if let Some(title) = entry.title {
            node.set("Title", Object::string_literal(title));
        }
        if index > 0 {
            node.set("Prev", ids[index - 1]);
        }
        if let Some(next) = ids.get(index + 1) {
            node.set("Next", *next);
        }
        match entry.target {
            Target::Explicit(page, view, arguments) => node.set("Dest", explicit(page, view, arguments)),
            Target::DestsName(name) => node.set("Dest", Object::Name(name.as_bytes().to_vec())),
            Target::TreeName(name) => node.set("Dest", Object::string_literal(name)),
            Target::GoTo(page, view, arguments) => node.set(
                "A",
                dictionary! { "S" => "GoTo", "D" => explicit(page, view, arguments) },
            ),
            Target::RemoteGoTo => node.set(
                "A",
                dictionary! {
                    "S" => "GoToR",
                    "F" => Object::string_literal("other.pdf"),
                    "D" => vec![0.into(), "Fit".into()],
                },
            ),
            Target::Uri => node.set(
                "A",
                dictionary! { "S" => "URI", "URI" => Object::string_literal("https://example.com") },
            ),
            Target::None => {}
        }
        let (first, last) = add_entries(document, ids[index], entry.children, explicit);
        if let (Some(first), Some(last)) = (first, last) {
            node.set("First", first);
            node.set("Last", last);
        }
        document.objects.insert(ids[index], Object::Dictionary(node));
    }
    (ids.first().copied(), ids.last().copied())
}
