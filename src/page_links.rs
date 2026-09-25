//! A page's link annotations: where each link sits on the page and where it leads.
//!
//! A link names its target the way an outline item does, by `/Dest` or by a `/GoTo` action, so it
//! resolves through the same destinations as [`DocumentOutline`](crate::DocumentOutline), named
//! destinations and their name trees included. A link to a web page carries a `/URI` action
//! instead. A malformed annotation is skipped rather than failing its page, and every walk is
//! bounded, so a page with a very long `/Annots` cannot exhaust a reader.

use crate::document_outline::{Destinations, catalog};
use crate::page_geometry::PageRect;
use crate::resolver::{ObjectResolver, dereference, skip_unless_io};
use crate::{Dictionary, Object, ObjectId, OutlineTarget, Result};

/// `/F` flags that keep an annotation off the page: Hidden, and NoView.
const HIDING_FLAGS: i64 = 2 | 32;

/// Bounds on reading one page's links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageLinkLimits {
    /// Most annotations looked at on one page, links or not.
    pub max_annotations: usize,
    /// Most rectangles kept for one link, from its `/QuadPoints`.
    pub max_rects: usize,
}

impl Default for PageLinkLimits {
    fn default() -> Self {
        Self {
            max_annotations: 4_000,
            max_rects: 64,
        }
    }
}

/// The links on one page, in the order its `/Annots` lists them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PageLinks {
    pub links: Vec<PageLink>,
    /// Link annotations left out: hidden ones, ones without an area, and ones leading somewhere a
    /// reader of this document cannot follow, such as another file or a page not in the page tree.
    pub skipped: usize,
    /// Whether reading stopped at [`PageLinkLimits::max_annotations`] before the end of `/Annots`.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageLink {
    /// Where the link sits, in user space: one rectangle for each quadrilateral of its
    /// `/QuadPoints`, as a link that wraps onto a second line has, or else its `/Rect`.
    pub rects: Vec<PageRect>,
    pub target: LinkTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    /// A place in this document.
    Page(OutlineTarget),
    /// A `/URI` action's address, as written.
    Uri(String),
}

/// Reads the links of a document's pages, one page at a time, looking up each named destination
/// once for all of them.
pub struct PageLinkReader<'a, R: ?Sized> {
    resolver: &'a R,
    page_ids: Vec<ObjectId>,
    /// `None` for a document without a readable catalog, whose links can only be web links.
    destinations: Option<Destinations<'a, R>>,
    limits: PageLinkLimits,
}

impl<'a, R: ObjectResolver + ?Sized> PageLinkReader<'a, R> {
    /// A reader with the default [`PageLinkLimits`]. Only a failure to read the source, or the
    /// page tree, is an error.
    pub fn new(resolver: &'a R) -> Result<Self> {
        Self::with_limits(resolver, PageLinkLimits::default())
    }

    pub fn with_limits(resolver: &'a R, limits: PageLinkLimits) -> Result<Self> {
        let page_ids = resolver.page_ids()?;
        let destinations =
            skip_unless_io(catalog(resolver))?.map(|catalog| Destinations::new(resolver, catalog, &page_ids));
        Ok(Self {
            resolver,
            page_ids,
            destinations,
            limits,
        })
    }

    /// Pages in the document.
    pub fn page_count(&self) -> u32 {
        self.page_ids.len() as u32
    }

    /// The object id of page `page`, numbered from 1.
    pub fn page_id(&self, page: u32) -> Option<ObjectId> {
        self.page_ids.get((page as usize).checked_sub(1)?).copied()
    }

    /// The links on page `page`, numbered from 1, and none for a page outside the document. Only
    /// a failure to read the source is an error.
    pub fn read(&mut self, page: u32) -> Result<PageLinks> {
        let mut links = PageLinks::default();
        let Some(page_id) = self.page_id(page) else {
            return Ok(links);
        };
        let Some(entries) = skip_unless_io(
            self.resolver
                .object(page_id)
                .and_then(|page| page.as_dict().and_then(|page| page.get(b"Annots")).cloned())
                .and_then(|annots| dereference(self.resolver, &annots).and_then(|annots| annots.as_array().cloned())),
        )?
        else {
            return Ok(links);
        };
        if entries.len() > self.limits.max_annotations {
            links.truncated = true;
        }
        for entry in entries.iter().take(self.limits.max_annotations) {
            // `/Annots` may hold an annotation in place or refer to it.
            let Some(annotation) =
                skip_unless_io(dereference(self.resolver, entry).and_then(|entry| entry.as_dict().cloned()))?
            else {
                continue;
            };
            if annotation.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Link") {
                continue;
            }
            match self.link(&annotation)? {
                Some(link) => links.links.push(link),
                None => links.skipped += 1,
            }
        }
        Ok(links)
    }

    fn link(&mut self, annotation: &Dictionary) -> Result<Option<PageLink>> {
        let flags = annotation.get(b"F").and_then(Object::as_i64).unwrap_or(0);
        if flags & HIDING_FLAGS != 0 {
            return Ok(None);
        }
        let rects = rects(self.resolver, annotation, self.limits.max_rects)?;
        if rects.is_empty() {
            return Ok(None);
        }
        Ok(self.target(annotation)?.map(|target| PageLink { rects, target }))
    }

    /// A place in this document, from `/Dest` or a `/GoTo` action, or else a web address from a
    /// `/URI` action.
    fn target(&mut self, annotation: &Dictionary) -> Result<Option<LinkTarget>> {
        if let Some(destinations) = self.destinations.as_mut()
            && let Some(target) = destinations.item_target(annotation)?
        {
            return Ok(Some(LinkTarget::Page(target)));
        }
        Ok(uri(self.resolver, annotation)?.map(LinkTarget::Uri))
    }
}

/// A link's areas: each quadrilateral of `/QuadPoints` as the rectangle around it, when it has any,
/// else its `/Rect`. Areas without extent are dropped.
fn rects<R: ObjectResolver + ?Sized>(resolver: &R, annotation: &Dictionary, max_rects: usize) -> Result<Vec<PageRect>> {
    let quads = skip_unless_io(
        annotation
            .get(b"QuadPoints")
            .and_then(|quads| dereference(resolver, quads))
            .and_then(|quads| quads.as_array().cloned()),
    )?;
    // Every entry must be a number: one that is not would shift the rest out of their quads.
    let numbers: Option<Vec<f32>> = quads.and_then(|quads| quads.iter().map(|value| value.as_float().ok()).collect());
    if let Some(numbers) = numbers.filter(|numbers| !numbers.is_empty() && numbers.len() % 8 == 0) {
        let rects: Vec<PageRect> = numbers
            .chunks(8)
            .take(max_rects)
            .map(|quad| {
                let xs = [quad[0], quad[2], quad[4], quad[6]];
                let ys = [quad[1], quad[3], quad[5], quad[7]];
                PageRect {
                    left: xs.iter().copied().fold(f32::INFINITY, f32::min),
                    bottom: ys.iter().copied().fold(f32::INFINITY, f32::min),
                    right: xs.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                    top: ys.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                }
            })
            .filter(|rect| !rect.is_empty())
            .collect();
        if !rects.is_empty() {
            return Ok(rects);
        }
    }
    let rect = match annotation.get(b"Rect") {
        Ok(rect) => PageRect::read(resolver, rect)?,
        Err(_) => None,
    };
    Ok(rect.filter(|rect| !rect.is_empty()).into_iter().collect())
}

/// The address of a link's `/URI` action, when it has one.
fn uri<R: ObjectResolver + ?Sized>(resolver: &R, annotation: &Dictionary) -> Result<Option<String>> {
    let Some(action) = skip_unless_io(
        annotation
            .get(b"A")
            .and_then(|action| dereference(resolver, action))
            .and_then(|action| action.as_dict().cloned()),
    )?
    else {
        return Ok(None);
    };
    if action.get(b"S").and_then(Object::as_name).ok() != Some(b"URI") {
        return Ok(None);
    }
    let Some(address) = skip_unless_io(action.get(b"URI").and_then(|address| dereference(resolver, address)))? else {
        return Ok(None);
    };
    let Ok(bytes) = address.as_str() else {
        return Ok(None);
    };
    // The standard makes a URI 7-bit ASCII; anything else still reads better than nothing.
    let address = String::from_utf8_lossy(bytes).trim().to_owned();
    Ok((!address.is_empty()).then_some(address))
}
