//! Item IDs, natural keys, the normalised form of a name and file
//! fingerprints (`docs/design.md` section 3.3).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// The kinds of item that have a prefixed surrogate ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Track,
    Album,
    Artist,
    Playlist,
}

impl Kind {
    /// The two letters that start every ID of this kind.
    pub const fn prefix(self) -> &'static str {
        match self {
            Kind::Track => "tr",
            Kind::Album => "al",
            Kind::Artist => "ar",
            Kind::Playlist => "pl",
        }
    }

    fn from_prefix(prefix: &str) -> Option<Kind> {
        match prefix {
            "tr" => Some(Kind::Track),
            "al" => Some(Kind::Album),
            "ar" => Some(Kind::Artist),
            "pl" => Some(Kind::Playlist),
            _ => None,
        }
    }
}

/// The number of random bytes in an ID.
const ID_BYTES: usize = 16;

/// Returns a new ID of the given kind: the prefix, a hyphen and 32 lower-case
/// hex characters that encode 16 random bytes.
pub fn new_id(kind: Kind) -> String {
    let mut bytes = [0u8; ID_BYTES];
    rand::fill(&mut bytes);
    format!("{}-{}", kind.prefix(), hex::encode(bytes))
}

/// Splits an ID into its kind and its 32 hex characters.
///
/// Returns `None` unless the input is exactly a known prefix, one hyphen and
/// 32 characters from `0-9a-f`.
pub fn parse(id: &str) -> Option<(Kind, &str)> {
    let (prefix, hex) = id.split_once('-')?;
    let kind = Kind::from_prefix(prefix)?;
    let is_hex = hex.len() == 2 * ID_BYTES
        && hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    is_hex.then_some((kind, hex))
}

/// Returns the normalised form of `text`: the text in Unicode form NFC, and
/// then in lower case (`docs/design.md` section 3.3).
///
/// The server compares names in this form. The search brings the terms of a
/// query into it (`db::queries::search::Terms::parse`), and the scanner
/// stores the title of a track in it in `tracks.title_lc`
/// (`db::queries::tracks`). Both call this function, so a term and a stored
/// title are in one form.
///
/// - NFC composes a base letter and its combining marks into one character
///   where Unicode has that character: `e` followed by U+0301 becomes
///   U+00E9. A text in decomposed form (NFD) and the same text in composed
///   form have one normalised form.
/// - Lower case is the lower case of `str::to_lowercase`: the default case
///   mapping of Unicode for every character, with no rule of a language. `I`
///   becomes `i` in every text, and U+03A3 becomes U+03C2 at the end of a
///   word and U+03C3 elsewhere. An ASCII letter becomes its ASCII lower-case
///   letter.
/// - No diacritic is removed: U+00E9 and `e` stay different letters.
/// - The white space of the text stays. A natural key is the same form
///   without the white space at its ends.
///
/// The lower case of a composed text is taken as it is and is not composed
/// again. A capital letter with a combining mark that Unicode has as one
/// character in lower case only, such as `J` followed by U+030C, therefore
/// has another normalised form than that lower-case character, U+01F0.
pub fn normalise(text: &str) -> String {
    let composed: String = text.nfc().collect();
    composed.to_lowercase()
}

/// Returns the natural key of an artist name: Unicode NFC, trimmed,
/// lower-cased. It is the normalised form ([`normalise`]) of the trimmed
/// name.
pub fn artist_key(name: &str) -> String {
    natural_key(name)
}

/// Returns the natural key of an album title: Unicode NFC, trimmed,
/// lower-cased. It is the normalised form ([`normalise`]) of the trimmed
/// title.
pub fn album_key(title: &str) -> String {
    natural_key(title)
}

// The key of a row decides its identity, so this function keeps its own
// statements. The test `a_key_is_the_normalised_form_of_the_trimmed_name`
// fails when its result and the result of `normalise` differ.
fn natural_key(text: &str) -> String {
    let composed: String = text.nfc().collect();
    composed.trim().to_lowercase()
}

/// The number of bytes hashed from each end of a file.
const WINDOW: usize = 64 * 1024;

/// Returns the fingerprint of the file at `path`.
///
/// The fingerprint is the file size in bytes, a colon and the first 16 bytes,
/// in hex, of the SHA-256 of the first 64 KiB followed by the last 64 KiB. A
/// file of 128 KiB or less is hashed whole.
pub fn fingerprint(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut hasher = Sha256::new();
    if size <= 2 * WINDOW as u64 {
        // `size` is at most 128 KiB here, so the cast keeps its value.
        let mut whole = vec![0u8; size as usize];
        file.read_exact(&mut whole)?;
        hasher.update(&whole);
    } else {
        let mut window = vec![0u8; WINDOW];
        file.read_exact(&mut window)?;
        hasher.update(&window);
        file.seek(SeekFrom::Start(size - WINDOW as u64))?;
        file.read_exact(&mut window)?;
        hasher.update(&window);
    }
    let digest = hasher.finalize();
    Ok(format!("{size}:{}", hex::encode(&digest[..16])))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    const KIB: usize = 1024;
    const HEX_32: &str = "0123456789abcdef0123456789abcdef";

    /// Returns `len` bytes from a fixed xorshift64 sequence.
    fn pseudo_random_bytes(len: usize) -> Vec<u8> {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut bytes = Vec::with_capacity(len + 8);
        while bytes.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(len);
        bytes
    }

    fn write_file(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    /// Returns the fingerprint of a file that holds `bytes` with the byte at
    /// `offset` inverted.
    fn fingerprint_with_flipped_byte(dir: &TempDir, bytes: &[u8], offset: usize) -> String {
        let mut changed = bytes.to_vec();
        changed[offset] ^= 0xff;
        fingerprint(&write_file(dir, "changed.bin", &changed)).unwrap()
    }

    /// Builds the fingerprint of `hashed` for a file of `size` bytes.
    fn expected_fingerprint(size: usize, hashed: &[u8]) -> String {
        format!("{size}:{}", hex::encode(&Sha256::digest(hashed)[..16]))
    }

    #[test]
    fn new_id_has_prefix_hyphen_and_32_lower_case_hex() {
        for (kind, prefix) in [
            (Kind::Track, "tr-"),
            (Kind::Album, "al-"),
            (Kind::Artist, "ar-"),
            (Kind::Playlist, "pl-"),
        ] {
            let id = new_id(kind);
            let hex = id.strip_prefix(prefix).unwrap();
            assert_eq!(hex.len(), 32, "{id}");
            assert!(
                hex.bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
                "{id}"
            );
        }
    }

    #[test]
    fn new_id_differs_between_calls() {
        assert_ne!(new_id(Kind::Track), new_id(Kind::Track));
    }

    #[test]
    fn parse_accepts_an_id_of_each_kind() {
        for kind in [Kind::Track, Kind::Album, Kind::Artist, Kind::Playlist] {
            let id = new_id(kind);
            assert_eq!(parse(&id), Some((kind, &id[3..])));
        }
    }

    #[test]
    fn parse_accepts_tr_with_32_hex() {
        let id = format!("tr-{HEX_32}");
        assert_eq!(parse(&id), Some((Kind::Track, HEX_32)));
    }

    #[test]
    fn parse_rejects_other_shapes() {
        let rejected = [
            "al-ABC".to_string(),
            format!("al-{}", &HEX_32[..31]),
            format!("al-{HEX_32}/x"),
            "../x".to_string(),
            String::new(),
            "al".to_string(),
            "al-".to_string(),
            HEX_32.to_string(),
            format!("al{HEX_32}"),
            format!("al-{HEX_32}0"),
            format!("al-{}", HEX_32.to_uppercase()),
            format!("al-{}g", &HEX_32[..31]),
            format!("al-{}é", &HEX_32[..30]),
            format!("xx-{HEX_32}"),
            format!("AL-{HEX_32}"),
            format!("al--{}", &HEX_32[..31]),
            format!(" al-{HEX_32}"),
            format!("al-{HEX_32}\n"),
            format!("../al-{HEX_32}"),
        ];
        for id in rejected {
            assert_eq!(parse(&id), None, "{id:?}");
        }
    }

    #[test]
    fn keys_ignore_surrounding_space_and_case() {
        assert_eq!(artist_key("  The Beatles "), artist_key("the beatles"));
        assert_eq!(artist_key("  The Beatles "), "the beatles");
        assert_eq!(album_key("\tAbbey Road\n"), album_key("ABBEY ROAD"));
        assert_eq!(album_key("\tAbbey Road\n"), "abbey road");
    }

    #[test]
    fn keys_are_equal_for_nfd_and_nfc_input() {
        let nfc = "Beyonc\u{e9}";
        let nfd = "Beyonce\u{301}";
        assert_ne!(nfc, nfd);
        assert_eq!(artist_key(nfd), artist_key(nfc));
        assert_eq!(artist_key(nfd), "beyonc\u{e9}");
        assert_eq!(album_key(nfd), album_key(nfc));
        assert_eq!(album_key("A\u{30a}NGSTRO\u{308}M"), "\u{e5}ngstr\u{f6}m");
    }

    #[test]
    fn normalise_composes_a_text_and_lower_cases_it() {
        // A text without a letter outside ASCII: ASCII lower case.
        assert_eq!(normalise("Low Tide 7\" (Remix)"), "low tide 7\" (remix)");
        assert_eq!(normalise("already lower"), "already lower");
        assert_eq!(normalise(""), "");
        // The white space of the text stays.
        assert_eq!(normalise("  A\tB \u{3000}"), "  a\tb \u{3000}");

        // The decomposed form and the composed form have one normalised
        // form, in either letter case.
        let composed = "caf\u{e9}";
        for spelling in ["Cafe\u{301}", "CAFE\u{301}", "Caf\u{e9}", "CAF\u{c9}"] {
            assert_eq!(normalise(spelling), composed, "{spelling:?}");
        }
        // Two marks on one letter in the order that is not the canonical
        // one: U+0323 is below the letter and U+0302 is above it.
        assert_eq!(normalise("A\u{302}\u{323}"), "\u{1ead}");
        assert_eq!(normalise("A\u{323}\u{302}"), "\u{1ead}");
        // Hangul syllables are composed from their jamo.
        assert_eq!(normalise("\u{1112}\u{1161}\u{11ab}"), "\u{d55c}");

        // Lower case is the default mapping of Unicode for letters outside
        // ASCII, without a rule of a language.
        assert_eq!(normalise("\u{c9}DITH"), "\u{e9}dith");
        assert_eq!(
            normalise("\u{41a}\u{438}\u{43d}\u{43e}"),
            "\u{43a}\u{438}\u{43d}\u{43e}"
        );
        assert_eq!(normalise("STRASSE"), "strasse");
        assert_eq!(normalise("Stra\u{df}e"), "stra\u{df}e");
        assert_eq!(normalise("I"), "i");
        assert_eq!(normalise("\u{130}"), "i\u{307}");
        // U+03A3 at the end of a word and inside a word.
        assert_eq!(
            normalise("\u{39f}\u{394}\u{39f}\u{3a3} \u{3a3}\u{391}"),
            "\u{3bf}\u{3b4}\u{3bf}\u{3c2} \u{3c3}\u{3b1}"
        );

        // A letter with a diacritic and its base letter stay different, and
        // so do a ligature and its letters: nothing is folded.
        assert_ne!(normalise("\u{e9}"), normalise("e"));
        assert_ne!(normalise("\u{fb01}"), normalise("fi"));
        assert_ne!(normalise("\u{ff21}"), normalise("A"));
    }

    #[test]
    fn normalise_does_not_compose_the_lower_case_of_a_text_again() {
        // `J` and U+030C have no composed form, and `j` and U+030C have one.
        let capital = "J\u{30c}";
        let small: String = "j\u{30c}".nfc().collect();
        assert_eq!(small, "\u{1f0}");
        assert_eq!(normalise(capital), "j\u{30c}");
        assert_eq!(normalise(&small), "\u{1f0}");
        assert_ne!(normalise(capital), normalise(&small));
    }

    #[test]
    fn a_key_is_the_normalised_form_of_the_trimmed_name() {
        let names = [
            "Aurora Field",
            "  The Beatles ",
            "\tAbbey Road\n",
            "Beyonce\u{301}",
            "BEYONC\u{c9}",
            "A\u{30a}NGSTRO\u{308}M",
            "\u{3000}\u{5742}\u{672c}\u{9f8d}\u{4e00}\u{a0}",
            "\u{2000}En Quad\u{2001}",
            "\u{39f}\u{394}\u{39f}\u{3a3} ",
            " \u{3a3}\u{391}",
            "\u{130}stanbul",
            "J\u{30c}",
            "e\u{301} ",
            " \u{301}e",
            "",
            "   ",
        ];
        for name in names {
            assert_eq!(artist_key(name), normalise(name.trim()), "{name:?}");
            assert_eq!(artist_key(name), normalise(name).trim(), "{name:?}");
            assert_eq!(album_key(name), artist_key(name), "{name:?}");
        }
    }

    #[test]
    fn fingerprint_of_a_copy_equals_the_original() {
        let dir = TempDir::new().unwrap();
        let original = write_file(&dir, "original.bin", &pseudo_random_bytes(300 * KIB));
        let copy = dir.path().join("copy.bin");
        fs::copy(&original, &copy).unwrap();
        assert_eq!(fingerprint(&copy).unwrap(), fingerprint(&original).unwrap());
    }

    #[test]
    fn fingerprint_changes_when_one_byte_is_appended() {
        let dir = TempDir::new().unwrap();
        for len in [100, 300 * KIB] {
            let mut bytes = pseudo_random_bytes(len);
            let before = fingerprint(&write_file(&dir, "a.bin", &bytes)).unwrap();
            bytes.push(0);
            let after = fingerprint(&write_file(&dir, "a.bin", &bytes)).unwrap();
            assert_ne!(before, after, "{len} bytes");
        }
    }

    #[test]
    fn fingerprint_of_a_100_byte_file_hashes_the_whole_file() {
        let dir = TempDir::new().unwrap();
        let bytes = pseudo_random_bytes(100);
        let path = write_file(&dir, "small.bin", &bytes);
        assert_eq!(
            fingerprint(&path).unwrap(),
            expected_fingerprint(100, &bytes)
        );
    }

    #[test]
    fn fingerprint_of_an_empty_file_is_the_sha256_of_no_bytes() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "empty.bin", &[]);
        assert_eq!(
            fingerprint(&path).unwrap(),
            "0:e3b0c44298fc1c149afbf4c8996fb924"
        );
    }

    #[test]
    fn fingerprint_of_a_300_kib_file_hashes_the_first_and_last_64_kib() {
        let dir = TempDir::new().unwrap();
        let bytes = pseudo_random_bytes(300 * KIB);
        let original = fingerprint(&write_file(&dir, "original.bin", &bytes)).unwrap();

        let mut hashed = bytes[..64 * KIB].to_vec();
        hashed.extend_from_slice(&bytes[bytes.len() - 64 * KIB..]);
        assert_eq!(original, expected_fingerprint(300 * KIB, &hashed));

        let last = bytes.len() - 1;
        let unchanged = [150 * KIB, 64 * KIB, last - 64 * KIB];
        for offset in unchanged {
            assert_eq!(
                fingerprint_with_flipped_byte(&dir, &bytes, offset),
                original,
                "offset {offset}"
            );
        }
        let changed = [0, 64 * KIB - 1, last - 64 * KIB + 1, last];
        for offset in changed {
            assert_ne!(
                fingerprint_with_flipped_byte(&dir, &bytes, offset),
                original,
                "offset {offset}"
            );
        }
    }

    #[test]
    fn fingerprint_differs_between_128_kib_plus_one_and_128_kib() {
        let dir = TempDir::new().unwrap();
        let longer = pseudo_random_bytes(128 * KIB + 1);
        let shorter = &longer[..128 * KIB];
        let longer_print = fingerprint(&write_file(&dir, "longer.bin", &longer)).unwrap();
        let shorter_print = fingerprint(&write_file(&dir, "shorter.bin", shorter)).unwrap();
        assert_ne!(longer_print, shorter_print);

        // A file of exactly 128 KiB is hashed whole.
        assert_eq!(shorter_print, expected_fingerprint(128 * KIB, shorter));
        // One byte more leaves the byte between the two windows out of the hash.
        assert_eq!(
            fingerprint_with_flipped_byte(&dir, &longer, 64 * KIB),
            longer_print
        );
    }

    #[test]
    fn fingerprint_of_a_missing_file_is_an_error() {
        let dir = TempDir::new().unwrap();
        let error = fingerprint(&dir.path().join("missing.bin")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
