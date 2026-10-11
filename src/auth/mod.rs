//! Credential verification and password generation (`docs/design.md`
//! sections 1.3 and 3.7).
//!
//! The password of a user is stored as plain text, because the Subsonic
//! token check needs the password itself. No function of this module logs a
//! password, a token or a salt, or puts one in an error or a panic message.

use std::hint::black_box;

use md5::{Digest, Md5};
use rand::TryRng;
use rand::rngs::SysRng;

/// The characters of a generated password: the lower-case letters without
/// `i`, `l` and `o`, and the digits without `0` and `1`.
pub const PASSWORD_ALPHABET: &str = "abcdefghjkmnpqrstuvwxyz23456789";

/// The number of character groups in a generated password.
const PASSWORD_GROUPS: usize = 4;

/// The number of characters in one group of a generated password.
const PASSWORD_GROUP_LEN: usize = 5;

/// The prefix of a password that a client sends hex-encoded.
const ENC_PREFIX: &str = "enc:";

/// Returns whether `given`, the value of the `p` parameter, is the stored
/// password.
///
/// A value that starts with `enc:` is the hex encoding of the password. The
/// rest of the value is hex-decoded and the result is compared. A value with
/// that prefix and invalid hex matches nothing. Any other value is compared
/// as it is.
///
/// The comparison does not stop at the first byte that differs.
pub fn verify_password(stored: &str, given: &str) -> bool {
    match given.strip_prefix(ENC_PREFIX) {
        Some(encoded) => match hex::decode(encoded) {
            Ok(decoded) => constant_time_eq(stored.as_bytes(), &decoded),
            Err(_) => false,
        },
        None => constant_time_eq(stored.as_bytes(), given.as_bytes()),
    }
}

/// Returns whether `token` is the Subsonic token of the stored password for
/// `salt`: the MD5 digest of the password followed by the salt, as 32
/// lower-case hex characters.
///
/// A token in upper-case hex does not match. The comparison does not stop at
/// the first byte that differs.
pub fn verify_token(stored: &str, salt: &str, token: &str) -> bool {
    let mut hasher = Md5::new();
    hasher.update(stored.as_bytes());
    hasher.update(salt.as_bytes());
    let expected = hex::encode(hasher.finalize());
    constant_time_eq(expected.as_bytes(), token.as_bytes())
}

/// Returns a new password: four groups of five characters of
/// [`PASSWORD_ALPHABET`], separated by hyphens. Each character is drawn from
/// the random generator of the operating system, and every character of the
/// alphabet is equally likely.
///
/// # Panics
///
/// Panics when the operating system cannot supply random bytes.
pub fn generate_password() -> String {
    let wanted = PASSWORD_GROUPS * PASSWORD_GROUP_LEN;
    let mut password = String::with_capacity(wanted + PASSWORD_GROUPS - 1);
    let mut drawn = 0;
    while drawn < wanted {
        // A batch holds more bytes than the password has characters, so one
        // batch is enough unless many of its bytes are discarded.
        let mut bytes = [0u8; 32];
        SysRng
            .try_fill_bytes(&mut bytes)
            .expect("the operating system supplies random bytes");
        let characters = bytes.into_iter().filter_map(alphabet_char);
        for character in characters.take(wanted - drawn) {
            if drawn > 0 && drawn % PASSWORD_GROUP_LEN == 0 {
                password.push('-');
            }
            password.push(character);
            drawn += 1;
        }
    }
    password
}

/// Maps a random byte to a character of [`PASSWORD_ALPHABET`], or to `None`
/// for a byte that is discarded.
///
/// The alphabet has 31 characters. The byte values 0 to 247 map to a
/// character, 8 values for each one. The values 248 to 255 are discarded,
/// because mapping them would make 8 characters more likely than the others.
fn alphabet_char(byte: u8) -> Option<char> {
    let alphabet = PASSWORD_ALPHABET.as_bytes();
    let values = usize::from(u8::MAX) + 1;
    let accepted = values - values % alphabet.len();
    let index = usize::from(byte);
    (index < accepted).then(|| char::from(alphabet[index % alphabet.len()]))
}

/// Returns whether `given` equals `secret`.
///
/// The loop reads every byte of `given` and does not stop at the first byte
/// that differs, so the time of a call does not depend on the position of a
/// wrong byte or on the value of any byte. The time can depend on the two
/// lengths: the loop runs once for each byte of `given`, and the read of
/// `secret` takes another branch past its end. A caller learns at most the
/// length of `secret` from the time. `black_box` asks the compiler not to
/// replace the loop with a comparison that stops early.
fn constant_time_eq(secret: &[u8], given: &[u8]) -> bool {
    let mut difference = u8::from(secret.len() != given.len());
    for (index, byte) in given.iter().enumerate() {
        // A position past the end of the secret is compared with 0. The
        // length difference has already made the result false.
        let expected = secret.get(index).copied().unwrap_or(0);
        difference |= black_box(expected ^ byte);
    }
    black_box(difference) == 0
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    /// The example of the Subsonic API documentation: the token of the
    /// password `sesame` for the salt `c19b2d`.
    const SESAME_TOKEN: &str = "26719a1196d2a940705a59634eb18eab";

    #[test]
    fn verify_token_accepts_the_md5_of_password_and_salt() {
        assert!(verify_token("sesame", "c19b2d", SESAME_TOKEN));
    }

    #[test]
    fn verify_token_rejects_upper_case_hex() {
        let upper = SESAME_TOKEN.to_uppercase();
        assert_ne!(upper, SESAME_TOKEN);
        assert!(!verify_token("sesame", "c19b2d", &upper));
    }

    #[test]
    fn verify_token_rejects_another_salt_and_another_password() {
        assert!(!verify_token("sesame", "c19b2e", SESAME_TOKEN));
        assert!(!verify_token("sesame", "", SESAME_TOKEN));
        assert!(!verify_token("Sesame", "c19b2d", SESAME_TOKEN));
    }

    #[test]
    fn verify_token_rejects_a_token_of_another_length() {
        assert!(!verify_token("sesame", "c19b2d", ""));
        assert!(!verify_token("sesame", "c19b2d", &SESAME_TOKEN[..31]));
        assert!(!verify_token(
            "sesame",
            "c19b2d",
            &format!("{SESAME_TOKEN}0")
        ));
    }

    #[test]
    fn verify_token_does_not_accept_the_password_as_the_token() {
        assert!(!verify_token("sesame", "c19b2d", "sesame"));
    }

    #[test]
    fn verify_password_accepts_the_stored_password() {
        assert!(verify_password("sesame", "sesame"));
        assert!(verify_password("pässword ☃", "pässword ☃"));
    }

    #[test]
    fn verify_password_rejects_any_other_value() {
        for given in ["", "sesam", "sesame ", "sesamee", "Sesame", "sesamf"] {
            assert!(!verify_password("sesame", given), "{given:?}");
        }
    }

    #[test]
    fn verify_password_accepts_enc_hex_input() {
        // The hex encoding of `sesame`.
        assert!(verify_password("sesame", "enc:736573616d65"));
        assert!(verify_password("sesame", "enc:736573616D65"));
    }

    #[test]
    fn verify_password_rejects_enc_input_of_another_password() {
        // The hex encoding of `sesamf`.
        assert!(!verify_password("sesame", "enc:736573616d66"));
        assert!(!verify_password("sesame", "enc:"));
        // Bytes that are not UTF-8.
        assert!(!verify_password("sesame", "enc:ff"));
    }

    #[test]
    fn verify_password_rejects_enc_input_that_is_not_hex() {
        assert!(!verify_password("sesame", "enc:sesame"));
        // An odd number of hex digits.
        assert!(!verify_password("sesame", "enc:736573616d6"));
        // The input is not compared as it is when the hex is invalid.
        assert!(!verify_password("enc:zz", "enc:zz"));
    }

    #[test]
    fn verify_password_decodes_only_the_enc_prefix_in_lower_case() {
        assert!(!verify_password("sesame", "ENC:736573616d65"));
        assert!(verify_password("ENC:736573616d65", "ENC:736573616d65"));
    }

    #[test]
    fn constant_time_eq_is_byte_equality() {
        let cases: [(&[u8], &[u8], bool); 8] = [
            (b"", b"", true),
            (b"a", b"a", true),
            (b"abc", b"abc", true),
            (b"abc", b"abd", false),
            (b"abc", b"xbc", false),
            (b"abc", b"ab", false),
            (b"abc", b"abcd", false),
            // A zero byte past the end of the secret does not match.
            (b"abc", b"abc\0", false),
        ];
        for (secret, given, expected) in cases {
            assert_eq!(
                constant_time_eq(secret, given),
                expected,
                "{secret:?} {given:?}"
            );
        }
    }

    #[test]
    fn password_alphabet_is_31_distinct_characters_without_look_alikes() {
        assert_eq!(PASSWORD_ALPHABET.len(), 31);
        let distinct: HashSet<char> = PASSWORD_ALPHABET.chars().collect();
        assert_eq!(distinct.len(), 31);
        for character in PASSWORD_ALPHABET.chars() {
            assert!(matches!(character, 'a'..='z' | '2'..='9'), "{character:?}");
        }
        for excluded in ['i', 'l', 'o', '0', '1'] {
            assert!(!PASSWORD_ALPHABET.contains(excluded), "{excluded:?}");
        }
    }

    #[test]
    fn alphabet_char_maps_eight_byte_values_to_each_character() {
        let mut counts: HashMap<char, usize> = HashMap::new();
        let mut discarded = 0;
        for byte in u8::MIN..=u8::MAX {
            match alphabet_char(byte) {
                Some(character) => *counts.entry(character).or_default() += 1,
                None => discarded += 1,
            }
        }
        assert_eq!(counts.len(), 31);
        for character in PASSWORD_ALPHABET.chars() {
            assert_eq!(counts[&character], 8, "{character:?}");
        }
        assert_eq!(discarded, 8);
    }

    /// Returns whether `password` is four groups of five characters from
    /// `a-z` and `2-9`, separated by hyphens.
    fn has_generated_shape(password: &str) -> bool {
        let groups: Vec<&str> = password.split('-').collect();
        groups.len() == 4
            && groups.iter().all(|group| {
                group.len() == 5
                    && group
                        .chars()
                        .all(|character| matches!(character, 'a'..='z' | '2'..='9'))
            })
    }

    #[test]
    fn generated_shape_check_rejects_other_shapes() {
        assert!(has_generated_shape("abcde-fghjk-mnpqr-23456"));
        for other in [
            "",
            "abcde-fghjk-mnpqr",
            "abcde-fghjk-mnpqr-23456-789ab",
            "abcde-fghjk-mnpqr-2345",
            "abcde-fghjk-mnpqr-234567",
            "abcde-fghjk-mnpqr-2345A",
            "abcde-fghjk-mnpqr-2345_",
            "abcdefghjkmnpqr23456",
        ] {
            assert!(!has_generated_shape(other), "{other:?}");
        }
    }

    #[test]
    fn generated_passwords_have_the_shape_and_the_alphabet() {
        for _ in 0..1000 {
            let password = generate_password();
            assert_eq!(password.len(), 23);
            assert!(has_generated_shape(&password));
            for character in password.chars().filter(|character| *character != '-') {
                assert!(PASSWORD_ALPHABET.contains(character));
            }
            for excluded in ['i', 'l', 'o', '0', '1'] {
                assert!(!password.contains(excluded));
            }
        }
    }

    #[test]
    fn a_thousand_generated_passwords_are_distinct() {
        let passwords: HashSet<String> = (0..1000).map(|_| generate_password()).collect();
        assert_eq!(passwords.len(), 1000);
    }

    #[test]
    fn a_generated_password_verifies_as_itself() {
        let password = generate_password();
        assert!(verify_password(&password, &password));
        assert!(verify_password(
            &password,
            &format!("enc:{}", hex::encode(&password))
        ));
    }
}
