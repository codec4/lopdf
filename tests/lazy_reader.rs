//! `LazyDocument` reads a PDF's structure from a random-access source, one object at a time, and
//! agrees with what `Document` loads eagerly.

#![cfg(all(feature = "lazy-reader", not(feature = "async")))]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io;

use lopdf::lazy::{LazyDocument, RandomAccessSource};
use lopdf::xref::XrefType;
use lopdf::{
    Document, EncryptionState, EncryptionVersion, Error, IncrementalDocument, LoadOptions, Object, Permissions,
    SaveOptions, Stream, StringFormat, dictionary,
};

#[test]
fn every_asset_matches_the_eager_reader() {
    for asset in [
        "example.pdf",
        "Incremental.pdf",
        "AnnotationDemo.pdf",
        "test.pdf",
        "unicode.pdf",
        "encrypted.pdf",
    ] {
        let bytes = std::fs::read(format!("assets/{asset}")).unwrap();
        assert_matches_eager(&bytes, asset);
    }
}

#[test]
fn metadata_matches_the_eager_metadata_loader() {
    let mut sources: Vec<(String, Vec<u8>)> = [
        "example.pdf",
        "Incremental.pdf",
        "AnnotationDemo.pdf",
        "test.pdf",
        "unicode.pdf",
        "encrypted.pdf",
    ]
    .into_iter()
    .map(|asset| (asset.to_owned(), std::fs::read(format!("assets/{asset}")).unwrap()))
    .collect();
    let mut doc = sample_document(4);
    let info = doc.add_object(dictionary! {
        "Title" => Object::String(b"\xFE\xFF\x00C\x00a\x00f\x00\xE9".to_vec(), StringFormat::Hexadecimal),
        "Author" => Object::string_literal("Ann Author"),
        "Custom" => 7,
    });
    doc.trailer.set("Info", info);
    for (object_streams, xref_streams) in [(false, false), (true, true)] {
        let label = format!("Info in object streams {object_streams}");
        sources.push((label, save(&mut doc, object_streams, xref_streams)));
    }

    for (label, bytes) in sources {
        let eager = Document::load_metadata_mem(&bytes).unwrap();
        let lazy = LazyDocument::from_source(bytes.as_slice(), LoadOptions::default())
            .unwrap()
            .metadata()
            .unwrap();
        let fields = |metadata: &lopdf::PdfMetadata| {
            let mut custom: Vec<String> = metadata.custom.iter().map(|entry| format!("{entry:?}")).collect();
            custom.sort();
            (
                [
                    metadata.title.clone(),
                    metadata.author.clone(),
                    metadata.subject.clone(),
                    metadata.keywords.clone(),
                    metadata.creator.clone(),
                    metadata.producer.clone(),
                    metadata.creation_date.clone(),
                    metadata.modification_date.clone(),
                ],
                custom,
                metadata.version.clone(),
                metadata.encrypted,
            )
        };
        assert_eq!(fields(&lazy), fields(&eager), "{label}");
        // The eager metadata loader cannot read a page tree kept in an object stream, as in
        // encrypted.pdf, and counts no pages; the full eager load counts them.
        let pages = Document::load_mem(&bytes).unwrap().get_pages().len() as u32;
        assert_eq!(lazy.page_count, pages, "{label}");
        if label.starts_with("Info") {
            assert_eq!(lazy.title.as_deref(), Some("Caf\u{e9}"));
            assert_eq!(lazy.author.as_deref(), Some("Ann Author"));
            assert_eq!(lazy.page_count, 4);
        }
    }
}

#[test]
fn classic_xref_stream_and_object_stream_files_match_the_eager_reader() {
    for (label, object_streams, xref_streams) in [
        ("classic table", false, false),
        ("xref stream", false, true),
        ("object streams", true, true),
    ] {
        let bytes = save(&mut sample_document(7), object_streams, xref_streams);
        let lazy = assert_matches_eager(&bytes, label);
        assert_eq!(lazy.page_count().unwrap(), 7, "{label}");
    }
}

#[test]
fn an_incremental_update_wins_over_the_earlier_revision() {
    let mut original = sample_document(4);
    let first_page = original.get_pages()[&1];
    let previous = save(&mut original, false, false);

    let loaded = Document::load_mem(&previous).unwrap();
    let mut page = loaded.get_dictionary(first_page).unwrap().clone();
    page.set("Rotate", 90);
    let mut incremental = IncrementalDocument::create_from(previous.clone(), loaded);
    incremental
        .new_document
        .objects
        .insert(first_page, Object::Dictionary(page));
    let mut updated = Vec::new();
    incremental.save_to(&mut updated).unwrap();

    let lazy = assert_matches_eager(&updated, "incremental update");
    let rotate = lazy
        .get_dictionary(first_page)
        .unwrap()
        .get(b"Rotate")
        .unwrap()
        .as_i64()
        .unwrap();
    assert_eq!(rotate, 90);
}

#[test]
fn a_file_encrypted_with_an_empty_password_opens_decrypted() {
    let bytes = encrypted_document("");

    let lazy = assert_matches_eager(&bytes, "empty password");

    assert!(lazy.is_encrypted());
    assert!(lazy.encryption_state().is_some());
}

#[test]
fn a_user_password_is_needed_to_decrypt_and_a_wrong_one_is_refused() {
    let bytes = encrypted_document("secret");

    let without = LazyDocument::from_source(bytes.as_slice(), LoadOptions::default()).unwrap();
    assert!(without.is_encrypted());
    assert!(without.encryption_state().is_none());

    let with = LazyDocument::from_source(bytes.as_slice(), with_password("secret")).unwrap();
    let eager = Document::load_mem_with_options(&bytes, with_password("secret")).unwrap();
    assert!(with.encryption_state().is_some());
    assert_eq!(with.get_pages().unwrap(), eager.get_pages());
    for id in eager.get_pages().values() {
        assert_eq!(&with.get_dictionary(*id).unwrap(), eager.get_dictionary(*id).unwrap());
    }

    let wrong = LazyDocument::from_source(bytes.as_slice(), with_password("wrong"));
    assert!(matches!(wrong, Err(Error::InvalidPassword)));
}

#[test]
fn walking_the_pages_reads_a_small_part_of_a_large_file() {
    let mut doc = sample_document(6);
    // A large image the page tree never needs to read.
    let image = Stream::new(
        dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 1024, "Height" => 1024 },
        vec![7; 4 * 1024 * 1024],
    )
    .with_compression(false);
    let image_id = doc.add_object(image);
    let first_page = doc.get_pages()[&1];
    doc.get_dictionary_mut(first_page).unwrap().set(
        "Resources",
        dictionary! { "XObject" => dictionary! { "Im1" => Object::Reference(image_id) } },
    );
    let bytes = save(&mut doc, false, false);
    let source = CountingSource::new(bytes.clone());

    let lazy = LazyDocument::from_source(&source, LoadOptions::default()).unwrap();
    let pages = lazy.get_pages().unwrap();

    assert_eq!(pages, Document::load_mem(&bytes).unwrap().get_pages());
    let read = source.bytes_read.get();
    assert!(read < 128 * 1024, "read {read} of {} bytes", bytes.len());
}

#[test]
fn a_file_on_disk_opens_through_positional_reads() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sample.pdf");
    let bytes = save(&mut sample_document(3), true, true);
    std::fs::write(&path, &bytes).unwrap();

    let lazy = LazyDocument::open(&path).unwrap();

    assert_eq!(
        lazy.get_pages().unwrap(),
        Document::load_mem(&bytes).unwrap().get_pages()
    );
}

#[test]
fn page_tree_cycles_and_deep_trees_stop_where_the_eager_reader_stops() {
    let mut cyclic = sample_document(2);
    let root = cyclic.catalog().unwrap().get(b"Pages").unwrap().as_reference().unwrap();
    let kids = cyclic.get_dictionary_mut(root).unwrap().get_mut(b"Kids").unwrap();
    kids.as_array_mut().unwrap().push(Object::Reference(root));
    assert_matches_eager(&save(&mut cyclic, false, false), "page tree cycle");

    // Each level holds a deeper node and a page. The eager iterator stacks the page while it
    // descends, so the stack deepens by one per level and stops descending at its limit.
    let mut deep = Document::with_version("1.4");
    let bottom_page = deep.new_object_id();
    let mut child = bottom_page;
    let mut levels = Vec::new();
    for _ in 0..300 {
        let node_id = deep.new_object_id();
        let page_id = deep.add_object(dictionary! { "Type" => "Page", "Parent" => Object::Reference(node_id) });
        levels.push((node_id, child, page_id));
        child = node_id;
    }
    for (node_id, deeper, page_id) in &levels {
        deep.objects.insert(
            *node_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(*deeper), Object::Reference(*page_id)],
            }),
        );
    }
    deep.objects.insert(
        bottom_page,
        Object::Dictionary(dictionary! { "Type" => "Page", "Parent" => Object::Reference(levels[0].0) }),
    );
    let catalog_id = deep.add_object(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference(child) });
    deep.trailer.set("Root", Object::Reference(catalog_id));
    let deep_bytes = save(&mut deep, false, false);
    let lazy = assert_matches_eager(&deep_bytes, "deep page tree");
    let pages = lazy.get_pages().unwrap();
    assert!(
        pages.len() < 300,
        "the depth limit stops the walk early: {} pages",
        pages.len()
    );
    assert!(!pages.values().any(|id| *id == bottom_page));
}

#[test]
fn a_wrong_neighbouring_offset_does_not_cut_an_object_short() {
    let long_title = "x".repeat(8 * 1024);
    let body = format!("<< /Type /Catalog /Pages 2 0 R /Title ({long_title}) >>");
    let objects = [
        (1, body.as_str()),
        (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        (3, "<< /Type /Page /Parent 2 0 R >>"),
    ];
    // Object 2's entry points into the middle of object 1, so the bytes up to "the next object"
    // end inside object 1.
    let bytes = raw_pdf(
        &objects,
        "/Root 1 0 R",
        |id, offset| if id == 2 { offset - 4096 } else { offset },
    );

    let lazy = LazyDocument::from_source(bytes.as_slice(), LoadOptions::default()).unwrap();
    let catalog = lazy.catalog().unwrap();

    let eager = Document::load_mem(&bytes).unwrap();
    assert_eq!(&catalog, eager.catalog().unwrap());
    assert_eq!(catalog.get(b"Title").unwrap().as_str().unwrap().len(), long_title.len());
}

#[test]
fn malformed_input_returns_errors_without_panicking() {
    // A stream whose /Length is itself.
    let self_length = raw_pdf(
        &[
            (1, "<< /Type /Catalog /Pages 2 0 R >>"),
            (2, "<< /Type /Pages /Kids [] /Count 0 >>"),
            (3, "<< /Length 3 0 R >>\nstream\nabc\nendstream"),
        ],
        "/Root 1 0 R",
        |_, offset| offset,
    );
    let lazy = LazyDocument::from_source(self_length.as_slice(), LoadOptions::default()).unwrap();
    let _ = lazy.get_object((3, 0));
    assert!(matches!(lazy.get_object((9, 0)), Err(Error::MissingXrefEntry)));

    // startxref far from any xref section.
    let mut bad_start = save(&mut sample_document(1), false, false);
    let marker = find(&bad_start, b"startxref").unwrap();
    bad_start.truncate(marker);
    bad_start.extend_from_slice(b"startxref\n9\n%%EOF\n");
    assert!(LazyDocument::from_source(bad_start.as_slice(), LoadOptions::default()).is_err());

    // Cut in half.
    let whole = save(&mut sample_document(3), false, false);
    let half = &whole[..whole.len() / 2];
    assert!(LazyDocument::from_source(half, LoadOptions::default()).is_err());

    // Not a PDF.
    let text: &[u8] = b"just some text, no header here";
    assert!(matches!(
        LazyDocument::from_source(text, LoadOptions::default()),
        Err(Error::Parse(_))
    ));
}

/// Opens `bytes` lazily and checks the version, the page map, the catalog, and every page
/// dictionary against the eager reader.
fn assert_matches_eager<'a>(bytes: &'a [u8], label: &str) -> LazyDocument<&'a [u8]> {
    let eager = Document::load_mem(bytes).unwrap_or_else(|error| panic!("{label}: eager load failed: {error}"));
    let lazy = LazyDocument::from_source(bytes, LoadOptions::default())
        .unwrap_or_else(|error| panic!("{label}: lazy open failed: {error}"));

    assert_eq!(lazy.version(), eager.version, "{label}: version");
    let pages = lazy.get_pages().unwrap();
    assert_eq!(pages, eager.get_pages(), "{label}: pages");
    assert_eq!(&lazy.catalog().unwrap(), eager.catalog().unwrap(), "{label}: catalog");
    for id in pages.values() {
        assert_eq!(
            &lazy.get_dictionary(*id).unwrap(),
            eager.get_dictionary(*id).unwrap(),
            "{label}: page {id:?}"
        );
    }
    lazy
}

/// A document of `page_count` pages under two levels of `/Pages` nodes, so the page order depends
/// on following each node's `/Kids` in order.
fn sample_document(page_count: usize) -> Document {
    let mut doc = Document::with_version("1.5");
    let root_id = doc.new_object_id();
    let mut groups = Vec::new();
    let page_numbers: Vec<usize> = (1..=page_count).collect();
    for chunk in page_numbers.chunks(3) {
        let group_id = doc.new_object_id();
        let kids = chunk
            .iter()
            .map(|number| {
                let content = Stream::new(dictionary! {}, format!("BT (page {number}) Tj ET").into_bytes());
                let content_id = doc.add_object(content);
                Object::Reference(doc.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => Object::Reference(group_id),
                    "Contents" => Object::Reference(content_id),
                    "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                }))
            })
            .collect::<Vec<_>>();
        doc.objects.insert(
            group_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Parent" => Object::Reference(root_id),
                "Count" => kids.len() as i64,
                "Kids" => kids,
            }),
        );
        groups.push(Object::Reference(group_id));
    }
    doc.objects.insert(
        root_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => page_count as i64,
            "Kids" => groups,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference(root_id) });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    doc
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

/// A one-page document encrypted with RC4 128 and the given user password.
fn encrypted_document(user_password: &str) -> Vec<u8> {
    let mut doc = sample_document(1);
    doc.trailer.set(
        "ID",
        Object::Array(vec![
            Object::String(b"lazy-reader-id-1".to_vec(), StringFormat::Literal),
            Object::String(b"lazy-reader-id-2".to_vec(), StringFormat::Literal),
        ]),
    );
    let first_page = doc.get_pages()[&1];
    doc.get_dictionary_mut(first_page)
        .unwrap()
        .set("Title", Object::string_literal("an encrypted string"));
    let state = EncryptionState::try_from(EncryptionVersion::V2 {
        document: &doc,
        owner_password: "owner",
        user_password,
        key_length: 128,
        permissions: Permissions::all(),
    })
    .unwrap();
    doc.encrypt(&state).unwrap();
    save(&mut doc, false, false)
}

fn with_password(password: &str) -> LoadOptions {
    LoadOptions {
        password: Some(password.to_owned()),
        ..LoadOptions::default()
    }
}

/// A classic PDF from numbered object bodies. `offset_of` may move an object's xref offset.
fn raw_pdf(objects: &[(u32, &str)], trailer: &str, offset_of: impl Fn(u32, usize) -> usize) -> Vec<u8> {
    let mut bytes = b"%PDF-1.4\n".to_vec();
    let mut offsets = BTreeMap::new();
    for (id, body) in objects {
        offsets.insert(*id, bytes.len());
        bytes.extend_from_slice(format!("{id} 0 obj\n{body}\nendobj\n").as_bytes());
    }
    let xref_start = bytes.len();
    let size = objects.iter().map(|(id, _)| id + 1).max().unwrap_or(1);
    bytes.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for id in 1..size {
        let entry = match offsets.get(&id) {
            Some(&offset) => format!("{:010} 00000 n \n", offset_of(id, offset)),
            None => "0000000000 65535 f \n".to_owned(),
        };
        bytes.extend_from_slice(entry.as_bytes());
    }
    bytes.extend_from_slice(
        format!("trailer\n<< /Size {size} {trailer} >>\nstartxref\n{xref_start}\n%%EOF\n").as_bytes(),
    );
    bytes
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|window| window == needle)
}

/// Serves bytes from memory and counts how many were read.
struct CountingSource {
    bytes: Vec<u8>,
    bytes_read: Cell<u64>,
}

impl CountingSource {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            bytes_read: Cell::new(0),
        }
    }
}

impl RandomAccessSource for CountingSource {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.bytes_read.set(self.bytes_read.get() + buf.len() as u64);
        self.bytes.read_exact_at(offset, buf)
    }
}
