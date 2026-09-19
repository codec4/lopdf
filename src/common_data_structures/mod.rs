use crate::{
    Error, Object, Result, StringFormat,
    encodings::{self, bytes_to_string},
};

/// Creates a text string.
/// If the input only contains ASCII characters, the string is encoded
/// in PDFDocEncoding, otherwise in UTF-16BE.
pub fn text_string(text: &str) -> Object {
    if text.is_ascii() {
        return Object::String(text.into(), StringFormat::Literal);
    }
    let mut string = Vec::new();
    encodings::encode_utf16_be(text, &mut string);
    Object::String(string, StringFormat::Hexadecimal)
}

/// Decodes a text string.
/// Depending on the BOM at the start of the string, a different encoding is chosen.
/// All encodings specified in PDF2.0 are supported (PDFDocEncoding, UTF-16BE,
/// and UTF-8).
pub fn decode_text_string(obj: &Object) -> Result<String> {
    let s = obj.as_str()?;
    if let Some(utf16) = s.strip_prefix(b"\xFE\xFF") {
        // Detected UTF-16BE BOM
        decode_utf16(utf16, u16::from_be_bytes)
    } else if let Some(utf16) = s.strip_prefix(b"\xFF\xFE") {
        // A UTF-16LE BOM is not allowed in a text string, but some producers write one and other
        // readers accept it.
        decode_utf16(utf16, u16::from_le_bytes)
    } else if let Some(utf8) = s.strip_prefix(b"\xEF\xBB\xBF") {
        // Detected UTF-8 BOM (PDF 2.0)
        String::from_utf8(utf8.to_vec()).map_err(|_| Error::TextStringDecode)
    } else {
        // If neither BOM is detected, PDFDocEncoding is used
        let mut out = String::new();
        bytes_to_string(&encodings::PDF_DOC_ENCODING, s, &mut out)?;
        Ok(out)
    }
}

/// Decodes UTF-16 code units from byte pairs. A trailing odd byte is taken as the first byte of a
/// final pair padded with zero.
fn decode_utf16(bytes: &[u8], from_bytes: fn([u8; 2]) -> u16) -> Result<String> {
    let units = bytes
        .chunks(2)
        .map(|pair| from_bytes([pair[0], pair.get(1).copied().unwrap_or(0)]))
        .collect::<Vec<u16>>();
    String::from_utf16(&units).map_err(|_| Error::TextStringDecode)
}

#[cfg(test)]
mod test {
    use crate::{
        Object, StringFormat, common_data_structures::decode_text_string, encodings, text_string, writer::Writer,
    };

    #[test]
    fn spec_example1_encode() {
        let input = "text‰";
        let text_string = encodings::string_to_bytes(&encodings::PDF_DOC_ENCODING, input);
        // let text_string = input.bytes().collect::<Vec<_>>();
        let dict = Object::Dictionary(dictionary!(
            "Key" => Object::String(text_string, StringFormat::Literal),
        ));
        let mut actual = vec![];
        Writer::write_object(&mut actual, &dict).unwrap();
        // "\x8B" is equivalent to the escaped version "\\213" which is used
        // in the original example.
        let expected = b"<</Key(text\x8B)>>";
        assert_eq!(actual.as_slice(), expected);
    }

    #[test]
    fn pdf_doc_encoding_keeps_tab_line_feed_and_carriage_return() {
        let text = Object::String(b"Two\tpart\rheading\n".to_vec(), StringFormat::Literal);
        assert_eq!(decode_text_string(&text).unwrap(), "Two\tpart\rheading\n");
        assert_eq!(
            encodings::string_to_bytes(&encodings::PDF_DOC_ENCODING, "a\tb\nc\rd"),
            b"a\tb\nc\rd"
        );
    }

    #[test]
    fn utf8_text_strings_decode_without_their_byte_order_mark() {
        let text = Object::String(b"\xEF\xBB\xBFCaf\xC3\xA9".to_vec(), StringFormat::Literal);
        assert_eq!(decode_text_string(&text).unwrap(), "Caf\u{e9}");
    }

    #[test]
    fn utf16_text_strings_decode_in_either_byte_order() {
        let big_endian = Object::String(b"\xFE\xFF\x00C\x00a\x00f\x00\xE9".to_vec(), StringFormat::Hexadecimal);
        let little_endian = Object::String(b"\xFF\xFEC\x00a\x00f\x00\xE9\x00".to_vec(), StringFormat::Hexadecimal);
        assert_eq!(decode_text_string(&big_endian).unwrap(), "Caf\u{e9}");
        assert_eq!(decode_text_string(&little_endian).unwrap(), "Caf\u{e9}");
    }

    #[test]
    fn spec_example1_decode() {
        let input = b"<</Key(text\\213)>>";
        let dict = crate::parser::direct_object(input).unwrap();
        let dict = dict.as_dict().unwrap();
        let actual = decode_text_string(dict.get(b"Key").unwrap()).unwrap();
        let expected = "text‰";
        assert_eq!(&actual, expected);
    }

    #[test]
    fn spec_example2_encode() {
        // Russian for "test"
        let input = "тест";
        // let text_string = encodings::string_to_bytes(encodings::PDF_DOC_ENCODING, input);
        let dict = Object::Dictionary(dictionary!(
            "Key" => text_string(input),
        ));
        let mut actual = vec![];
        Writer::write_object(&mut actual, &dict).unwrap();
        let expected = b"<</Key<FEFF0442043504410442>>>";
        assert_eq!(actual.as_slice(), expected);
    }

    #[test]
    fn spec_example2_decode() {
        let input = b"<</Key<FEFF0442043504410442>>>";
        let dict = crate::parser::direct_object(input).unwrap();
        let dict = dict.as_dict().unwrap();
        let actual = decode_text_string(dict.get(b"Key").unwrap()).unwrap();
        // Russian for "test"
        let expected = "тест";
        assert_eq!(&actual, expected);
    }
}
