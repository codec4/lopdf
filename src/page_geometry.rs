//! A page's visible box and orientation as a viewer displays it, and where a destination's view
//! puts the top of the window on it.

use std::collections::HashSet;

use crate::resolver::{ObjectResolver, dereference, skip_unless_io};
use crate::{DestinationView, Object, ObjectId, Result};

/// Deepest page-tree ancestry searched for an inherited page attribute.
const MAX_INHERITANCE_DEPTH: usize = 64;

/// A rectangle in PDF user space, with `left <= right` and `bottom <= top`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PageRect {
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

impl PageRect {
    /// US Letter, the page box pdfium and pdf.js use for a page without a usable `/MediaBox`.
    pub const LETTER: Self = Self {
        left: 0.0,
        bottom: 0.0,
        right: 612.0,
        top: 792.0,
    };

    /// Reads `[x1 y1 x2 y2]`, with its corners in either order.
    pub(crate) fn read<R: ObjectResolver + ?Sized>(resolver: &R, object: &Object) -> Result<Option<Self>> {
        let Some(object) = skip_unless_io(dereference(resolver, object))? else {
            return Ok(None);
        };
        let Ok(array) = object.as_array() else {
            return Ok(None);
        };
        let mut numbers = [0.0; 4];
        if array.len() != numbers.len() {
            return Ok(None);
        }
        for (number, value) in numbers.iter_mut().zip(array) {
            let value = skip_unless_io(dereference(resolver, value))?;
            match value.and_then(|value| value.as_float().ok()) {
                Some(value) => *number = value,
                None => return Ok(None),
            }
        }
        let [x1, y1, x2, y2] = numbers;
        Ok(Some(Self {
            left: x1.min(x2),
            bottom: y1.min(y2),
            right: x1.max(x2),
            top: y1.max(y2),
        }))
    }

    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    pub fn height(&self) -> f32 {
        self.top - self.bottom
    }

    pub fn is_empty(&self) -> bool {
        self.left >= self.right || self.bottom >= self.top
    }

    /// The overlap of the two rectangles, or an empty rectangle at the origin when they do not
    /// overlap, as in pdfium.
    pub fn intersection(&self, other: &Self) -> Self {
        let overlap = Self {
            left: self.left.max(other.left),
            bottom: self.bottom.max(other.bottom),
            right: self.right.min(other.right),
            top: self.top.min(other.top),
        };
        if overlap.left > overlap.right || overlap.bottom > overlap.top {
            Self::default()
        } else {
            overlap
        }
    }
}

/// A rectangle on a page as it is displayed, in points from the displayed page's top-left corner,
/// with `left <= right` and `top <= bottom`: the space a viewer reports a tap in.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DisplayedRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// A page as viewers display it. pdfium, which the Android PDF renderer uses, shows the
/// `/CropBox` clipped to the `/MediaBox`, turned clockwise by `/Rotate`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageGeometry {
    /// The part of the page that is displayed, in user space.
    pub visible: PageRect,
    /// Clockwise quarter turns, 0 to 3.
    pub quarter_turns: u8,
}

impl PageGeometry {
    /// Reads `page`'s boxes and rotation, inherited through the page tree, as pdfium does:
    /// - a missing or empty `/MediaBox` is [`PageRect::LETTER`];
    /// - a missing or empty `/CropBox` shows the whole media box;
    /// - `/Rotate` counts whole quarter turns, and a negative angle turns counterclockwise.
    ///
    /// Only a failure to read the source is an error.
    pub fn read<R: ObjectResolver + ?Sized>(resolver: &R, page: ObjectId) -> Result<Self> {
        let mut media_box = None;
        let mut crop_box = None;
        let mut rotate = None;
        let mut visited = HashSet::new();
        let mut node = Some(page);
        while let Some(id) = node.take() {
            if visited.len() >= MAX_INHERITANCE_DEPTH || !visited.insert(id) {
                break;
            }
            let Some(dictionary) = skip_unless_io(resolver.object(id))? else {
                break;
            };
            let Ok(dictionary) = dictionary.as_dict() else {
                break;
            };
            for (key, value) in [
                (b"MediaBox".as_slice(), &mut media_box),
                (b"CropBox".as_slice(), &mut crop_box),
                (b"Rotate".as_slice(), &mut rotate),
            ] {
                if value.is_none() {
                    *value = dictionary.get(key).ok().cloned();
                }
            }
            node = dictionary.get(b"Parent").and_then(Object::as_reference).ok();
        }

        let media_box = match &media_box {
            Some(media_box) => PageRect::read(resolver, media_box)?,
            None => None,
        }
        .filter(|media_box| !media_box.is_empty())
        .unwrap_or(PageRect::LETTER);
        let crop_box = match &crop_box {
            Some(crop_box) => PageRect::read(resolver, crop_box)?,
            None => None,
        };
        let visible = match crop_box {
            Some(crop_box) if !crop_box.is_empty() => crop_box.intersection(&media_box),
            _ => media_box,
        };
        let degrees = match &rotate {
            Some(rotate) => skip_unless_io(dereference(resolver, rotate))?.and_then(|rotate| match rotate.as_ref() {
                Object::Integer(degrees) => Some(*degrees),
                Object::Real(degrees) => Some(*degrees as i64),
                _ => None,
            }),
            None => None,
        };
        Ok(Self {
            visible,
            quarter_turns: (degrees.unwrap_or(0) / 90).rem_euclid(4) as u8,
        })
    }

    /// Width and height of the page as displayed, in points.
    pub fn displayed_size(&self) -> (f32, f32) {
        if self.quarter_turns.is_multiple_of(2) {
            (self.visible.width(), self.visible.height())
        } else {
            (self.visible.height(), self.visible.width())
        }
    }

    /// Where `rect`, in user space, sits on the page as displayed: shifted to the visible box's
    /// corner, turned with the page, and measured down from the top, as a viewer reports a tap.
    ///
    /// Turning the page a quarter clockwise brings its left edge to the top and its bottom edge to
    /// the left, so a point's distance down the displayed page is its distance from the left edge
    /// in user space, and so on round the page, the same way [`Self::top_fraction`] reads a view.
    pub fn displayed_rect(&self, rect: PageRect) -> DisplayedRect {
        let visible = &self.visible;
        let displayed = |x: f32, y: f32| match self.quarter_turns {
            0 => (x - visible.left, visible.top - y),
            1 => (y - visible.bottom, x - visible.left),
            2 => (visible.right - x, y - visible.bottom),
            _ => (visible.top - y, visible.right - x),
        };
        // Opposite corners stay opposite through a quarter turn, so two are enough.
        let (x1, y1) = displayed(rect.left, rect.bottom);
        let (x2, y2) = displayed(rect.right, rect.top);
        DisplayedRect {
            left: x1.min(x2),
            top: y1.min(y2),
            right: x1.max(x2),
            bottom: y1.max(y2),
        }
    }

    /// Where `view` puts the top of the window, as a fraction of the displayed page's height from
    /// its top edge: negative above the page, and over 1 below it. `None` when the view gives no
    /// such coordinate or the page has no height.
    ///
    /// Turning the page clockwise brings its left, bottom, and then right edge to the top, so a
    /// turned page is measured along the view's left coordinate instead of its top.
    pub fn top_fraction(&self, view: DestinationView) -> Option<f64> {
        let (left, top) = match view {
            DestinationView::Xyz { left, top, .. } => (left, top),
            DestinationView::FitH { top } | DestinationView::FitBH { top } => (None, top),
            DestinationView::FitV { left } | DestinationView::FitBV { left } => (left, None),
            DestinationView::FitR { left, top, .. } => (Some(left), Some(top)),
            DestinationView::Fit | DestinationView::FitB | DestinationView::Unknown => return None,
        };
        let visible = &self.visible;
        let (offset, length) = match self.quarter_turns {
            0 => (visible.top - top?, visible.height()),
            1 => (left? - visible.left, visible.width()),
            2 => (top? - visible.bottom, visible.height()),
            _ => (visible.right - left?, visible.width()),
        };
        let fraction = f64::from(offset) / f64::from(length);
        fraction.is_finite().then_some(fraction)
    }
}
