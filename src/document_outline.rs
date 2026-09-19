//! A document outline (bookmarks) as a flat list, with each item's page and view.
//!
//! [`DocumentOutline`] keeps each destination's view, such as the `/XYZ` point a heading sits at,
//! skips what it cannot read, and bounds every walk, so a cyclic or very deep outline cannot hang
//! or exhaust a reader. [`Document::get_toc`](crate::Document::get_toc) reads through it and keeps
//! only the page numbers.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::resolver::{ObjectResolver, dereference, skip_unless_io};
use crate::{Dictionary, Object, ObjectId, Result, decode_text_string};

/// Bounds on an outline walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutlineLimits {
    /// Deepest nesting read; top-level items have depth 0.
    pub max_depth: usize,
    /// Most items read.
    pub max_items: usize,
}

impl Default for OutlineLimits {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_items: 10_000,
        }
    }
}

/// The items of a document outline in reading order: each item, then its children, then its
/// next sibling.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DocumentOutline {
    pub items: Vec<OutlineItem>,
    /// Items left out: an unreadable node, a node without a readable title, or a node reached
    /// twice through a cycle.
    pub skipped: usize,
    /// Whether the walk stopped at [`OutlineLimits`] before reading every item.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutlineItem {
    pub title: String,
    /// Nesting depth; top-level items have depth 0.
    pub depth: usize,
    /// Where the item leads in this document, or `None` for an item without a destination, one
    /// that leads to another file or a URI, or one whose page is not in the page tree.
    pub target: Option<OutlineTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlineTarget {
    /// Page number, from 1, in page-tree order.
    pub page: u32,
    pub view: DestinationView,
}

/// How a destination shows its page, in PDF user space (points, origin at the bottom left).
/// `None` coordinates mean "keep the current value".
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DestinationView {
    Xyz {
        left: Option<f32>,
        top: Option<f32>,
        zoom: Option<f32>,
    },
    Fit,
    FitH {
        top: Option<f32>,
    },
    FitV {
        left: Option<f32>,
    },
    FitR {
        left: f32,
        bottom: f32,
        right: f32,
        top: f32,
    },
    FitB,
    FitBH {
        top: Option<f32>,
    },
    FitBV {
        left: Option<f32>,
    },
    /// A missing or unrecognised view.
    Unknown,
}

/// Deepest name tree walked when looking up a named destination.
const MAX_NAME_TREE_DEPTH: usize = 32;

impl DocumentOutline {
    /// Reads the outline with the default [`OutlineLimits`]. A document without an outline gives
    /// an empty one. Only a failure to read the source is an error.
    pub fn read<R: ObjectResolver + ?Sized>(resolver: &R) -> Result<Self> {
        Self::read_with_limits(resolver, OutlineLimits::default())
    }

    /// Reads the outline, stopping at `limits`.
    pub fn read_with_limits<R: ObjectResolver + ?Sized>(resolver: &R, limits: OutlineLimits) -> Result<Self> {
        let mut outline = Self::default();
        let Some(catalog) = skip_unless_io(catalog(resolver))? else {
            return Ok(outline);
        };
        let first = skip_unless_io(
            catalog
                .get(b"Outlines")
                .and_then(|outlines| dereference(resolver, outlines))
                .and_then(|outlines| outlines.as_dict().and_then(|outlines| outlines.get(b"First")).cloned()),
        )?;
        let Some(first) = first.and_then(|first| first.as_reference().ok()) else {
            return Ok(outline);
        };
        let pages = resolver
            .page_ids()?
            .into_iter()
            .enumerate()
            .map(|(index, id)| (id, index as u32 + 1))
            .collect();
        let mut destinations = Destinations {
            resolver,
            catalog: &catalog,
            pages,
            named: HashMap::new(),
        };

        // Preorder walk: an item, its children, then its next sibling. `resume` holds the next
        // siblings of the items whose children are being read.
        let mut visited = HashSet::new();
        let mut resume: Vec<(ObjectId, usize)> = Vec::new();
        let mut cursor = Some((first, 0));
        while let Some((id, depth)) = cursor.take() {
            if outline.items.len() >= limits.max_items {
                outline.truncated = true;
                break;
            }
            if !visited.insert(id) {
                outline.skipped += 1;
                cursor = resume.pop();
                continue;
            }
            let Some(node) = skip_unless_io(resolver.object(id).and_then(|node| node.as_dict().cloned()))? else {
                outline.skipped += 1;
                cursor = resume.pop();
                continue;
            };
            match title(resolver, &node)? {
                Some(title) => {
                    let target = destinations.item_target(&node)?;
                    outline.items.push(OutlineItem { title, depth, target });
                }
                None => outline.skipped += 1,
            }
            let next = node.get(b"Next").and_then(Object::as_reference).ok();
            let child = node.get(b"First").and_then(Object::as_reference).ok();
            cursor = match child {
                Some(child) if depth + 1 < limits.max_depth => {
                    if let Some(next) = next {
                        resume.push((next, depth));
                    }
                    Some((child, depth + 1))
                }
                _ => {
                    if child.is_some() {
                        outline.truncated = true;
                    }
                    next.map(|next| (next, depth)).or_else(|| resume.pop())
                }
            };
        }
        Ok(outline)
    }
}

fn catalog<R: ObjectResolver + ?Sized>(resolver: &R) -> Result<Dictionary> {
    let root = resolver.trailer().get(b"Root").and_then(Object::as_reference)?;
    resolver.object(root)?.as_dict().cloned()
}

/// The item's title with its whitespace collapsed to single spaces, or `None` when it has no
/// readable one. Titles often carry the line break of a two-line heading.
fn title<R: ObjectResolver + ?Sized>(resolver: &R, node: &Dictionary) -> Result<Option<String>> {
    let Some(title) = skip_unless_io(node.get(b"Title").and_then(|title| dereference(resolver, title)))? else {
        return Ok(None);
    };
    let Ok(bytes) = title.as_str() else {
        return Ok(None);
    };
    let text = decode_text_string(&title);
    // An undecodable string still reads better than nothing.
    let text = text.unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned());
    Ok(Some(text.split_whitespace().collect::<Vec<_>>().join(" ")))
}

struct Destinations<'a, R: ?Sized> {
    resolver: &'a R,
    catalog: &'a Dictionary,
    pages: HashMap<ObjectId, u32>,
    /// Named destinations already looked up, found or not.
    named: HashMap<Vec<u8>, Option<Object>>,
}

impl<R: ObjectResolver + ?Sized> Destinations<'_, R> {
    /// The target of an outline item, from its `/Dest` or its `/GoTo` action.
    fn item_target(&mut self, node: &Dictionary) -> Result<Option<OutlineTarget>> {
        if let Ok(destination) = node.get(b"Dest") {
            return self.destination(destination);
        }
        let Some(action) = skip_unless_io(
            node.get(b"A")
                .and_then(|action| dereference(self.resolver, action))
                .and_then(|action| action.as_dict().cloned()),
        )?
        else {
            return Ok(None);
        };
        // Other actions, such as GoToR and URI, lead outside this document.
        if action.get(b"S").and_then(Object::as_name).ok() != Some(b"GoTo") {
            return Ok(None);
        }
        match action.get(b"D") {
            Ok(destination) => self.destination(destination),
            Err(_) => Ok(None),
        }
    }

    /// Resolves an explicit destination array, or a destination named by a name or a string.
    fn destination(&mut self, destination: &Object) -> Result<Option<OutlineTarget>> {
        let Some(destination) = skip_unless_io(dereference(self.resolver, destination))? else {
            return Ok(None);
        };
        let explicit = match destination.as_ref() {
            Object::Array(array) => Some(Cow::Borrowed(array.as_slice())),
            Object::Name(name) | Object::String(name, _) => self.named(name)?.map(Cow::Owned),
            _ => None,
        };
        Ok(explicit.and_then(|array| self.explicit_destination(&array)))
    }

    /// The destination array behind `name`, from the catalog's `/Dests` dictionary (PDF 1.1) or
    /// its `/Names` `/Dests` name tree. Some producers mix the two, so both are tried.
    fn named(&mut self, name: &[u8]) -> Result<Option<Vec<Object>>> {
        if let Some(found) = self.named.get(name) {
            return Ok(found.as_ref().and_then(|value| self.destination_array(value)));
        }
        let mut found = skip_unless_io(
            self.catalog
                .get(b"Dests")
                .and_then(|dests| dereference(self.resolver, dests))
                .and_then(|dests| dests.as_dict().and_then(|dests| dests.get(name)).cloned()),
        )?;
        if found.is_none()
            && let Some(tree) = skip_unless_io(
                self.catalog
                    .get(b"Names")
                    .and_then(|names| dereference(self.resolver, names))
                    .and_then(|names| names.as_dict().and_then(|names| names.get(b"Dests")).cloned()),
            )?
        {
            found = name_tree_lookup(self.resolver, &tree, name)?;
        }
        let array = found.as_ref().and_then(|value| self.destination_array(value));
        self.named.insert(name.to_vec(), found);
        Ok(array)
    }

    /// A named destination's value: a destination array, or a dictionary whose `/D` is one.
    fn destination_array(&self, value: &Object) -> Option<Vec<Object>> {
        let value = dereference(self.resolver, value).ok()?;
        let array = match value.as_ref() {
            Object::Dictionary(dictionary) => {
                let destination = dictionary.get(b"D").ok()?;
                dereference(self.resolver, destination).ok()?.as_array().ok()?.clone()
            }
            other => other.as_array().ok()?.clone(),
        };
        Some(array)
    }

    /// `[page view args...]`, where the page is a page object reference or, as some producers
    /// write it, a page index counted from 0.
    fn explicit_destination(&self, array: &[Object]) -> Option<OutlineTarget> {
        let page = match array.first()? {
            Object::Reference(id) => *self.pages.get(id)?,
            Object::Integer(index) => {
                let page = u32::try_from(*index).ok()?.checked_add(1)?;
                (page as usize <= self.pages.len()).then_some(page)?
            }
            _ => return None,
        };
        let number = |index: usize| array.get(index).and_then(|value| value.as_float().ok());
        let view = match array.get(1).and_then(|view| view.as_name().ok()) {
            Some(b"XYZ") => DestinationView::Xyz {
                left: number(2),
                top: number(3),
                zoom: number(4),
            },
            Some(b"Fit") => DestinationView::Fit,
            Some(b"FitH") => DestinationView::FitH { top: number(2) },
            Some(b"FitV") => DestinationView::FitV { left: number(2) },
            Some(b"FitR") => match (number(2), number(3), number(4), number(5)) {
                (Some(left), Some(bottom), Some(right), Some(top)) => DestinationView::FitR {
                    left,
                    bottom,
                    right,
                    top,
                },
                _ => DestinationView::Unknown,
            },
            Some(b"FitB") => DestinationView::FitB,
            Some(b"FitBH") => DestinationView::FitBH { top: number(2) },
            Some(b"FitBV") => DestinationView::FitBV { left: number(2) },
            _ => DestinationView::Unknown,
        };
        Some(OutlineTarget { page, view })
    }
}

/// The value stored under `key` in the name tree rooted at `root`. Nodes whose `/Limits` exclude
/// the key are skipped, and a node reached twice is read once.
fn name_tree_lookup<R: ObjectResolver + ?Sized>(resolver: &R, root: &Object, key: &[u8]) -> Result<Option<Object>> {
    let mut visited = HashSet::new();
    let mut stack = vec![(root.clone(), 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth > MAX_NAME_TREE_DEPTH {
            continue;
        }
        if let Ok(id) = node.as_reference()
            && !visited.insert(id)
        {
            continue;
        }
        let Some(node) = skip_unless_io(dereference(resolver, &node).and_then(|node| node.as_dict().cloned()))? else {
            continue;
        };
        if let Ok(limits) = node.get(b"Limits").and_then(Object::as_array)
            && let (Some(Ok(low)), Some(Ok(high))) =
                (limits.first().map(Object::as_str), limits.get(1).map(Object::as_str))
            && (key < low || key > high)
        {
            continue;
        }
        if let Some(names) = skip_unless_io(
            node.get(b"Names")
                .and_then(|names| dereference(resolver, names))
                .and_then(|names| names.as_array().cloned()),
        )? {
            for pair in names.chunks(2) {
                if let [name, value] = pair
                    && name.as_str().ok() == Some(key)
                {
                    return Ok(Some(value.clone()));
                }
            }
        }
        if let Some(kids) = skip_unless_io(
            node.get(b"Kids")
                .and_then(|kids| dereference(resolver, kids))
                .and_then(|kids| kids.as_array().cloned()),
        )? {
            stack.extend(kids.into_iter().rev().map(|kid| (kid, depth + 1)));
        }
    }
    Ok(None)
}
