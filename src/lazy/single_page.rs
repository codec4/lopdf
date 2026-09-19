//! Page text through the eager extractor, one page at a time.
//!
//! [`Document::extract_text_with_limit`] reads a page's fonts, their encodings and `/ToUnicode`
//! CMaps, the resources inherited through the page tree, and the content streams, all from
//! [`Document::objects`]. [`LazyDocument::single_page_document`] copies exactly those objects,
//! with their ids, into a document of one page, so the eager extractor runs unchanged and a
//! page's objects are dropped once its text is out.

use std::collections::HashSet;

use super::LazyDocument;
use super::source::RandomAccessSource;
use crate::resolver::skip_unless_io;
use crate::{Dictionary, Document, Object, ObjectId, Result, dictionary};

/// Font entries that text extraction never reads and that can hold large streams: embedded font
/// programs, Type 3 glyph procedures, and Type 3 resources.
const SKIPPED_FONT_KEYS: &[&[u8]] = &[b"FontFile", b"FontFile2", b"FontFile3", b"CharProcs", b"Resources"];

impl<S: RandomAccessSource> LazyDocument<S> {
    /// A document of the single page `page_id`, holding what text extraction reads: the page, its
    /// ancestors in the page tree, their `/Resources` down to the fonts, each font with its
    /// encoding and `/ToUnicode` CMap, and the content streams. Objects keep their ids; entries
    /// that extraction never follows, such as images, stay as references to objects that are
    /// not copied.
    pub fn single_page_document(&self, page_id: ObjectId) -> Result<Document> {
        let mut document = Document::with_version(self.version());
        // New objects take numbers past every object of the file, so that they cannot stand in
        // for an object that was not copied.
        let highest = self.reference_table().entries.keys().next_back().copied().unwrap_or(0);
        document.max_id = highest.max(self.reference_table().size.saturating_sub(1));
        let mut copied = HashSet::new();

        let page = self.copy_object(&mut document, &mut copied, page_id)?;
        if let Some(Object::Dictionary(page)) = &page
            && let Ok(contents) = page.get(b"Contents")
        {
            self.copy_closure(&mut document, &mut copied, contents.clone(), &[])?;
        }

        // The page and its ancestors, as far as the eager resource lookup walks them.
        let mut node = page;
        let mut ancestors = 0;
        while let Some(Object::Dictionary(dictionary)) = node.take() {
            self.copy_fonts(&mut document, &mut copied, &dictionary)?;
            let Ok(parent) = dictionary.get(b"Parent").and_then(Object::as_reference) else {
                break;
            };
            if copied.contains(&parent) || ancestors > crate::reader::MAX_NESTING_DEPTH {
                break;
            }
            ancestors += 1;
            node = self.copy_object(&mut document, &mut copied, parent)?;
        }

        // A root of its own, so that the page tree holds only this page.
        let root_id = document.new_object_id();
        document.objects.insert(
            root_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => root_id });
        document.trailer.set("Root", catalog_id);
        Ok(document)
    }

    /// The text of page `page_id`, as [`Document::extract_text_with_limit`] extracts it from the
    /// whole document.
    pub fn extract_page_text_with_limit(&self, page_id: ObjectId, max_decompressed_size: usize) -> Result<String> {
        self.single_page_document(page_id)?
            .extract_text_with_limit(&[1], max_decompressed_size)
    }

    /// Copies the fonts of the `/Resources` of a page-tree node: the resources object when it is
    /// a reference, and the closure of its `/Font` entry.
    fn copy_fonts(&self, document: &mut Document, copied: &mut HashSet<ObjectId>, node: &Dictionary) -> Result<()> {
        let resources = match node.get(b"Resources") {
            Ok(Object::Reference(id)) => self.copy_object(document, copied, *id)?,
            Ok(Object::Dictionary(resources)) => Some(Object::Dictionary(resources.clone())),
            _ => None,
        };
        if let Some(Object::Dictionary(resources)) = resources
            && let Ok(fonts) = resources.get(b"Font")
        {
            self.copy_closure(document, copied, fonts.clone(), SKIPPED_FONT_KEYS)?;
        }
        Ok(())
    }

    /// Copies object `id` alone, and returns it, or `None` when it cannot be read.
    fn copy_object(
        &self, document: &mut Document, copied: &mut HashSet<ObjectId>, id: ObjectId,
    ) -> Result<Option<Object>> {
        if !copied.insert(id) {
            return Ok(document.objects.get(&id).cloned());
        }
        let object = skip_unless_io(self.get_object(id))?;
        if let Some(object) = &object {
            document.objects.insert(id, object.clone());
            document.max_id = document.max_id.max(id.0);
        }
        Ok(object)
    }

    /// Copies every object that `object` refers to, directly or through other objects, except
    /// through dictionary entries named in `skipped_keys`.
    fn copy_closure(
        &self, document: &mut Document, copied: &mut HashSet<ObjectId>, object: Object, skipped_keys: &[&[u8]],
    ) -> Result<()> {
        let mut pending = vec![object];
        while let Some(object) = pending.pop() {
            match object {
                Object::Reference(id) => {
                    if !copied.contains(&id)
                        && let Some(object) = self.copy_object(document, copied, id)?
                    {
                        pending.push(object);
                    }
                }
                Object::Array(items) => pending.extend(items),
                Object::Dictionary(dictionary) => pending.extend(entries(dictionary, skipped_keys)),
                Object::Stream(stream) => pending.extend(entries(stream.dict, skipped_keys)),
                _ => {}
            }
        }
        Ok(())
    }
}

fn entries(dictionary: Dictionary, skipped_keys: &[&[u8]]) -> impl Iterator<Item = Object> {
    dictionary
        .into_iter()
        .filter(|(key, _)| !skipped_keys.contains(&key.as_slice()))
        .map(|(_, value)| value)
}
