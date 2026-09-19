//! Page text through the eager extractor, one page at a time.
//!
//! [`Document`]'s text extraction reads a page's fonts, their encodings and `/ToUnicode` CMaps,
//! and the resources inherited through the page tree from [`Document::objects`].
//! [`LazyDocument::page_text_document`] copies exactly those objects, with their ids, into a
//! document of one page, so the eager extractor runs unchanged, and a page's objects are dropped
//! once its text is out. The content streams stay in the source, which the extractor reads a chunk
//! at a time as it parses them.

use std::collections::HashSet;

use super::LazyDocument;
use super::source::RandomAccessSource;
use crate::page_content::PageContent;
use crate::resolver::skip_unless_io;
use crate::{DecodeLimits, Dictionary, Document, Object, ObjectId, Result};

/// Font entries that text extraction never reads and that can hold large streams: embedded font
/// programs, Type 3 glyph procedures, and Type 3 resources.
const SKIPPED_FONT_KEYS: &[&[u8]] = &[b"FontFile", b"FontFile2", b"FontFile3", b"CharProcs", b"Resources"];

impl<S: RandomAccessSource> LazyDocument<S> {
    /// The text of page `page_id`, as [`Document::extract_text_with_limits`] extracts it from the
    /// whole document.
    pub fn extract_page_text_with_limits(&self, page_id: ObjectId, limits: DecodeLimits) -> Result<String> {
        self.extract_pages_text_with_limits([page_id], limits)
            .next()
            .expect("one page")
    }

    /// The text of each of `page_ids` in turn, as [`Document::extract_text_with_limits`] extracts
    /// it from the whole document. The pages share [`DecodeLimits::max_total_content_size`].
    ///
    /// Memory does not grow with a page's content: each content stream is read from the source a
    /// chunk at a time, decrypted and inflated on the way, and parsed one operation at a time.
    pub fn extract_pages_text_with_limits<'a>(
        &'a self, page_ids: impl IntoIterator<Item = ObjectId> + 'a, limits: DecodeLimits,
    ) -> impl Iterator<Item = Result<String>> + 'a {
        // Content decoded by the pages read so far, against the total limit.
        let mut spent = 0;
        page_ids.into_iter().map(move |page_id| {
            let document = self.page_text_document(page_id)?;
            let mut content = PageContent::new(self, self.page_content_ids(page_id)?, limits, spent);
            let chunks = document.page_text_chunks(page_id, Some(limits), &mut content);
            spent += content.len();
            chunks?.into_iter().collect()
        })
    }

    /// A document of the single page `page_id`, holding what text extraction reads besides the
    /// content: the page, its ancestors in the page tree, their `/Resources` down to the fonts,
    /// and each font with its encoding and `/ToUnicode` CMap. Objects keep their ids; entries that
    /// extraction never follows, such as images and the content streams, stay as references to
    /// objects that are not copied.
    pub(crate) fn page_text_document(&self, page_id: ObjectId) -> Result<Document> {
        let mut document = Document::with_version(self.version());
        let mut copied = HashSet::new();

        // The page and its ancestors, as far as the eager resource lookup walks them.
        let mut node = self.copy_object(&mut document, &mut copied, page_id)?;
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
        Ok(document)
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

    /// Copies object `id` alone, and returns what refers onward from it: the object, without the
    /// content if it is a stream. `None` when it cannot be read.
    fn copy_object(
        &self, document: &mut Document, copied: &mut HashSet<ObjectId>, id: ObjectId,
    ) -> Result<Option<Object>> {
        if !copied.insert(id) {
            return Ok(document.objects.get(&id).map(without_content));
        }
        let Some(object) = skip_unless_io(self.get_object(id))? else {
            return Ok(None);
        };
        let onward = without_content(&object);
        document.objects.insert(id, object);
        document.max_id = document.max_id.max(id.0);
        Ok(Some(onward))
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
                _ => {}
            }
        }
        Ok(())
    }
}

/// `object`, or a stream's dictionary alone: its content refers to nothing, and can be large.
fn without_content(object: &Object) -> Object {
    match object {
        Object::Stream(stream) => Object::Dictionary(stream.dict.clone()),
        other => other.clone(),
    }
}

fn entries(dictionary: Dictionary, skipped_keys: &[&[u8]]) -> impl Iterator<Item = Object> {
    dictionary
        .into_iter()
        .filter(|(key, _)| !skipped_keys.contains(&key.as_slice()))
        .map(|(_, value)| value)
}
