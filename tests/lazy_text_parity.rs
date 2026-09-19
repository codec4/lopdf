//! Page text read through the lazy reader matches the eager extractor exactly, and a page's
//! single-page document leaves out what extraction never reads.

#![cfg(feature = "lazy-reader")]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use lopdf::content::{Content, Operation};
use lopdf::encryption::crypt_filters::{Aes128CryptFilter, Aes256CryptFilter, CryptFilter};
use lopdf::lazy::{LazyDocument, RandomAccessSource};
use lopdf::xref::{XrefEntry, XrefType};
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
    for (label, version) in [
        ("RC4", Cipher::Rc4),
        ("AES-128", Cipher::Aes128),
        ("AES-256", Cipher::Aes256),
    ] {
        sources.push((
            format!("encrypted with {label}"),
            encrypted(fixture.document.clone(), version),
        ));
    }
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

/// Page text reads the page, its ancestors, fonts, and content, and nothing else: no font
/// program, no image, no other page. Content streams are read from the source a chunk at a time,
/// so no read is large even for a page with megabytes of content.
#[test]
fn page_text_reads_only_what_it_needs_and_never_a_large_block() {
    let mut fixture = Fixture::build();
    let first = fixture.pages[0];
    // Hex digits from xorshift, which deflate to about half their size.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let noise: String = (0..3_000_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b"0123456789abcdef"[(state >> 60) as usize])
        })
        .collect();
    let mut stream = Stream::new(dictionary! {}, format!("BT /F1 12 Tf <{noise}> Tj ET").into_bytes());
    stream.compress().unwrap();
    let large = fixture.document.add_object(stream);
    let contents = fixture
        .document
        .get_dictionary_mut(first)
        .unwrap()
        .get_mut(b"Contents")
        .unwrap();
    contents.as_array_mut().unwrap().push(Object::Reference(large));
    let bytes = save(&mut fixture.document, false, false);
    let file_len = bytes.len() as u64;
    let source = RecordingSource::new(bytes);
    let lazy = LazyDocument::from_source(&source, LoadOptions::default()).unwrap();
    let forbidden: Vec<(u64, u64)> = [fixture.font_program, fixture.image]
        .iter()
        .chain(&fixture.pages[1..])
        .map(|id| object_range(&lazy, *id, file_len))
        .collect();
    let (large_start, large_end) = object_range(&lazy, large, file_len);
    assert!(large_end - large_start > 1_000_000, "{}", large_end - large_start);
    source.reads.borrow_mut().clear();

    let text = lazy
        .extract_page_text_with_limits(first, DecodeLimits::uniform(LIMIT))
        .unwrap();

    assert!(text.contains("Hello") && text.contains("BAAB"), "{text:?}");
    let reads = source.reads.borrow();
    for &(offset, len) in reads.iter() {
        assert!(len <= 64 * 1024, "a read of {len} bytes at {offset}");
        for &(start, end) in &forbidden {
            assert!(
                !(start..end).contains(&offset),
                "read at {offset} inside an object at {start}..{end}"
            );
        }
    }
    assert!(
        reads
            .iter()
            .any(|&(offset, _)| (large_start..large_end).contains(&offset))
    );
}

/// The pages of one call share the total bound, as in the eager extractor.
#[test]
fn the_pages_of_one_call_share_the_total_bound() {
    let fixture = Fixture::build();
    let bytes = save(&mut fixture.document.clone(), true, true);
    let eager = Document::load_mem(&bytes).unwrap();
    let lens: Vec<usize> = fixture
        .pages
        .iter()
        .map(|id| eager.get_page_content(*id).len())
        .collect();
    let limits = DecodeLimits {
        max_total_content_size: lens[0] + lens[1],
        ..DecodeLimits::uniform(LIMIT)
    };
    let lazy = LazyDocument::from_source(bytes.as_slice(), LoadOptions::default()).unwrap();

    let texts: Vec<_> = lazy
        .extract_pages_text_with_limits(fixture.pages.clone(), limits)
        .collect();

    assert!(texts[0].is_ok() && texts[1].is_ok());
    for text in &texts[2..] {
        assert!(matches!(
            text,
            Err(lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded { limit }))
                if *limit == lens[0] + lens[1]
        ));
    }
    assert_eq!(
        format!("{:?}", eager.extract_text_with_limits(&[1, 2, 3], limits)),
        format!(
            "{:?}",
            Err::<String, _>(lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded {
                limit: lens[0] + lens[1]
            }))
        )
    );
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

enum Cipher {
    Rc4,
    Aes128,
    Aes256,
}

/// The document encrypted with an empty user password, which both readers open.
fn encrypted(mut doc: Document, cipher: Cipher) -> Vec<u8> {
    doc.trailer.set(
        "ID",
        Object::Array(vec![
            Object::String(b"text-parity-id-1".to_vec(), StringFormat::Literal),
            Object::String(b"text-parity-id-2".to_vec(), StringFormat::Literal),
        ]),
    );
    let aes = |filter: Arc<dyn CryptFilter>| BTreeMap::from([(b"StdCF".to_vec(), filter)]);
    let version = match cipher {
        Cipher::Rc4 => EncryptionVersion::V2 {
            document: &doc,
            owner_password: "owner",
            user_password: "",
            key_length: 128,
            permissions: Permissions::all(),
        },
        Cipher::Aes128 => EncryptionVersion::V4 {
            document: &doc,
            encrypt_metadata: true,
            crypt_filters: aes(Arc::new(Aes128CryptFilter)),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner",
            user_password: "",
            permissions: Permissions::all(),
        },
        Cipher::Aes256 => EncryptionVersion::V5 {
            encrypt_metadata: true,
            crypt_filters: aes(Arc::new(Aes256CryptFilter)),
            file_encryption_key: &[7; 32],
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner",
            user_password: "",
            permissions: Permissions::all(),
        },
    };
    let state = EncryptionState::try_from(version).unwrap();
    doc.encrypt(&state).unwrap();
    save(&mut doc, false, false)
}

/// Where object `id` lies in a source of `len` bytes: from its offset to the next object's.
fn object_range<S: RandomAccessSource>(lazy: &LazyDocument<S>, id: ObjectId, len: u64) -> (u64, u64) {
    let offsets: Vec<u64> = lazy
        .reference_table()
        .entries
        .values()
        .filter_map(|entry| match entry {
            XrefEntry::Normal { offset, .. } => Some(u64::from(*offset)),
            _ => None,
        })
        .collect();
    let Some(XrefEntry::Normal { offset, .. }) = lazy.reference_table().get(id.0) else {
        panic!("object {id:?} is not stored on its own");
    };
    let start = u64::from(*offset);
    let end = offsets
        .iter()
        .copied()
        .filter(|&next| next > start)
        .min()
        .unwrap_or(len);
    (start, end)
}

/// A source that records every read.
struct RecordingSource {
    bytes: Vec<u8>,
    reads: RefCell<Vec<(u64, usize)>>,
}

impl RecordingSource {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            reads: RefCell::new(Vec::new()),
        }
    }
}

impl RandomAccessSource for RecordingSource {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.reads.borrow_mut().push((offset, buf.len()));
        self.bytes.read_exact_at(offset, buf)
    }
}
