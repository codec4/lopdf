//! A page's text in runs, each with the font that set it, read into the forms the page draws.

#![cfg(feature = "lazy-reader")]

use std::collections::BTreeMap;

use lopdf::content::{Content, Operation};
use lopdf::lazy::LazyDocument;
use lopdf::{
    DecodeLimits, Document, LoadOptions, Object, ObjectId, PageTextRun, SaveOptions, Stream, StringFormat, dictionary,
};

const LIMIT: usize = 16 * 1024 * 1024;

fn literal(text: &str) -> Object {
    Object::String(text.as_bytes().to_vec(), StringFormat::Literal)
}

fn show(font: &str, text: &str) -> Vec<Operation> {
    vec![
        Operation::new("BT", vec![]),
        Operation::new("Tf", vec![font.into(), 12.into()]),
        Operation::new("Tj", vec![literal(text)]),
        Operation::new("ET", vec![]),
    ]
}

fn draw(name: &str) -> Operation {
    Operation::new("Do", vec![name.into()])
}

fn stream(dict: lopdf::Dictionary, operations: Vec<Operation>) -> Stream {
    Stream::new(dict, Content { operations }.encode().unwrap())
}

fn font(doc: &mut Document, base: &str) -> ObjectId {
    doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => base,
        "Encoding" => "WinAnsiEncoding",
    })
}

fn font_with_differences(doc: &mut Document, base: &str, differences: Vec<Object>) -> ObjectId {
    doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => base,
        "Encoding" => dictionary! { "Type" => "Encoding", "Differences" => differences },
    })
}

/// One page: text in two fonts, then a form with its own font, a form with no resources, a form
/// that draws itself, and an image, then text in two fonts with `/Differences`.
fn fixture() -> (Vec<u8>, ObjectId) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let body = font(&mut doc, "ABCDEF+TimesLTStd-Roman");
    let maths = font(&mut doc, "MathematicalPiLTStd-3");
    let form_font = font(&mut doc, "MathematicalPiLTStd-1");
    // Names from a type foundry's catalogue, which have no Unicode reading.
    let catalogue = font_with_differences(
        &mut doc,
        "MathematicalPi-One-Italic",
        vec![33.into(), "H9266".into(), "H9258".into()],
    );
    let minus = font_with_differences(&mut doc, "MathematicalPiLTStd-5", vec![33.into(), "minus".into()]);

    let own = doc.add_object(stream(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "F9" => form_font } },
        },
        show("F9", "21"),
    ));
    let bare = doc.add_object(stream(
        dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()] },
        show("F1", "inherited"),
    ));
    let looping = doc.new_object_id();
    doc.objects.insert(
        looping,
        Object::Stream(stream(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                "Resources" => dictionary! {
                    "Font" => dictionary! { "F1" => body },
                    "XObject" => dictionary! { "Me" => looping },
                },
            },
            [show("F1", "once"), vec![draw("Me")]].concat(),
        )),
    );
    // Bytes that would read as text, were an image read as content.
    let image = doc.add_object(stream(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => 5, "Height" => 5,
            "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
        },
        show("F1", "pixels"),
    ));

    let content = doc.add_object(stream(
        dictionary! {},
        [
            show("F1", "Hello"),
            show("F2", "sdk"),
            vec![draw("Own"), draw("Bare"), draw("Loop"), draw("Img")],
            show("F3", "!\""),
            vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F4".into(), 12.into()]),
                Operation::new(
                    "TJ",
                    vec![Object::Array(vec![literal("!"), (-200).into(), literal("!")])],
                ),
                Operation::new("ET", vec![]),
            ],
        ]
        .concat(),
    ));
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content,
        "Resources" => dictionary! {
            "Font" => dictionary! { "F1" => body, "F2" => maths, "F3" => catalogue, "F4" => minus },
            "XObject" => dictionary! { "Own" => own, "Bare" => bare, "Loop" => looping, "Img" => image },
        },
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_with_options(&mut bytes, SaveOptions::default()).unwrap();
    (bytes, page)
}

fn runs(bytes: &[u8], page: ObjectId) -> Vec<PageTextRun> {
    let lazy = LazyDocument::from_source(bytes, LoadOptions::default()).unwrap();
    lazy.extract_page_text_runs_with_limits(page, DecodeLimits::uniform(LIMIT))
        .unwrap()
}

#[test]
fn each_run_names_its_font_and_the_forms_a_page_draws_are_read() {
    let (bytes, page) = fixture();

    assert_eq!(
        runs(&bytes, page)
            .into_iter()
            .map(|run| (String::from_utf8(run.font).unwrap(), run.text.trim().to_owned()))
            .collect::<Vec<_>>(),
        [
            ("TimesLTStd-Roman", "Hello"),
            // A maths font's letters, which only its name tells apart from prose.
            ("MathematicalPiLTStd-3", "sdk"),
            // A form's text, in the form's own font.
            ("MathematicalPiLTStd-1", "21"),
            // A form with no resources of its own draws in the page's.
            ("TimesLTStd-Roman", "inherited"),
            // A form that draws itself is read once, and an image not at all.
            ("TimesLTStd-Roman", "once"),
            ("MathematicalPi-One-Italic", "!\""),
            ("MathematicalPiLTStd-5", "\u{2212} \u{2212}"),
        ]
        .map(|(font, text)| (font.to_owned(), text.to_owned()))
    );
}

#[test]
fn a_run_keeps_the_codes_it_showed_and_the_names_its_font_gives_them() {
    let (bytes, page) = fixture();
    let runs = runs(&bytes, page);
    let run = |font: &str| runs.iter().find(|run| run.font == font.as_bytes()).unwrap();
    let names = |pairs: &[(u8, &str)]| {
        pairs
            .iter()
            .map(|&(code, name)| (code, name.as_bytes().to_vec()))
            .collect::<BTreeMap<_, _>>()
    };

    // The text falls back to the standard encoding, but the names are still there to read.
    let catalogue = run("MathematicalPi-One-Italic");
    assert_eq!(catalogue.codes, [(0, b'!'), (1, b'"')]);
    assert_eq!(*catalogue.differences, names(&[(33, "H9266"), (34, "H9258")]));

    // A kerned space falls between codes, and the minus sign takes three bytes of the text.
    let minus = run("MathematicalPiLTStd-5");
    assert_eq!(minus.text, "\u{2212} \u{2212} \n");
    assert_eq!(minus.codes, [(0, b'!'), (4, b'!')]);
    assert_eq!(*minus.differences, names(&[(33, "minus")]));

    let prose = run("TimesLTStd-Roman");
    assert_eq!(prose.codes[..2], [(0, b'H'), (1, b'e')]);
    assert!(prose.differences.is_empty());
}

/// One page whose content sets its maths inside `q ... Q`, then goes on in the font `Q` restores,
/// and draws a form that shows text without setting a font of its own.
fn graphics_state_fixture() -> (Vec<u8>, ObjectId) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let body = font(&mut doc, "TimesLTStd-Roman");
    let maths = font(&mut doc, "MathematicalPiLTStd-3");
    let form = doc.add_object(stream(
        dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()] },
        vec![
            Operation::new("BT", vec![]),
            Operation::new("Tj", vec![literal("inherited")]),
            Operation::new("ET", vec![]),
        ],
    ));
    let content = doc.add_object(stream(
        dictionary! {},
        [
            show("F1", "Let"),
            vec![Operation::new("q", vec![])],
            show("F2", "sxd"),
            vec![Operation::new("Q", vec![])],
            vec![
                Operation::new("BT", vec![]),
                Operation::new("Tj", vec![literal("be")]),
                Operation::new("ET", vec![]),
                draw("Fm"),
            ],
        ]
        .concat(),
    ));
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content,
        "Resources" => dictionary! {
            "Font" => dictionary! { "F1" => body, "F2" => maths },
            "XObject" => dictionary! { "Fm" => form },
        },
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_with_options(&mut bytes, SaveOptions::default()).unwrap();
    (bytes, page)
}

#[test]
fn the_font_follows_the_graphics_state_through_q_and_into_forms() {
    let (bytes, page) = graphics_state_fixture();

    assert_eq!(
        runs(&bytes, page)
            .into_iter()
            .map(|run| (String::from_utf8(run.font).unwrap(), run.text.trim().to_owned()))
            .collect::<Vec<_>>(),
        [
            ("TimesLTStd-Roman", "Let"),
            ("MathematicalPiLTStd-3", "sxd"),
            // `Q` restores the font `q` saved, so what follows is prose again, not maths.
            ("TimesLTStd-Roman", "be"),
            // A form draws in the graphics state of the content that draws it.
            ("TimesLTStd-Roman", "inherited"),
        ]
        .map(|(font, text)| (font.to_owned(), text.to_owned()))
    );
}

#[test]
fn on_pages_without_forms_the_runs_hold_the_page_text_exactly() {
    for asset in [
        "example.pdf",
        "Incremental.pdf",
        "AnnotationDemo.pdf",
        "test.pdf",
        "unicode.pdf",
    ] {
        let bytes = std::fs::read(format!("assets/{asset}")).unwrap();
        let lazy = LazyDocument::from_source(bytes.as_slice(), LoadOptions::default()).unwrap();
        for (number, id) in lazy.get_pages().unwrap() {
            let text = lazy
                .extract_page_text_with_limits(id, DecodeLimits::uniform(LIMIT))
                .unwrap();
            let runs = lazy
                .extract_page_text_runs_with_limits(id, DecodeLimits::uniform(LIMIT))
                .unwrap();
            let joined: String = runs.iter().map(|run| run.text.as_str()).collect();
            assert_eq!(joined, text, "{asset}, page {number}");
        }
    }
}
