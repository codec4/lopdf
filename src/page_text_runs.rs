//! A page's text in runs, each with the font that set it, read into the forms the page draws.
//!
//! A font whose `/ToUnicode` is missing or wrong, and whose glyphs are named after the letters at
//! their codes, reports characters it does not draw: a maths font draws `⟨` at the code of `k`,
//! and the text says `k`. Nothing in the text tells the two apart; only the font it came from
//! does, which is why each run carries its font's name. Where the font's encoding reads a byte at
//! a time, the run also keeps each code it showed and the names the font's `/Differences` gives
//! them, so a caller that knows a font's glyphs can read them. A page drawn inside a Form XObject,
//! as a third of some textbooks' pages are, has its text in the form, read with the form's own
//! resources.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::encodings::Encoding;
use crate::page_content::PageContent;
use crate::parser;
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
    /// Each code shown, with the byte offset in `text` of the one character it decoded to, for a
    /// font whose encoding reads a byte at a time. A code that decoded to no character, or to
    /// several, is left out, and so is every code of a font read through its `/ToUnicode` alone.
    pub codes: Vec<(usize, u8)>,
    /// The glyph names the font's `/Differences` gives codes, spelt as in the file. A name with no
    /// Unicode reading, such as a type foundry's catalogue number, is kept here, although the text
    /// then falls back to the standard encoding for the whole font.
    pub differences: Rc<BTreeMap<u8, Vec<u8>>>,
}

/// A font a content stream names.
struct Font<'a> {
    /// `/BaseFont` without a subset prefix.
    name: Vec<u8>,
    encoding: Encoding<'a>,
    differences: Rc<BTreeMap<u8, Vec<u8>>>,
}

/// What a content stream can name: its fonts and its forms.
struct Scope<'a> {
    fonts: BTreeMap<Vec<u8>, Rc<Font<'a>>>,
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
            if let Some(font) = self.run_font(font, limits) {
                scope.fonts.insert(name, font);
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
        reader.read(&scope, &mut content, None)?;
        Ok(reader.runs)
    }

    fn run_font<'a>(&'a self, font: &'a Dictionary, limits: DecodeLimits) -> Option<Rc<Font<'a>>> {
        let encoding = font.get_font_encoding_with_limit(self, limits.max_stream_size).ok()?;
        Some(Rc::new(Font {
            name: base_font(font),
            encoding,
            differences: Rc::new(self.differences(font)),
        }))
    }

    /// The names a font's `/Differences` gives its codes, whether or not a name has a Unicode
    /// reading.
    fn differences(&self, font: &Dictionary) -> BTreeMap<u8, Vec<u8>> {
        let mut names = BTreeMap::new();
        let Some(differences) = font
            .get(b"Encoding")
            .ok()
            .and_then(|e| self.dereference(e).ok())
            .and_then(|(_, e)| e.as_dict().ok())
            .and_then(|e| e.get(b"Differences").ok())
            .and_then(|d| self.dereference(d).ok())
            .and_then(|(_, d)| d.as_array().ok())
        else {
            return names;
        };
        let mut code = None;
        for entry in differences {
            match entry {
                Object::Integer(at) => code = u8::try_from(*at).ok(),
                Object::Name(name) => {
                    if let Some(at) = code {
                        names.insert(at, name.clone());
                    }
                    code = code.and_then(|at| at.checked_add(1));
                }
                _ => {}
            }
        }
        names
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
                if let Some(font) = self.run_font(font, limits) {
                    fonts.insert(name.clone(), font);
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
    /// Reads a content stream, starting in [`font`](Font), the font of the graphics state it is
    /// drawn in: a form inherits the font of the content that draws it.
    fn read(&mut self, scope: &Scope<'a>, content: &mut PageContent<'c>, mut font: Option<Rc<Font<'a>>>) -> Result<()> {
        let max_operation_size = self.limits.max_stream_size;
        let operations =
            parser::ChunkedContentOperations::new(|buffer: &mut Vec<u8>| content.read_into(buffer), max_operation_size);
        // The font is part of the graphics state, which `q` saves and `Q` restores.
        let mut saved: Vec<Option<Rc<Font<'a>>>> = Vec::new();
        let mut run = Run::default();
        for operation in operations {
            let operation = operation?;
            match operation.operator.as_ref() {
                "Tf" => {
                    self.push(font.as_ref(), &mut run);
                    font = operation
                        .operands
                        .first()
                        .and_then(|name| name.as_name().ok())
                        .and_then(|name| scope.fonts.get(name))
                        .cloned();
                }
                "q" => saved.push(font.clone()),
                "Q" => {
                    if let Some(restored) = saved.pop() {
                        if !same_font(restored.as_ref(), font.as_ref()) {
                            self.push(font.as_ref(), &mut run);
                        }
                        font = restored;
                    }
                }
                "Tj" | "TJ" => {
                    if let Some(font) = &font {
                        let _ = run.collect(&font.encoding, &operation.operands);
                    }
                }
                "'" | "\"" => {
                    if let Some(font) = &font {
                        run.line_break();
                        let shown = if operation.operator == "'" { 0 } else { 2 };
                        if let Some(string) = operation.operands.get(shown) {
                            let _ = run.collect(&font.encoding, std::slice::from_ref(string));
                        }
                    }
                }
                "T*" | "ET" => run.line_break(),
                "Do" => {
                    self.push(font.as_ref(), &mut run);
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
                    let read = self.read(&inner, &mut form, font.clone());
                    self.stack.pop();
                    self.spent += form.len();
                    if read.is_err() {
                        continue;
                    }
                }
                _ => {}
            }
        }
        self.push(font.as_ref(), &mut run);
        self.spent += content.len();
        Ok(())
    }

    fn push(&mut self, font: Option<&Rc<Font<'a>>>, run: &mut Run) {
        if run.text.is_empty() {
            return;
        }
        let Run { text, codes } = std::mem::take(run);
        self.runs.push(PageTextRun {
            font: font.map(|font| font.name.clone()).unwrap_or_default(),
            text,
            codes,
            differences: font.map(|font| font.differences.clone()).unwrap_or_default(),
        });
    }
}

fn same_font(a: Option<&Rc<Font>>, b: Option<&Rc<Font>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// The text of the run being read, and its codes.
#[derive(Default)]
struct Run {
    text: String,
    codes: Vec<(usize, u8)>,
}

impl Run {
    /// Reads the strings a text operator shows as the plain text extraction does, byte by byte
    /// where the encoding reads a byte at a time.
    fn collect(&mut self, encoding: &Encoding, operands: &[Object]) -> Result<()> {
        for operand in operands {
            match operand {
                Object::String(bytes, _) => self.show(encoding, bytes)?,
                Object::Array(array) => {
                    self.collect(encoding, array)?;
                    self.text.push(' ');
                }
                Object::Integer(i) if *i < -100 => self.text.push(' '),
                _ => {}
            }
        }
        Ok(())
    }

    fn show(&mut self, encoding: &Encoding, bytes: &[u8]) -> Result<()> {
        if !matches!(
            encoding,
            Encoding::OneByteEncoding(_) | Encoding::Differences(_) | Encoding::SimpleEncoding(b"WinAnsiEncoding")
        ) {
            return encoding.write_to_string(bytes, &mut self.text);
        }
        for &code in bytes {
            let at = self.text.len();
            encoding.write_to_string(&[code], &mut self.text)?;
            if self.text[at..].chars().count() == 1 {
                self.codes.push((at, code));
            }
        }
        Ok(())
    }

    fn line_break(&mut self) {
        if !self.text.ends_with('\n') {
            self.text.push('\n');
        }
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
