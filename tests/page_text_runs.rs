//! A page's text in runs, each with the font that set it, read into the forms the page draws.

#![cfg(feature = "lazy-reader")]

use lopdf::content::{Content, Operation};
use lopdf::lazy::LazyDocument;
use lopdf::{DecodeLimits, Document, LoadOptions, Object, ObjectId, SaveOptions, Stream, StringFormat, dictionary};

const LIMIT: usize = 16 * 1024 * 1024;

fn show(font: &str, text: &str) -> Vec<Operation> {
    vec![
        Operation::new("BT", vec![]),
        Operation::new("Tf", vec![font.into(), 12.into()]),
        Operation::new(
            "Tj",
            vec![Object::String(text.as_bytes().to_vec(), StringFormat::Literal)],
        ),
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

/// One page: text in two fonts, then a form with its own font, a form with no resources, a form
/// that draws itself, and an image.
fn fixture() -> (Vec<u8>, ObjectId) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let body = font(&mut doc, "ABCDEF+TimesLTStd-Roman");
    let maths = font(&mut doc, "MathematicalPiLTStd-3");
    let form_font = font(&mut doc, "MathematicalPiLTStd-1");

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

fn runs(bytes: &[u8], page: ObjectId) -> Vec<(String, String)> {
    let lazy = LazyDocument::from_source(bytes, LoadOptions::default()).unwrap();
    lazy.extract_page_text_runs_with_limits(page, DecodeLimits::uniform(LIMIT))
        .unwrap()
        .into_iter()
        .map(|run| (String::from_utf8(run.font).unwrap(), run.text.trim().to_owned()))
        .collect()
}

#[test]
fn each_run_names_its_font_and_the_forms_a_page_draws_are_read() {
    let (bytes, page) = fixture();

    assert_eq!(
        runs(&bytes, page),
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
