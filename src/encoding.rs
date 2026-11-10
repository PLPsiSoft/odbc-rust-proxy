// Copyright (C) 2026 PSI SOFT Sp. z o.o.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//         http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

/// Obsługa dekodowania stron kodowych: CP852 (DOS Latin-2), Windows-1250, ISO-8859-2, UTF-8.

/// Tabela konwersji bajtów 0x80..=0xFF dla CP852 (DOS Latin 2 / EE8PC852).
/// Standard IBM Code Page 852.
const CP852_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'ů', 'ć', 'ç', 'ł', 'ë', 'Ő', 'ő', 'î', 'Ź', 'Ä', 'Ć', // 0x80 - 0x8F
    'É', 'Ĺ', 'ĺ', 'ô', 'ö', 'Ľ', 'ľ', 'Ś', 'ś', 'Ö', 'Ü', 'Ť', 'ť', 'Ł', '×', 'č', // 0x90 - 0x9F
    'á', 'í', 'ó', 'ú', 'Ą', 'ą', 'Ž', 'ž', 'Ę', 'ę', '¬', 'ź', 'Č', 'ş', '«', '»', // 0xA0 - 0xAF
    '░', '▒', '▓', '│', '┤', 'Á', 'Â', 'Ě', 'Ş', '╣', '║', '╗', '╝', 'Ż', 'ż', '┐', // 0xB0 - 0xBF
    '└', '┴', '┬', '├', '─', '┼', 'Ă', 'ă', '╚', '╔', '╩', '╦', '╠', '═', '╬', '¤', // 0xC0 - 0xCF
    'đ', 'Đ', 'Ď', 'Ë', 'ď', 'Ň', 'Í', 'Î', 'ě', '┘', '┌', '█', '▄', 'Ţ', 'Ů', '▀', // 0xD0 - 0xDF
    'Ó', 'ß', 'Ô', 'Ń', 'ń', 'ň', 'Š', 'š', 'Ŕ', 'Ú', 'ŕ', 'Ű', 'ý', 'Ý', 'ţ', '´', // 0xE0 - 0xEF
    '\u{00AD}', '˝', '˛', 'ˇ', '˘', '§', '÷', '¸', '°', '¨', '˙', 'ű', 'Ř', 'ř', '■', '\u{00A0}', // 0xF0 - 0xFF
];

/// Dekoduje bufor bajtów w CP852 do String.
pub fn decode_cp852(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if b < 0x80 {
            s.push(b as char);
        } else {
            s.push(CP852_HIGH[(b - 0x80) as usize]);
        }
    }
    s
}

/// Rozstrzyga i dekoduje bajty na podstawie wskazówki charsetu.
pub fn decode_bytes(bytes: &[u8], charset_hint: Option<&str>) -> String {
    let hint = charset_hint.map(|s| s.trim().to_ascii_lowercase());

    match hint.as_deref() {
        Some("cp852") | Some("ibm852") | Some("ee8pc852") | Some("852") => {
            decode_cp852(bytes)
        }
        Some("iso-8859-2") | Some("iso8859-2") | Some("latin2") | Some("ee8iso8859p2") => {
            let (cow, _, _) = encoding_rs::ISO_8859_2.decode(bytes);
            cow.into_owned()
        }
        Some("cp1250") | Some("windows-1250") | Some("ee8mswin1250") | Some("1250") => {
            let (cow, _, _) = encoding_rs::WINDOWS_1250.decode(bytes);
            cow.into_owned()
        }
        Some("utf-8") | Some("utf8") => {
            match std::str::from_utf8(bytes) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    // Fallback do Windows-1250 w przypadku nieprawidłowego UTF-8
                    let (cow, _, _) = encoding_rs::WINDOWS_1250.decode(bytes);
                    cow.into_owned()
                }
            }
        }
        _ => {
            // Domyślna heurystyka: jeśli poprawny UTF-8, użyj UTF-8; w przeciwnym razie Windows-1250
            match std::str::from_utf8(bytes) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    let (cow, _, _) = encoding_rs::WINDOWS_1250.decode(bytes);
                    cow.into_owned()
                }
            }
        }
    }
}

/// Konwertuje surowe bajty na format szesnastkowy (lowercase hex) dla kolumn binarnych.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cp852_polish_letters() {
        // Test wszystkich polskich znaków diakrytycznych w CP852
        // ą, ć, ę, ł, ń, ó, ś, ź, ż
        let cp852_lower = [0xA5, 0x86, 0xA9, 0x88, 0xE4, 0xA2, 0x98, 0xAB, 0xBE];
        let decoded_lower = decode_cp852(&cp852_lower);
        assert_eq!(decoded_lower, "ąćęłńóśźż");

        // Ą, Ć, Ę, Ł, Ń, Ó, Ś, Ź, Ż
        let cp852_upper = [0xA4, 0x8F, 0xA8, 0x9D, 0xE3, 0xE0, 0x97, 0x8D, 0xBD];
        let decoded_upper = decode_cp852(&cp852_upper);
        assert_eq!(decoded_upper, "ĄĆĘŁŃÓŚŹŻ");
    }

    #[test]
    fn test_decode_bytes_with_hints() {
        let cp852_bytes = [0xA5, 0x86, 0xA9]; // ąćę
        assert_eq!(decode_bytes(&cp852_bytes, Some("cp852")), "ąćę");
        assert_eq!(decode_bytes(&cp852_bytes, Some("EE8PC852")), "ąćę");

        let utf8_bytes = "Zażółć gęślą jaźń".as_bytes();
        assert_eq!(decode_bytes(utf8_bytes, Some("utf-8")), "Zażółć gęślą jaźń");
        assert_eq!(decode_bytes(utf8_bytes, None), "Zażółć gęślą jaźń");
    }
}
