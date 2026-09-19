//! Page text read through the lazy reader matches the eager extractor exactly, and a page's
//! single-page document leaves out what extraction never reads.

#![cfg(feature = "lazy-reader")]

use lopdf::content::{Content, Operation};
use lopdf::lazy::LazyDocument;
use lopdf::xref::XrefType;
use lopdf::{
    DecodeLimits, Dictionary, Document, EncryptionState, EncryptionVersion, LoadOptions, Object, ObjectId, Permissions,
    SaveOptions, Stream, StringFormat, dictionary,
};

const LIMIT: usize = 16 * 1024 * 1024;

#[test]
fn every_page_matches_the_eager_extractor() {
    let fixture = Fixture::build();
    let mut sources: Vec<(String, Vec<u8>)> = Vec::new();
    for (label, object_streams, xref_streams) in [
        ("classic table", false, false),
        ("xref stream", false, true),
        ("object streams", true, true),
    ] {
        sources.push((
            label.to_owned(),
            save(&mut fixture.document.clone(), object_streams, xref_streams),
        ));
    }
    sources.push(("encrypted".to_owned(), encrypted(fixture.document.clone())));
    for asset in [
        "example.pdf",
        "Incremental.pdf",
        "AnnotationDemo.pdf",
        "test.pdf",
        "unicode.pdf",
        "encrypted.pdf",
    ] {
        sources.push((asset.to_owned(), std::fs::read(format!("assets/{asset}")).unwrap()));
    }

    for (label, bytes) in &sources {
        let eager = Document::load_mem(bytes).unwrap();
        let lazy = LazyDocument::from_source(bytes.as_slice(), LoadOptions::default()).unwrap();
        for (number, id) in eager.get_pages() {
            let expected = eager.extract_text_with_limit(&[number], LIMIT);
            let actual = lazy.extract_page_text_with_limits(id, DecodeLimits::uniform(LIMIT));
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"), "{label}, page {number}");
        }
    }

    // The fixture's pages hold the text each font decodes, so the comparison is not of empty text.
    let bytes = save(&mut fixture.document.clone(), true, true);
    let lazy = LazyDocument::from_source(bytes.as_slice(), LoadOptions::default()).unwrap();
    let text = |page: ObjectId| {
        lazy.extract_page_text_with_limits(page, DecodeLimits::uniform(LIMIT))
            .unwrap()
    };
    assert!(text(fixture.pages[0]).contains("Hello"));
    assert!(text(fixture.pages[0]).contains("BAAB"));
    assert!(text(fixture.pages[1]).contains("\u{c7}a \u{2713}"));
    assert!(text(fixture.pages[3]).contains("indirect"));
}

#[test]
fn a_single_page_document_leaves_out_font_programs_images_and_other_pages() {
    let fixture = Fixture::build();
    let bytes = save(&mut fixture.document.clone(), false, false);
    let lazy = LazyDocument::from_source(bytes.as_slice(), LoadOptions::default()).unwrap();

    let first = lazy.single_page_document(fixture.pages[0]).unwrap();
    assert_eq!(first.get_pages().len(), 1);
    assert!(!first.objects.contains_key(&fixture.font_program));
    assert!(!first.objects.contains_key(&fixture.image));
    for other in &fixture.pages[1..] {
        assert!(!first.objects.contains_key(other));
    }

    let image_page = lazy.single_page_document(fixture.pages[2]).unwrap();
    assert!(!image_page.objects.contains_key(&fixture.image));
}

#[test]
fn a_decompression_bomb_fails_the_page_as_in_the_eager_extractor() {
    let mut doc = Fixture::build().document;
    let bomb = {
        let mut stream = Stream::new(dictionary! {}, vec![b' '; 4 * 1024 * 1024]);
        stream.compress().unwrap();
        doc.add_object(stream)
    };
    let first = doc.get_pages()[&1];
    doc.get_dictionary_mut(first).unwrap().set("Contents", bomb);
    let bytes = save(&mut doc, true, true);
    let limit = 1024 * 1024;

    let eager = Document::load_mem(&bytes).unwrap().extract_text_with_limit(&[1], limit);
    let lazy = LazyDocument::from_source(bytes.as_slice(), LoadOptions::default())
        .unwrap()
        .extract_page_text_with_limits(first, DecodeLimits::uniform(limit));

    assert!(matches!(
        lazy,
        Err(lopdf::Error::Decompress(
            lopdf::DecompressError::MemoryLimitExceeded { .. }
        ))
    ));
    assert_eq!(format!("{lazy:?}"), format!("{eager:?}"));
}

/// Four pages under a tree whose `/Resources` the first page inherits:
/// 1. two content streams with a standard Type 1 font and a font with a `/Differences` encoding;
/// 2. its own resources with a Type 0 font, whose `/ToUnicode` CMap maps to non-Latin text and
///    whose descendant carries an embedded font program;
/// 3. only an image;
/// 4. contents given as a reference to an array of streams.
struct Fixture {
    document: Document,
    pages: Vec<ObjectId>,
    font_program: ObjectId,
    image: ObjectId,
}

impl Fixture {
    fn build() -> Self {
        let mut doc = Document::with_version("1.7");
        let tree_id = doc.new_object_id();

        let helvetica = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        });
        let swapped = doc.add_object(dictionary! {
            "Type" => "Encoding",
            "BaseEncoding" => "WinAnsiEncoding",
            "Differences" => vec![65.into(), "B".into(), "A".into()],
        });
        let differences_font = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Times-Roman",
            "Encoding" => swapped,
        });
        let inherited = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => helvetica, "F2" => differences_font },
        });

        let font_program = doc.add_object(Stream::new(dictionary! {}, vec![0x5a; 4096]));
        let descriptor = doc.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "Embedded",
            "FontFile2" => font_program,
        });
        let descendant = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "CIDFontType2",
            "BaseFont" => "Embedded",
            "FontDescriptor" => descriptor,
        });
        let cmap = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
            /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
            1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
            2 beginbfchar\n<0001> <00C700610020>\n<0002> <2713>\nendbfchar\n\
            endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n";
        let to_unicode = doc.add_object(Stream::new(dictionary! {}, cmap.as_bytes().to_vec()));
        let type0 = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => "Embedded",
            "Encoding" => "Identity-H",
            "DescendantFonts" => vec![Object::Reference(descendant)],
            "ToUnicode" => to_unicode,
        });

        let image = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1,
                "Height" => 1,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8,
            },
            vec![0x80],
        ));

        let mut content = |operations: Vec<Operation>| {
            let bytes = Content { operations }.encode().unwrap();
            doc.add_object(Stream::new(dictionary! {}, bytes))
        };
        let show = |font: &str, text: Object| {
            vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec![Object::Name(font.as_bytes().to_vec()), 12.into()]),
                Operation::new("Td", vec![72.into(), 700.into()]),
                Operation::new("Tj", vec![text]),
                Operation::new("ET", vec![]),
            ]
        };
        let first_contents = vec![
            Object::Reference(content(show("F1", Object::string_literal("Hello")))),
            Object::Reference(content(show("F2", Object::string_literal("ABBA")))),
        ];
        let second_contents = content(show(
            "F3",
            Object::String(vec![0x00, 0x01, 0x00, 0x02], StringFormat::Hexadecimal),
        ));
        let third_contents = content(vec![
            Operation::new("q", vec![]),
            Operation::new("cm", vec![10.into(), 0.into(), 0.into(), 10.into(), 0.into(), 0.into()]),
            Operation::new("Do", vec!["Im1".into()]),
            Operation::new("Q", vec![]),
        ]);
        let fourth_streams = vec![
            Object::Reference(content(show("F1", Object::string_literal("through an ")))),
            Object::Reference(content(show("F1", Object::string_literal("indirect array")))),
        ];
        let fourth_contents = doc.add_object(Object::Array(fourth_streams));

        let page = |doc: &mut Document, extra: Dictionary| {
            let mut page = dictionary! { "Type" => "Page", "Parent" => tree_id };
            for (key, value) in extra {
                page.set(key, value);
            }
            doc.add_object(page)
        };
        let pages = vec![
            page(&mut doc, dictionary! { "Contents" => first_contents }),
            page(
                &mut doc,
                dictionary! {
                    "Contents" => second_contents,
                    "Resources" => dictionary! { "Font" => dictionary! { "F3" => type0 } },
                },
            ),
            page(
                &mut doc,
                dictionary! {
                    "Contents" => third_contents,
                    "Resources" => dictionary! { "XObject" => dictionary! { "Im1" => image } },
                },
            ),
            page(&mut doc, dictionary! { "Contents" => fourth_contents }),
        ];
        doc.objects.insert(
            tree_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Count" => pages.len() as i64,
                "Kids" => pages.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
                "Resources" => inherited,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree_id });
        doc.trailer.set("Root", catalog);
        doc.compress();
        Self {
            document: doc,
            pages,
            font_program,
            image,
        }
    }
}

fn save(doc: &mut Document, object_streams: bool, xref_streams: bool) -> Vec<u8> {
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
    bytes
}

/// The document encrypted with an empty user password, which both readers open.
fn encrypted(mut doc: Document) -> Vec<u8> {
    doc.trailer.set(
        "ID",
        Object::Array(vec![
            Object::String(b"text-parity-id-1".to_vec(), StringFormat::Literal),
            Object::String(b"text-parity-id-2".to_vec(), StringFormat::Literal),
        ]),
    );
    let state = EncryptionState::try_from(EncryptionVersion::V2 {
        document: &doc,
        owner_password: "owner",
        user_password: "",
        key_length: 128,
        permissions: Permissions::all(),
    })
    .unwrap();
    doc.encrypt(&state).unwrap();
    save(&mut doc, false, false)
}
