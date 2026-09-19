use std::borrow::Cow;

use crate::{Dictionary, Document, Error, Object, ObjectId, Result};

/// Objects for code that walks a PDF's structure, such as
/// [`DocumentOutline`](crate::DocumentOutline), from a loaded [`Document`] or, with the
/// `lazy-reader` feature, from a [`LazyDocument`](crate::lazy::LazyDocument).
pub trait ObjectResolver {
    /// Object `id`, following an object that is itself a reference.
    fn object(&self, id: ObjectId) -> Result<Cow<'_, Object>>;

    /// The trailer of the newest revision.
    fn trailer(&self) -> &Dictionary;

    /// Page object ids in page-tree order, so that the page with number `n` is at index `n - 1`.
    fn page_ids(&self) -> Result<Vec<ObjectId>>;
}

impl ObjectResolver for Document {
    fn object(&self, id: ObjectId) -> Result<Cow<'_, Object>> {
        self.get_object(id).map(Cow::Borrowed)
    }

    fn trailer(&self) -> &Dictionary {
        &self.trailer
    }

    fn page_ids(&self) -> Result<Vec<ObjectId>> {
        Ok(self.get_pages().into_values().collect())
    }
}

/// `object`, or the object it refers to.
pub(crate) fn dereference<'a, R: ObjectResolver + ?Sized>(
    resolver: &'a R, object: &'a Object,
) -> Result<Cow<'a, Object>> {
    match object.as_reference() {
        Ok(id) => resolver.object(id),
        Err(_) => Ok(Cow::Borrowed(object)),
    }
}

/// Keeps a failure to read the source, and turns any other error into `None`, for structure walks
/// that skip malformed parts of a document but must not hide I/O failures.
pub(crate) fn skip_unless_io<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Error::IO(error)) => Err(Error::IO(error)),
        Err(_) => Ok(None),
    }
}
