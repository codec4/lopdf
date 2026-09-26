//! A page's text in runs, each with the font that set it, read into the forms the page draws.
//!
//! A font whose `/ToUnicode` is missing or wrong, and whose glyphs are named after the letters at
//! their codes, reports characters it does not draw: a maths font draws `⟨` at the code of `k`,
//! and the text says `k`. Nothing in the text tells the two apart; only the font it came from
//! does, which is why each run carries its font's name. A page drawn inside a Form XObject, as a
//! third of some textbooks' pages are, has its text in the form, read with the form's own
//! resources.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::encodings::Encoding;
use crate::page_content::PageContent;
use crate::parser;
use crate::parser_aux::collect_text;
use crate::{DecodeLimits, Dictionary, Document, Object, ObjectId, Result};

/// How deep forms drawn inside forms are followed.
const MAX_FORM_DEPTH: usize = 8;

/// Text set in one font, in the order the content draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageTextRun {
    /// The font's `/BaseFont`, without a subset's `ABCDEF+` prefix; empty when it has none.
    pub font: Vec<u8>,
    /// The text as the font's encoding decodes it, with the breaks and spaces the page's plain
    /// text extraction puts in.
    pub text: String,
}

/// A font a content stream names: its `/BaseFont` and its encoding.
type Font<'a> = Rc<(Vec<u8>, Encoding<'a>)>;

/// What a content stream can name: its fonts and its forms.
struct Scope<'a> {
    fonts: BTreeMap<Vec<u8>, Font<'a>>,
    forms: BTreeMap<Vec<u8>, ObjectId>,
}

impl Document {
    /// The text of page `page_id` in runs, each set in one font, reading into the forms it draws.
    /// `content` reads the page's content; `form_content` reads form `id`'s, given the content
    /// decoded so far against `limits`. A form that cannot be read is left out, not the page.
    pub(crate) fn page_text_runs<'c>(
        &self, page_id: ObjectId, limits: DecodeLimits, mut content: PageContent<'c>,
        form_content: &dyn Fn(ObjectId, usize) -> Result<PageContent<'c>>,
    ) -> Result<Vec<PageTextRun>> {
        let (own, inherited) = self.get_page_resources(page_id)?;
        let mut resources: Vec<&Dictionary> = own.into_iter().collect();
        resources.extend(inherited.iter().filter_map(|id| self.get_dictionary(*id).ok()));
        let mut scope = Scope {
            fonts: BTreeMap::new(),
            forms: BTreeMap::new(),
        };
        for (name, font) in self.get_page_fonts(page_id)? {
            if let Ok(encoding) = font.get_font_encoding_with_limit(self, limits.max_stream_size) {
                scope.fonts.insert(name, Rc::new((base_font(font), encoding)));
            }
        }
        for dictionary in resources.iter().rev() {
            scope.forms.extend(self.forms_of(dictionary));
        }
        let mut reader = RunReader {
            document: self,
            limits,
            form_content,
            runs: Vec::new(),
            spent: 0,
            stack: Vec::new(),
        };
        reader.read(&scope, &mut content)?;
        Ok(reader.runs)
    }

    /// The forms a resources dictionary's `/XObject` names; entries that are not references are
    /// left to be told apart when drawn.
    fn forms_of(&self, resources: &Dictionary) -> BTreeMap<Vec<u8>, ObjectId> {
        let Some(xobjects) = resources
            .get(b"XObject")
            .ok()
            .and_then(|x| self.dereference(x).ok())
            .and_then(|(_, x)| x.as_dict().ok())
        else {
            return BTreeMap::new();
        };
        xobjects
            .iter()
            .filter_map(|(name, value)| value.as_reference().ok().map(|id| (name.clone(), id)))
            .collect()
    }

    /// The scope of form `id`: the fonts and forms of its own `/Resources`, or, for a form with
    /// none, the scope it is drawn in. `None` when `id` is not a form this document holds.
    fn form_scope<'a>(&'a self, id: ObjectId, outer: &Scope<'a>, limits: DecodeLimits) -> Option<Scope<'a>> {
        let form = self.get_dictionary(id).ok()?;
        if form.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Form".as_slice()) {
            return None;
        }
        let Some(resources) = form
            .get(b"Resources")
            .ok()
            .and_then(|r| self.dereference(r).ok())
            .and_then(|(_, r)| r.as_dict().ok())
        else {
            return Some(Scope {
                fonts: outer.fonts.clone(),
                forms: outer.forms.clone(),
            });
        };
        let mut fonts = BTreeMap::new();
        if let Some(font_dict) = resources
            .get(b"Font")
            .ok()
            .and_then(|f| self.dereference(f).ok())
            .and_then(|(_, f)| f.as_dict().ok())
        {
            for (name, value) in font_dict.iter() {
                let Some(font) = self.dereference(value).ok().and_then(|(_, f)| f.as_dict().ok()) else {
                    continue;
                };
                if let Ok(encoding) = font.get_font_encoding_with_limit(self, limits.max_stream_size) {
                    fonts.insert(name.clone(), Rc::new((base_font(font), encoding)));
                }
            }
        }
        Some(Scope {
            fonts,
            forms: self.forms_of(resources),
        })
    }
}

struct RunReader<'a, 'c> {
    document: &'a Document,
    limits: DecodeLimits,
    form_content: &'a dyn Fn(ObjectId, usize) -> Result<PageContent<'c>>,
    runs: Vec<PageTextRun>,
    /// Content decoded so far, page and forms together, against the limits.
    spent: usize,
    /// The forms being read, outermost first, so a form that draws itself is read once.
    stack: Vec<ObjectId>,
}

impl<'a, 'c> RunReader<'a, 'c> {
    fn read(&mut self, scope: &Scope<'a>, content: &mut PageContent<'c>) -> Result<()> {
        let max_operation_size = self.limits.max_stream_size;
        let operations =
            parser::ChunkedContentOperations::new(|buffer: &mut Vec<u8>| content.read_into(buffer), max_operation_size);
        let mut font: Option<&Font<'a>> = None;
        let mut text = String::new();
        for operation in operations {
            let operation = operation?;
            match operation.operator.as_ref() {
                "Tf" => {
                    self.push(font, &mut text);
                    font = operation
                        .operands
                        .first()
                        .and_then(|name| name.as_name().ok())
                        .and_then(|name| scope.fonts.get(name));
                }
                "Tj" | "TJ" => {
                    if let Some(font) = font {
                        let _ = collect_text(&mut text, &font.1, &operation.operands);
                    }
                }
                "'" | "\"" => {
                    if let Some(font) = font {
                        if !text.ends_with('\n') {
                            text.push('\n');
                        }
                        let shown = if operation.operator == "'" { 0 } else { 2 };
                        if let Some(string) = operation.operands.get(shown) {
                            let _ = collect_text(&mut text, &font.1, std::slice::from_ref(string));
                        }
                    }
                }
                "T*" | "ET" if !text.ends_with('\n') => text.push('\n'),
                "Do" => {
                    self.push(font, &mut text);
                    let Some(id) = operation
                        .operands
                        .first()
                        .and_then(|name| name.as_name().ok())
                        .and_then(|name| scope.forms.get(name))
                        .copied()
                    else {
                        continue;
                    };
                    if self.stack.contains(&id) || self.stack.len() >= MAX_FORM_DEPTH {
                        continue;
                    }
                    let Some(inner) = self.document.form_scope(id, scope, self.limits) else {
                        continue;
                    };
                    let Ok(mut form) = (self.form_content)(id, self.spent) else {
                        continue;
                    };
                    self.stack.push(id);
                    let read = self.read(&inner, &mut form);
                    self.stack.pop();
                    self.spent += form.len();
                    if read.is_err() {
                        continue;
                    }
                }
                _ => {}
            }
        }
        self.push(font, &mut text);
        self.spent += content.len();
        Ok(())
    }

    fn push(&mut self, font: Option<&Font<'a>>, text: &mut String) {
        if text.is_empty() {
            return;
        }
        self.runs.push(PageTextRun {
            font: font.map(|font| font.0.clone()).unwrap_or_default(),
            text: std::mem::take(text),
        });
    }
}

/// A font's `/BaseFont` without a subset's six-letter prefix.
fn base_font(font: &Dictionary) -> Vec<u8> {
    let name = font.get(b"BaseFont").and_then(Object::as_name).unwrap_or_default();
    match name.iter().position(|&byte| byte == b'+') {
        Some(6) if name[..6].iter().all(u8::is_ascii_uppercase) => name[7..].to_vec(),
        _ => name.to_vec(),
    }
}
