#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use super::{Document, DocumentOutline, Error, Result};

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub struct TocType {
    pub level: usize,
    pub title: String,
    pub page: usize,
}

#[allow(dead_code)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Default)]
pub struct Toc {
    pub toc: Vec<TocType>,
    pub errors: Vec<String>,
}

impl Toc {
    pub fn new() -> Self {
        Toc {
            toc: Vec::new(),
            errors: Vec::new(),
        }
    }
}

impl Document {
    /// The outline as page numbers, read by [`DocumentOutline`]. Items without a page in this
    /// document are left out, and `errors` says which items and how many unreadable ones.
    pub fn get_toc(&self) -> Result<Toc> {
        if self.catalog()?.get(b"Outlines").is_err() {
            return Err(Error::NoOutline);
        }
        let outline = DocumentOutline::read(self)?;
        let mut toc = Toc::new();
        for item in outline.items {
            match item.target {
                Some(target) => toc.toc.push(TocType {
                    level: item.depth + 1,
                    title: item.title,
                    page: target.page as usize,
                }),
                None => toc
                    .errors
                    .push(format!("Outline item {:?} has no page in this document", item.title)),
            }
        }
        if outline.skipped > 0 {
            toc.errors
                .push(format!("{} outline items could not be read", outline.skipped));
        }
        if outline.truncated {
            toc.errors
                .push("The outline is larger than the default outline limits".to_string());
        }
        Ok(toc)
    }
}

#[cfg(not(feature = "async"))]
#[cfg(test)]
mod tests {
    use crate::{Document, TocType};

    #[test]
    fn parse_toc() {
        let expected = vec![
            TocType {
                level: 1,
                title: String::from("1. Flesh Fruits"),
                page: 1,
            },
            TocType {
                level: 2,
                title: String::from("1.1. Stone Fruits"),
                page: 2,
            },
            TocType {
                level: 3,
                title: String::from("1.1.1. Peaches"),
                page: 3,
            },
            TocType {
                level: 3,
                title: String::from("1.1.2. Plums"),
                page: 6,
            },
            TocType {
                level: 2,
                title: String::from("1.2. Pomes"),
                page: 28,
            },
            TocType {
                level: 3,
                title: String::from("1.2.1. Apples"),
                page: 30,
            },
            TocType {
                level: 3,
                title: String::from("1.2.2. Pears"),
                page: 35,
            },
            TocType {
                level: 2,
                title: String::from("Summary"),
                page: 36,
            },
            TocType {
                level: 1,
                title: String::from("2. Berries & Hesperidia"),
                page: 40,
            },
            TocType {
                level: 2,
                title: String::from("2.1. True Berries"),
                page: 40,
            },
            TocType {
                level: 2,
                title: String::from("Summary"),
                page: 41,
            },
            TocType {
                level: 1,
                title: String::from("3. The End"),
                page: 100,
            },
        ];

        let doc = Document::load("assets/test.pdf").unwrap();
        let toc = doc.get_toc().unwrap();
        assert_eq!(toc.toc, expected);
    }
}
