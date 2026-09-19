//! The page labels of a document: the page numbers a reader shows, such as `xii` or `A-3`, from
//! the catalog's `/PageLabels` number tree.

use std::cell::OnceCell;
use std::collections::HashMap;

use crate::resolver::{ObjectResolver, dereference, skip_unless_io};
use crate::{Dictionary, Object, ObjectId, Result, StringFormat, decode_text_string};

/// Deepest number-tree nesting read.
const MAX_TREE_DEPTH: usize = 32;
/// Most number-tree nodes read.
const MAX_NODES: usize = 10_000;
/// Most number-tree entries read.
const MAX_ENTRIES: usize = 100_000;

/// The label of each page, read as pdfium reads them, so that they match what other readers show.
///
/// A page takes the range that the number tree's lower bound for its index gives: the range's
/// `/P` prefix, then the page's number within it, counted from `/St` (1 when absent), in the `/S`
/// style: decimal, upper or lower Roman, or upper or lower letters (`A` to `Z`, then `AA`). A
/// range without a style gives the prefix alone. A page without a range, or whose range is not a
/// dictionary, takes its physical number.
///
/// pdfium finds the lower bound in its own way, which only a malformed tree tells apart from the
/// standard's: it scans `/Nums` and `/Kids` from the end, and a page at or past a node's upper
/// `/Limits` takes the range listed at that limit, or its physical number when there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageLabels {
    /// The label of each page, in page order.
    pub labels: Vec<String>,
}

impl PageLabels {
    /// The labels of a document of `page_count` pages, or `None` when it has no `/PageLabels`.
    /// Only a failure to read the source is an error.
    pub fn read<R: ObjectResolver + ?Sized>(resolver: &R, page_count: usize) -> Result<Option<Self>> {
        let Some(root) = skip_unless_io(page_labels_tree(resolver))? else {
            return Ok(None);
        };
        let mut reader = TreeReader {
            resolver,
            tree: NumberTree { nodes: Vec::new() },
            ids: HashMap::new(),
            path: Vec::new(),
            entries: 0,
        };
        let root = reader.read_node(&root, None, 0)?;
        let tree = reader.tree;
        let labels = (0..page_count)
            .map(|index| {
                let number = i64::try_from(index).unwrap_or(i64::MAX);
                match tree.lower_bound(root, number) {
                    Some((key, Some(Object::Dictionary(range)))) => label(range, number - key),
                    _ => (index + 1).to_string(),
                }
            })
            .collect();
        Ok(Some(Self { labels }))
    }
}

fn page_labels_tree<R: ObjectResolver + ?Sized>(resolver: &R) -> Result<Dictionary> {
    let root = resolver.trailer().get(b"Root").and_then(Object::as_reference)?;
    let catalog = resolver.object(root)?;
    let tree = dereference(resolver, catalog.as_dict()?.get(b"PageLabels")?)?;
    tree.as_dict().cloned()
}

/// The label of the page `offset` pages into the range `range`.
fn label(range: &Dictionary, offset: i64) -> String {
    let mut label = match range.get(b"P") {
        Ok(Object::Name(name)) => decode_text_string(&Object::String(name.clone(), StringFormat::Literal)),
        Ok(prefix) => decode_text_string(prefix),
        Err(_) => Ok(String::new()),
    }
    .unwrap_or_default();
    let first = range.get(b"St").map_or(1, integer);
    let number = offset.saturating_add(first);
    match range
        .get(b"S")
        .and_then(|style| style.as_name().or_else(|_| style.as_str()))
    {
        Ok(b"D") => label.push_str(&number.to_string()),
        Ok(b"R") => label.push_str(&roman(number).to_uppercase()),
        Ok(b"r") => label.push_str(&roman(number)),
        Ok(b"A") => label.push_str(&letters(number).to_uppercase()),
        Ok(b"a") => label.push_str(&letters(number)),
        _ => {}
    }
    label
}

/// `number` in lower-case Roman numerals, from its remainder by 1,000,000 as in pdfium; nothing
/// for zero or less.
fn roman(number: i64) -> String {
    const NUMERALS: [(i64, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut number = number % 1_000_000;
    let mut roman = String::new();
    for (value, numeral) in NUMERALS {
        while number >= value {
            number -= value;
            roman.push_str(numeral);
        }
    }
    roman
}

/// `number` as lower-case letters, as in pdfium: `a` to `z`, then `aa` to `zz`, and so on, with a
/// letter repeated at most 999 times.
fn letters(number: i64) -> String {
    if number == 0 {
        return String::new();
    }
    let index = number - 1;
    let count = (index / 26 + 1) % 1000;
    // pdfium's arithmetic, which a start below 1 takes below `a`.
    let letter = char::from((i64::from(b'a') + index % 26) as u8);
    std::iter::repeat_n(letter, usize::try_from(count).unwrap_or(0)).collect()
}

/// An integer as pdfium reads one: a real is truncated, and anything else is 0.
fn integer(object: &Object) -> i64 {
    match object {
        Object::Integer(value) => *value,
        Object::Real(value) => *value as i64,
        _ => 0,
    }
}

/// A number tree read once, with its references resolved.
struct NumberTree {
    nodes: Vec<Node>,
}

struct Node {
    limits: Option<(i64, i64)>,
    content: Content,
    /// The smallest index whose lower bound this node gives.
    threshold: i64,
    /// What an exact lookup finds at the upper limit.
    at_upper_limit: OnceCell<Option<usize>>,
}

enum Content {
    /// `/Nums` in the order the file lists them, and the smallest key from each entry on.
    Entries {
        keys: Vec<i64>,
        values: Vec<Object>,
        suffix_min: Vec<i64>,
    },
    /// `/Kids` in the order the file lists them, and the smallest threshold from each kid on.
    Kids {
        kids: Vec<usize>,
        suffix_min: Vec<i64>,
    },
    None,
}

impl NumberTree {
    /// The key and value that pdfium's lower bound for `number` gives in node `index`. The value
    /// is `None` when the node's upper limit is not listed under it.
    fn lower_bound(&self, index: usize, number: i64) -> Option<(i64, Option<&Object>)> {
        let node = &self.nodes[index];
        if let Some((low, high)) = node.limits {
            if number < low {
                return None;
            }
            if number >= high {
                let found = node.at_upper_limit.get_or_init(|| self.find_entry(index, high));
                return Some((high, found.map(|entry| self.entry_value(entry))));
            }
        }
        match &node.content {
            // Scanning from the end, the first entry or kid at or below `number` is the last one
            // whose suffix minimum is.
            Content::Entries {
                keys,
                values,
                suffix_min,
            } => {
                let last = suffix_min.partition_point(|&min| min <= number).checked_sub(1)?;
                Some((keys[last], Some(&values[last])))
            }
            Content::Kids { kids, suffix_min } => {
                let last = suffix_min.partition_point(|&min| min <= number).checked_sub(1)?;
                self.lower_bound(kids[last], number)
            }
            Content::None => None,
        }
    }

    /// The entry listed under `key` in node `index` or below, found as pdfium's exact lookup
    /// finds it, as the node and the entry's position packed into one number.
    fn find_entry(&self, index: usize, key: i64) -> Option<usize> {
        let node = &self.nodes[index];
        if let Some((low, high)) = node.limits
            && (key < low || key > high)
        {
            return None;
        }
        match &node.content {
            Content::Entries { keys, .. } => keys
                .iter()
                // pdfium stops at the first larger key, taking `/Nums` to be sorted.
                .take_while(|&&entry| entry <= key)
                .position(|&entry| entry == key)
                .map(|position| index * MAX_ENTRIES + position),
            Content::Kids { kids, .. } => kids.iter().find_map(|&kid| self.find_entry(kid, key)),
            Content::None => None,
        }
    }

    fn entry_value(&self, entry: usize) -> &Object {
        match &self.nodes[entry / MAX_ENTRIES].content {
            Content::Entries { values, .. } => &values[entry % MAX_ENTRIES],
            _ => unreachable!("an entry is in a node with entries"),
        }
    }
}

struct TreeReader<'a, R: ?Sized> {
    resolver: &'a R,
    tree: NumberTree,
    /// Nodes already read, by object id, so that a node listed twice is read once.
    ids: HashMap<ObjectId, usize>,
    /// The nodes from the root to the one being read, so that a cycle ends.
    path: Vec<ObjectId>,
    entries: usize,
}

impl<R: ObjectResolver + ?Sized> TreeReader<'_, R> {
    /// Reads `node`, with object id `id`, and the nodes under it, and returns its index.
    fn read_node(&mut self, node: &Dictionary, id: Option<ObjectId>, depth: usize) -> Result<usize> {
        let resolver = self.resolver;
        let array = |key: &[u8]| {
            skip_unless_io(
                node.get(key)
                    .and_then(|value| dereference(resolver, value))
                    .and_then(|value| value.as_array().cloned()),
            )
        };
        let limits = array(b"Limits")?.map(|limits| {
            let bound = |position: usize| limits.get(position).map_or(0, |bound| self.integer(bound));
            (bound(0), bound(1))
        });
        let content = if let Some(nums) = array(b"Nums")? {
            let mut keys = Vec::with_capacity(nums.len() / 2);
            let mut values = Vec::with_capacity(nums.len() / 2);
            for [key, value] in nums.as_chunks::<2>().0 {
                if self.entries >= MAX_ENTRIES {
                    break;
                }
                self.entries += 1;
                keys.push(self.integer(key));
                values.push(self.range(value)?);
            }
            let suffix_min = suffix_minimum(&keys);
            Content::Entries {
                keys,
                values,
                suffix_min,
            }
        } else if let Some(kid_objects) = array(b"Kids")?
            && depth < MAX_TREE_DEPTH
        {
            let mut kids = Vec::with_capacity(kid_objects.len());
            for kid in kid_objects {
                if let Some(kid) = self.read_kid(&kid, depth + 1)? {
                    kids.push(kid);
                }
            }
            let thresholds: Vec<i64> = kids.iter().map(|&kid| self.tree.nodes[kid].threshold).collect();
            Content::Kids {
                kids,
                suffix_min: suffix_minimum(&thresholds),
            }
        } else {
            Content::None
        };
        let inner = match &content {
            Content::Entries { suffix_min, .. } | Content::Kids { suffix_min, .. } => {
                suffix_min.first().copied().unwrap_or(i64::MAX)
            }
            Content::None => i64::MAX,
        };
        let threshold = match limits {
            // At or past the upper limit, a node always gives a lower bound.
            Some((low, high)) => low.max(high.min(inner)),
            None => inner,
        };
        let index = self.tree.nodes.len();
        self.tree.nodes.push(Node {
            limits,
            content,
            threshold,
            at_upper_limit: OnceCell::new(),
        });
        if let Some(id) = id {
            self.ids.insert(id, index);
        }
        Ok(index)
    }

    fn read_kid(&mut self, kid: &Object, depth: usize) -> Result<Option<usize>> {
        let id = kid.as_reference().ok();
        if let Some(id) = id {
            if let Some(&index) = self.ids.get(&id) {
                return Ok(Some(index));
            }
            if self.path.contains(&id) || self.tree.nodes.len() >= MAX_NODES {
                return Ok(None);
            }
        }
        let resolver = self.resolver;
        let Some(node) = skip_unless_io(dereference(resolver, kid).and_then(|kid| kid.as_dict().cloned()))? else {
            return Ok(None);
        };
        self.path.extend(id);
        let index = self.read_node(&node, id, depth);
        if id.is_some() {
            self.path.pop();
        }
        index.map(Some)
    }

    /// A range: the value resolved, and the entries of a range dictionary resolved too.
    fn range(&self, value: &Object) -> Result<Object> {
        let Some(value) = skip_unless_io(dereference(self.resolver, value))? else {
            return Ok(Object::Null);
        };
        let Object::Dictionary(range) = value.as_ref() else {
            return Ok(value.into_owned());
        };
        let mut resolved = Dictionary::new();
        for (key, entry) in range.iter() {
            if let Some(entry) = skip_unless_io(dereference(self.resolver, entry))? {
                resolved.set(key.clone(), entry.into_owned());
            }
        }
        Ok(Object::Dictionary(resolved))
    }

    fn integer(&self, object: &Object) -> i64 {
        dereference(self.resolver, object).map_or(0, |object| integer(&object))
    }
}

/// The smallest of `values` from each position to the end, which never decreases.
fn suffix_minimum(values: &[i64]) -> Vec<i64> {
    let mut minimum = vec![i64::MAX; values.len()];
    let mut running = i64::MAX;
    for (position, value) in values.iter().enumerate().rev() {
        running = running.min(*value);
        minimum[position] = running;
    }
    minimum
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Document, Object, StringFormat};

    fn document(pages: usize, page_labels: Option<Object>) -> Document {
        let mut doc = Document::with_version("1.7");
        let tree_id = doc.new_object_id();
        let kids: Vec<Object> = (0..pages)
            .map(|_| {
                doc.add_object(dictionary! { "Type" => "Page", "Parent" => tree_id })
                    .into()
            })
            .collect();
        doc.objects.insert(
            tree_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => pages as i64 }),
        );
        let mut catalog = dictionary! { "Type" => "Catalog", "Pages" => tree_id };
        if let Some(page_labels) = page_labels {
            catalog.set("PageLabels", page_labels);
        }
        let catalog = doc.add_object(catalog);
        doc.trailer.set("Root", catalog);
        doc
    }

    fn set_tree(doc: &mut Document, tree: Object) {
        let catalog = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(catalog).unwrap().set("PageLabels", tree);
    }

    fn labels(doc: &Document, pages: usize) -> Option<Vec<String>> {
        PageLabels::read(doc, pages).unwrap().map(|labels| labels.labels)
    }

    fn range(style: Option<&str>, prefix: Option<&str>, first: Option<i64>) -> Object {
        let mut range = Dictionary::new();
        if let Some(style) = style {
            range.set("S", Object::Name(style.as_bytes().to_vec()));
        }
        if let Some(prefix) = prefix {
            range.set("P", Object::string_literal(prefix));
        }
        if let Some(first) = first {
            range.set("St", first);
        }
        Object::Dictionary(range)
    }

    #[test]
    fn a_document_without_page_labels_has_none() {
        assert_eq!(labels(&document(3, None), 3), None);
    }

    #[test]
    fn every_style_prefix_and_start() {
        let nums = vec![
            0.into(),
            range(None, Some("Cover"), None),
            1.into(),
            range(Some("r"), None, None),
            4.into(),
            range(Some("D"), None, None),
            6.into(),
            range(Some("D"), Some("A-"), Some(9)),
            8.into(),
            range(Some("R"), None, Some(1999)),
            9.into(),
            range(Some("A"), None, Some(26)),
            12.into(),
            range(Some("a"), Some("x"), None),
        ];
        let doc = document(14, Some(Object::Dictionary(dictionary! { "Nums" => nums })));

        assert_eq!(
            labels(&doc, 14).unwrap(),
            [
                "Cover", "i", "ii", "iii", "1", "2", "A-9", "A-10", "MCMXCIX", "Z", "AA", "BB", "xa", "xb"
            ]
        );
    }

    #[test]
    fn pages_before_the_first_range_take_their_physical_numbers() {
        let nums = vec![2.into(), range(Some("r"), None, None), 3.into(), Object::Integer(7)];
        let doc = document(5, Some(Object::Dictionary(dictionary! { "Nums" => nums })));

        // A range that is not a dictionary gives physical numbers too.
        assert_eq!(labels(&doc, 5).unwrap(), ["1", "2", "i", "4", "5"]);
    }

    #[test]
    fn kids_limits_references_and_text_string_prefixes() {
        let mut doc = document(6, None);
        let prefix = Object::String([0xfe, 0xff, 0x00, 0xc9, 0x00, 0x2d].to_vec(), StringFormat::Hexadecimal);
        let first_value = doc.add_object(3);
        let late_range = doc.add_object(dictionary! { "S" => "D", "P" => prefix, "St" => first_value });
        let first = doc.add_object(dictionary! {
            "Limits" => vec![0.into(), 0.into()],
            "Nums" => vec![0.into(), range(Some("r"), None, None)],
        });
        let second = doc.add_object(dictionary! {
            "Limits" => vec![3.into(), 3.into()],
            "Nums" => vec![3.into(), Object::Reference(late_range)],
        });
        let root = doc.add_object(dictionary! { "Kids" => vec![first.into(), second.into()] });
        set_tree(&mut doc, root.into());

        assert_eq!(
            labels(&doc, 6).unwrap(),
            ["i", "ii", "iii", "\u{c9}-3", "\u{c9}-4", "\u{c9}-5"]
        );
    }

    #[test]
    fn malformed_trees_read_as_pdfium_reads_them() {
        // Out of order, scanned from the end: from index 1 on, the last key at or below it is 1.
        let unordered = Object::Dictionary(dictionary! {
            "Nums" => vec![
                0.into(), range(Some("D"), None, None),
                3.into(), range(Some("r"), None, None),
                1.into(), range(Some("A"), None, None),
            ],
        });
        assert_eq!(
            labels(&document(5, Some(unordered)), 5).unwrap(),
            ["1", "A", "B", "C", "D"]
        );

        // At or past its upper limit, a node gives the range listed at that limit, and a page
        // takes its physical number when there is none.
        let mut doc = document(6, None);
        let listed = doc.add_object(dictionary! {
            "Limits" => vec![0.into(), 2.into()],
            "Nums" => vec![0.into(), range(Some("r"), None, None), 2.into(), range(Some("A"), None, None)],
        });
        let unlisted = doc.add_object(dictionary! {
            "Limits" => vec![4.into(), 4.into()],
            "Nums" => vec![5.into(), range(Some("D"), Some("X-"), None)],
        });
        let root = doc.add_object(dictionary! { "Kids" => vec![listed.into(), unlisted.into()] });
        set_tree(&mut doc, root.into());
        assert_eq!(labels(&doc, 6).unwrap(), ["i", "ii", "A", "B", "5", "6"]);
    }

    #[test]
    fn a_kid_listed_twice_or_in_a_cycle_is_read_once() {
        let mut doc = document(4, None);
        let root_id = doc.new_object_id();
        let leaf = doc.add_object(dictionary! { "Nums" => vec![0.into(), range(Some("r"), None, None)] });
        doc.objects.insert(
            root_id,
            Object::Dictionary(dictionary! {
                "Kids" => vec![leaf.into(), Object::Reference(root_id), leaf.into()],
            }),
        );
        set_tree(&mut doc, root_id.into());

        assert_eq!(labels(&doc, 4).unwrap(), ["i", "ii", "iii", "iv"]);
    }

    #[test]
    fn roman_and_letters_follow_pdfium_at_the_edges() {
        assert_eq!(roman(0), "");
        assert_eq!(roman(-5), "");
        assert_eq!(roman(4), "iv");
        assert_eq!(roman(3999), "mmmcmxcix");
        assert_eq!(roman(1_000_001), "i");
        assert_eq!(letters(0), "");
        assert_eq!(letters(26), "z");
        assert_eq!(letters(27), "aa");
        assert_eq!(letters(53), "aaa");
    }
}
