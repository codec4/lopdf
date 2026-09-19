use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::io;
use std::path::Path;
use std::rc::Rc;

use log::warn;

use super::HEADER_SEARCH_LEN;
use super::source::{FileSource, RandomAccessSource};
use crate::encryption::{self, EncryptionState};
use crate::error::{ParseError, XrefError};
use crate::parser::{self, ParseContext};
use crate::reader::XREF_OFFSET_RECOVERY_WINDOW;
use crate::xref::{Xref, XrefEntry, XrefType};
use crate::{Dictionary, Document, Error, LoadOptions, Object, ObjectId, ObjectStream, Reader, Result};

/// How much of the end of the source is searched for `startxref`.
const TAIL_LEN: usize = 1024;
/// The first window read for a cross-reference section. It grows until the section parses.
const XREF_WINDOW: usize = 64 * 1024;
/// The smallest window read for an object, so that most objects cost one read.
const MIN_OBJECT_WINDOW: usize = 4 * 1024;
/// The largest window read for one cross-reference section or one object.
const MAX_WINDOW: usize = 64 * 1024 * 1024;
/// Room after an offset for an indirect-object header when correcting an xref offset.
const OBJECT_HEADER_LEN: usize = 32;
/// Non-stream objects kept after they are read, oldest out first.
const OBJECT_CACHE_ENTRIES: usize = 4096;
/// Decoded object streams kept after they are read. Members of one stream tend to be read
/// together, and decoding a stream costs a read and a decompression.
const OBJECT_STREAM_CACHE_ENTRIES: usize = 16;
/// Deepest `/Pages` nesting followed, as in [`Document::get_pages`].
const PAGE_TREE_DEPTH_LIMIT: usize = 256;
/// Longest chain of references followed to reach an object, as in [`Document::get_object`].
const DEREF_LIMIT: usize = 128;

/// A PDF read from a [`RandomAccessSource`] one object at a time. See the [module docs](super).
///
/// Objects are returned owned. Recently read objects are cached, within fixed bounds.
pub struct LazyDocument<S> {
    source: S,
    /// Where `%PDF-` starts in the source. Offsets inside the PDF count from here.
    base: u64,
    /// Length of the PDF from `base`.
    len: usize,
    version: String,
    trailer: Dictionary,
    reference_table: Xref,
    /// Sorted, unique offsets of in-use objects, which bound each object's bytes.
    normal_offsets: Vec<usize>,
    xref_start: usize,
    strict: bool,
    max_decompressed_size: Option<usize>,
    encryption_state: Option<EncryptionState>,
    encryption_dictionary: Option<ObjectId>,
    objects: RefCell<FifoCache<ObjectId, Object>>,
    object_streams: RefCell<FifoCache<u32, Rc<ObjectStream>>>,
}

impl LazyDocument<FileSource> {
    /// Opens the PDF at `path` with the default [`LoadOptions`].
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_options(path, LoadOptions::default())
    }

    /// Opens the PDF at `path`. `options.filter` is ignored.
    pub fn open_with_options<P: AsRef<Path>>(path: P, options: LoadOptions) -> Result<Self> {
        Self::from_source(FileSource::open(path)?, options)
    }
}

impl<S: RandomAccessSource> LazyDocument<S> {
    /// Reads the header, cross-reference data, and trailer from `source`, and sets up decryption
    /// the way [`Document::load_with_options`] does: the empty password first, then
    /// `options.password`. `options.filter` is ignored.
    pub fn from_source(source: S, options: LoadOptions) -> Result<Self> {
        let total = source.len();
        let head_len = usize::try_from(total).map_or(HEADER_SEARCH_LEN, |total| total.min(HEADER_SEARCH_LEN));
        let mut head = vec![0; head_len];
        source.read_exact_at(0, &mut head)?;
        let base = head.windows(5).position(|window| window == b"%PDF-").unwrap_or(0);
        let version = parser::header(&head[base..], options.strict).ok_or(ParseError::InvalidFileHeader)?;
        let len = usize::try_from(total - base as u64)
            .map_err(|_| io::Error::new(io::ErrorKind::Unsupported, "the PDF is too large to address"))?;

        let mut document = Self {
            source,
            base: base as u64,
            len,
            version,
            trailer: Dictionary::new(),
            reference_table: Xref::new(0, XrefType::CrossReferenceTable),
            normal_offsets: Vec::new(),
            xref_start: 0,
            strict: options.strict,
            max_decompressed_size: options.max_decompressed_size,
            encryption_state: None,
            encryption_dictionary: None,
            objects: RefCell::new(FifoCache::new(OBJECT_CACHE_ENTRIES)),
            object_streams: RefCell::new(FifoCache::new(OBJECT_STREAM_CACHE_ENTRIES)),
        };
        let (xref, trailer) = document.resolve_xref_and_trailer()?;
        document.trailer = trailer;
        document.set_reference_table(xref);
        document.setup_encryption(options.password.as_deref())?;
        Ok(document)
    }

    /// The version from the `%PDF-` header.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The trailer of the newest revision, without `/Prev`.
    pub fn trailer(&self) -> &Dictionary {
        &self.trailer
    }

    /// The merged cross-reference table of every revision.
    pub fn reference_table(&self) -> &Xref {
        &self.reference_table
    }

    /// Whether the file is encrypted. See [`Self::encryption_state`] for whether it can be read.
    pub fn is_encrypted(&self) -> bool {
        self.trailer.has(b"Encrypt")
    }

    /// The decryption state when the file is encrypted and a password opened it. Without it,
    /// strings and streams of an encrypted file are returned still encrypted.
    pub fn encryption_state(&self) -> Option<&EncryptionState> {
        self.encryption_state.as_ref()
    }

    /// Reads object `id`, following an object that is itself a reference, as
    /// [`Document::get_object`] does.
    pub fn get_object(&self, id: ObjectId) -> Result<Object> {
        let mut object = self.resolve(id, &mut HashSet::new())?;
        let mut derefs = 0;
        while let Ok(next) = object.as_reference() {
            derefs += 1;
            if derefs > DEREF_LIMIT {
                return Err(Error::ReferenceLimit);
            }
            object = self.resolve(next, &mut HashSet::new())?;
        }
        Ok(object)
    }

    /// Reads object `id` as a dictionary.
    pub fn get_dictionary(&self, id: ObjectId) -> Result<Dictionary> {
        match self.get_object(id)? {
            Object::Dictionary(dictionary) => Ok(dictionary),
            other => Err(Error::ObjectType {
                expected: "Dictionary",
                found: other.enum_variant(),
            }),
        }
    }

    /// Follows `object` if it is a reference, as [`Dictionary::get_deref`] does.
    pub fn dereference(&self, object: &Object) -> Result<Object> {
        match object.as_reference() {
            Ok(id) => self.get_object(id),
            Err(_) => Ok(object.clone()),
        }
    }

    /// The document catalog, from the trailer's `/Root`.
    pub fn catalog(&self) -> Result<Dictionary> {
        self.trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .and_then(|id| self.get_dictionary(id))
    }

    /// Page numbers, from 1, mapped to page object ids, in page-tree order.
    ///
    /// Follows the same rules as [`Document::get_pages`]: only children typed `/Page` count, and
    /// only children typed `/Pages` are descended into. A malformed node is skipped the same
    /// way, but a failure to read the source is returned.
    pub fn get_pages(&self) -> Result<BTreeMap<u32, ObjectId>> {
        let mut pages = BTreeMap::new();
        let Some(root) = skip_unless_io(
            self.catalog()
                .and_then(|catalog| catalog.get(b"Pages").and_then(Object::as_reference)),
        )?
        else {
            return Ok(pages);
        };
        // The eager iterator stops after as many kids as the document has objects.
        let mut visits_left = self
            .reference_table
            .entries
            .values()
            .filter(|entry| matches!(entry, XrefEntry::Normal { .. } | XrefEntry::Compressed { .. }))
            .count();
        let mut kids = self.page_tree_kids(root)?.into_iter();
        let mut stack = Vec::new();
        loop {
            while let Some(kid) = kids.next() {
                if visits_left == 0 {
                    return Ok(pages);
                }
                visits_left -= 1;
                let Ok(kid_id) = kid.as_reference() else {
                    continue;
                };
                let Some(node) = skip_unless_io(self.get_dictionary(kid_id))? else {
                    continue;
                };
                match node.get_type() {
                    Ok(b"Page") => {
                        pages.insert(pages.len() as u32 + 1, kid_id);
                    }
                    Ok(b"Pages") if stack.len() < PAGE_TREE_DEPTH_LIMIT => {
                        let siblings = std::mem::replace(&mut kids, self.page_tree_kids(kid_id)?.into_iter());
                        if siblings.len() > 0 {
                            stack.push(siblings);
                        }
                    }
                    _ => {}
                }
            }
            match stack.pop() {
                Some(siblings) => kids = siblings,
                None => return Ok(pages),
            }
        }
    }

    /// The number of pages [`Self::get_pages`] finds.
    pub fn page_count(&self) -> Result<u32> {
        Ok(self.get_pages()?.len() as u32)
    }

    /// The `/Kids` of page tree node `id`, or none when the node or its kids cannot be read.
    fn page_tree_kids(&self, id: ObjectId) -> Result<Vec<Object>> {
        let kids = skip_unless_io(self.get_dictionary(id).and_then(|node| {
            let kids = node.get(b"Kids")?;
            match self.dereference(kids)? {
                Object::Array(kids) => Ok(kids),
                other => Err(Error::ObjectType {
                    expected: "Array",
                    found: other.enum_variant(),
                }),
            }
        }))?;
        Ok(kids.unwrap_or_default())
    }

    /// Reads object `id` without following an object that is itself a reference, as the eager
    /// reader does while loading. `already_seen` guards reference cycles.
    fn resolve(&self, id: ObjectId, already_seen: &mut HashSet<ObjectId>) -> Result<Object> {
        if !already_seen.insert(id) {
            warn!("reference cycle detected resolving object {} {}", id.0, id.1);
            return Err(Error::ReferenceCycle(id));
        }
        if let Some(object) = self.objects.borrow().get(&id) {
            return Ok(object);
        }
        let object = match self.reference_table.get(id.0) {
            Some(XrefEntry::Compressed { container, .. }) => self.compressed_object(id, *container)?,
            Some(XrefEntry::Normal { offset, generation }) if *generation == id.1 => {
                let (_, mut object) = self.read_object(*offset as usize, Some(id), already_seen)?;
                // Like `Document::load`, keep an object whose strings do not all decrypt, as far
                // as decryption got; some files carry signature data that is not valid ciphertext.
                if let Some(state) = &self.encryption_state
                    && Some(id) != self.encryption_dictionary
                    && let Err(error) = encryption::decrypt_object(state, id, &mut object)
                {
                    warn!("object {} {} did not fully decrypt: {error}", id.0, id.1);
                }
                object
            }
            _ => return Err(Error::MissingXrefEntry),
        };
        // Streams can be large and are rarely read twice for structure.
        if !matches!(object, Object::Stream(_)) {
            self.objects.borrow_mut().insert(id, object.clone());
        }
        Ok(object)
    }

    fn compressed_object(&self, id: ObjectId, container: u32) -> Result<Object> {
        let cached = self.object_streams.borrow().get(&container);
        let object_stream = match cached {
            Some(object_stream) => object_stream,
            None => {
                let container_object = self.resolve((container, 0), &mut HashSet::new())?;
                let object_stream = Rc::new(ObjectStream::new_with_limit(
                    container_object.as_stream()?,
                    self.max_decompressed_size,
                )?);
                self.object_streams
                    .borrow_mut()
                    .insert(container, Rc::clone(&object_stream));
                object_stream
            }
        };
        object_stream.objects.get(&id).cloned().ok_or(Error::MissingXrefEntry)
    }

    /// Parses the indirect object at `offset`.
    ///
    /// The bytes up to the next object are enough for a well-formed object. A wrong neighbouring
    /// offset can cut an object short, and the eager reader parses against the whole file, so the
    /// window grows before the object counts as unreadable. Stream-length recovery still stops at
    /// the next object, as in the eager reader.
    fn read_object(
        &self, offset: usize, expected_id: Option<ObjectId>, already_seen: &mut HashSet<ObjectId>,
    ) -> Result<(ObjectId, Object)> {
        if offset >= self.len {
            return Err(Error::InvalidOffset(offset));
        }
        let next_index = self.normal_offsets.partition_point(|&next| next <= offset);
        let end = self.object_end(offset, self.normal_offsets.get(next_index).copied());
        let mut window_end = end.max(offset.saturating_add(MIN_OBJECT_WINDOW)).min(self.len);
        loop {
            let bytes = self.read(offset, window_end)?;
            // A failed attempt may have marked a stream length's object as seen.
            let mut seen = already_seen.clone();
            match parser::indirect_object(&bytes, 0, expected_id, self, &mut seen, Some(end - offset)) {
                Ok((id, mut object)) => {
                    *already_seen = seen;
                    if let Object::Stream(stream) = &mut object
                        && let Some(position) = stream.start_position.as_mut()
                    {
                        *position += offset;
                    }
                    return Ok((id, object));
                }
                Err(_) if window_end < self.len && window_end - offset < MAX_WINDOW => {
                    window_end = offset
                        .saturating_add((window_end - offset).saturating_mul(4))
                        .min(self.len);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn object_end(&self, offset: usize, next_object: Option<usize>) -> usize {
        let xref_start = (self.xref_start > offset).then_some(self.xref_start);
        next_object
            .into_iter()
            .chain(xref_start)
            .min()
            .unwrap_or(self.len)
            .min(self.len)
    }

    /// Resolves the newest cross-reference section and trailer and every `/Prev` revision, as
    /// the eager reader does.
    fn resolve_xref_and_trailer(&mut self) -> Result<(Xref, Dictionary)> {
        let tail = self.read(self.len.saturating_sub(TAIL_LEN), self.len)?;
        let xref_start = Reader::get_xref_start(&tail)?;
        if xref_start > self.len {
            return Err(Error::Xref(XrefError::Start));
        }
        let xref_start = self.correct_xref_offset(xref_start)?;
        self.xref_start = xref_start;

        let (mut xref, mut trailer) = self.xref_and_trailer_at(xref_start)?;

        let mut already_seen = HashSet::new();
        let mut prev_xref_start = trailer.remove(b"Prev");
        while let Some(prev) = prev_xref_start.and_then(|offset| offset.as_i64().ok()) {
            if !already_seen.insert(prev) {
                break;
            }
            if prev < 0 || prev as usize > self.len {
                return Err(Error::Xref(XrefError::PrevStart));
            }
            let (prev_xref, prev_trailer) = self.xref_and_trailer_at(prev as usize)?;
            xref.merge(prev_xref);

            // Read the xref stream of a hybrid-reference file.
            let prev_xref_stream_start = trailer.remove(b"XRefStm");
            if let Some(prev) = prev_xref_stream_start.and_then(|offset| offset.as_i64().ok()) {
                if prev < 0 || prev as usize > self.len {
                    return Err(Error::Xref(XrefError::StreamStart));
                }
                let (prev_xref, _) = self.xref_and_trailer_at(prev as usize)?;
                xref.merge(prev_xref);
            }

            prev_xref_start = prev_trailer.get(b"Prev").cloned().ok();
        }
        let xref_entry_count = xref.max_id().checked_add(1).ok_or(ParseError::InvalidXref)?;
        if xref.size != xref_entry_count {
            warn!(
                "Size entry of trailer dictionary is {}, correct value is {}.",
                xref.size, xref_entry_count
            );
            xref.size = xref_entry_count;
        }
        Ok((xref, trailer))
    }

    /// Parses the cross-reference section at `offset`, reading more of the source while the
    /// window is too short to hold it.
    fn xref_and_trailer_at(&self, offset: usize) -> Result<(Xref, Dictionary)> {
        let offset = self.correct_xref_offset(offset)?;
        let mut window = XREF_WINDOW;
        loop {
            let end = offset.saturating_add(window).min(self.len);
            let bytes = self.read(offset, end)?;
            match parser::xref_and_trailer(&bytes, self) {
                Ok(parsed) => return Ok(parsed),
                Err(_) if end < self.len && window < MAX_WINDOW => window = window.saturating_mul(4),
                Err(error) => return Err(error),
            }
        }
    }

    fn correct_xref_offset(&self, offset: usize) -> Result<usize> {
        if self.strict || offset >= self.len {
            return Ok(offset);
        }
        // Five more bytes before the window let the correction rule out `startxref`.
        let start = offset.saturating_sub(XREF_OFFSET_RECOVERY_WINDOW + 5);
        let end = offset
            .saturating_add(XREF_OFFSET_RECOVERY_WINDOW + OBJECT_HEADER_LEN)
            .min(self.len);
        let window = self.read(start, end)?;
        Ok(Reader::correct_xref_offset_in(&window, start, offset))
    }

    fn set_reference_table(&mut self, xref: Xref) {
        let mut normal_offsets: Vec<usize> = xref
            .entries
            .values()
            .filter_map(|entry| match entry {
                XrefEntry::Normal { offset, .. } => Some(*offset as usize),
                _ => None,
            })
            .collect();
        normal_offsets.sort_unstable();
        normal_offsets.dedup();
        self.normal_offsets = normal_offsets;
        self.reference_table = xref;
    }

    fn setup_encryption(&mut self, password: Option<&str>) -> Result<()> {
        let Ok(encrypt) = self.trailer.get(b"Encrypt") else {
            return Ok(());
        };
        let mut document = Document::new();
        document.trailer = self.trailer.clone();
        if let Ok(id) = encrypt.as_reference() {
            let dictionary = self.resolve(id, &mut HashSet::new())?;
            document.objects.insert(id, dictionary);
            self.encryption_dictionary = Some(id);
        }
        let password = if document.authenticate_password("").is_ok() {
            ""
        } else {
            match password {
                Some(password) if document.authenticate_password(password).is_ok() => password,
                Some(_) => return Err(Error::InvalidPassword),
                None => {
                    warn!("PDF is encrypted and requires a password");
                    return Ok(());
                }
            }
        };
        self.encryption_state = Some(EncryptionState::decode(&document, password)?);
        // Anything read so far was read before it could be decrypted.
        self.objects.borrow_mut().clear();
        self.object_streams.borrow_mut().clear();
        Ok(())
    }

    /// Reads bytes `start..end` of the PDF, counted from its `%PDF-` header.
    fn read(&self, start: usize, end: usize) -> Result<Vec<u8>> {
        let mut bytes = vec![0; end.saturating_sub(start)];
        self.source.read_exact_at(self.base + start as u64, &mut bytes)?;
        Ok(bytes)
    }
}

impl<S: RandomAccessSource> ParseContext for LazyDocument<S> {
    fn strict(&self) -> bool {
        self.strict
    }

    fn max_decompressed_size(&self) -> Option<usize> {
        self.max_decompressed_size
    }

    fn get_object(&self, id: ObjectId, already_seen: &mut HashSet<ObjectId>) -> Result<Object> {
        self.resolve(id, already_seen)
    }
}

/// Keeps a failure to read the source, and turns any other error into `None`, the way the eager
/// reader skips a malformed page tree node.
fn skip_unless_io<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Error::IO(error)) => Err(Error::IO(error)),
        Err(_) => Ok(None),
    }
}

/// A cache that holds at most `capacity` entries and drops the oldest first.
struct FifoCache<K, V> {
    capacity: usize,
    entries: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K: Copy + Eq + Hash, V: Clone> FifoCache<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &K) -> Option<V> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: K, value: V) {
        if self.entries.insert(key, value).is_none() {
            self.order.push_back(key);
        }
        while self.entries.len() > self.capacity {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.entries.remove(&oldest);
                }
                None => break,
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}
