//! The parameters of an API request.

use std::str::FromStr;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use super::envelope::ApiError;

/// The parameters of one API request: the fields of the query string,
/// followed by the fields of the form body when the request has one. A name
/// may occur more than once. The fields keep the order of the request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params {
    fields: Vec<(String, String)>,
}

impl Params {
    /// Parses a query string. `query` is the text after the `?` of the
    /// request URI.
    pub fn from_query(query: &str) -> Self {
        Self {
            fields: parse_urlencoded(query.as_bytes()).collect(),
        }
    }

    /// Appends the fields of an `application/x-www-form-urlencoded` body. The
    /// fields that are already present stay in front, so the query string
    /// supplies the first value of a name that both hold.
    pub fn append_form(&mut self, body: &[u8]) {
        self.fields.extend(parse_urlencoded(body));
    }

    /// The first value of `name`. `None` when no field has that name.
    pub fn opt_str(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    /// The first value of the required parameter `name`. An empty value is a
    /// value. The error has code 10 and names the parameter.
    pub fn req_str(&self, name: &str) -> Result<&str, ApiError> {
        self.opt_str(name)
            .ok_or_else(|| ApiError::MissingParam(name.to_owned()))
    }

    /// The first value of `name` as a signed integer. `None` when the
    /// parameter is absent or empty. A value that is not an integer is an
    /// error with code 0.
    pub fn opt_i64(&self, name: &str) -> Result<Option<i64>, ApiError> {
        self.opt_parsed(name, "an integer")
    }

    /// The first value of `name` as an unsigned 32-bit integer. `None` when
    /// the parameter is absent or empty. A value that is negative, too large
    /// or not an integer is an error with code 0.
    pub fn opt_u32(&self, name: &str) -> Result<Option<u32>, ApiError> {
        self.opt_parsed(name, "an integer from 0 to 4294967295")
    }

    /// The first value of `name` as a boolean. `true` and `false` are
    /// accepted in any letter case. `None` when the parameter is absent or
    /// empty. Any other value is an error with code 0.
    pub fn opt_bool(&self, name: &str) -> Result<Option<bool>, ApiError> {
        match self.opt_str(name) {
            None | Some("") => Ok(None),
            Some(value) if value.eq_ignore_ascii_case("true") => Ok(Some(true)),
            Some(value) if value.eq_ignore_ascii_case("false") => Ok(Some(false)),
            Some(_) => Err(invalid(name, "true or false")),
        }
    }

    /// Every value of `name`, in the order of the request: the values of the
    /// query string and then the values of the form body.
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// The first value of `name` parsed as `T`. `expected` describes `T` in
    /// the error message.
    fn opt_parsed<T: FromStr>(&self, name: &str, expected: &str) -> Result<Option<T>, ApiError> {
        match self.opt_str(name) {
            None | Some("") => Ok(None),
            Some(value) => value.parse().map(Some).map_err(|_| invalid(name, expected)),
        }
    }
}

/// The error for a value of `name` that is not `expected`. The message leaves
/// the value out.
fn invalid(name: &str, expected: &str) -> ApiError {
    ApiError::Other(format!("parameter {name} must be {expected}"))
}

/// A handler takes the parameters of its request as an argument of this type.
/// The router stores them in the request before it calls the handler.
impl<S: Send + Sync> FromRequestParts<S> for Params {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Self>()
            .cloned()
            .ok_or_else(|| ApiError::Other("the request has no parsed parameters".to_owned()))
    }
}

/// Parses `application/x-www-form-urlencoded` data by the rules of the URL
/// standard (<https://url.spec.whatwg.org/#urlencoded-parsing>). A query
/// string has the same form.
///
/// The fields are separated by `&`. An empty field is skipped. The first `=`
/// of a field separates the name from the value. A field without `=` has an
/// empty value.
fn parse_urlencoded(input: &[u8]) -> impl Iterator<Item = (String, String)> {
    input
        .split(|&byte| byte == b'&')
        .filter(|field| !field.is_empty())
        .map(|field| {
            let (name, value) = match field.iter().position(|&byte| byte == b'=') {
                Some(at) => (&field[..at], &field[at + 1..]),
                None => (field, &[][..]),
            };
            (decode(name), decode(value))
        })
}

/// Decodes one name or value. `+` is a space. `%` and two hexadecimal digits
/// are the byte with that value. A `%` that two hexadecimal digits do not
/// follow stays as it is. Bytes that are not valid UTF-8 become U+FFFD.
fn decode(input: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let mut rest = input;
    while let [first, tail @ ..] = rest {
        let escaped = match (first, tail) {
            (b'%', [high, low, ..]) => hex_digit(*high).zip(hex_digit(*low)),
            _ => None,
        };
        match escaped {
            Some((high, low)) => {
                bytes.push((high << 4) | low);
                rest = &tail[2..];
            }
            None => {
                bytes.push(if *first == b'+' { b' ' } else { *first });
                rest = tail;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The value of one hexadecimal digit.
fn hex_digit(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .and_then(|value| u8::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(input: &str) -> Vec<(String, String)> {
        parse_urlencoded(input.as_bytes()).collect()
    }

    fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
        expected
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// The message of the code 0 error that `result` holds.
    fn error_0<T: std::fmt::Debug>(result: Result<T, ApiError>) -> String {
        let error = result.expect_err("an invalid value");
        assert_eq!(error.code(), 0, "{error:?}");
        error.message()
    }

    #[test]
    fn query_string_splits_into_fields_in_order() {
        assert_eq!(
            fields("u=owner&v=1.16.1&c=app"),
            pairs(&[("u", "owner"), ("v", "1.16.1"), ("c", "app")])
        );
    }

    #[test]
    fn empty_input_has_no_fields() {
        assert_eq!(fields(""), pairs(&[]));
        assert_eq!(Params::from_query(""), Params::default());
    }

    #[test]
    fn empty_fields_are_skipped() {
        assert_eq!(fields("&a=1&&b=2&"), pairs(&[("a", "1"), ("b", "2")]));
    }

    #[test]
    fn field_without_equals_sign_has_an_empty_value() {
        assert_eq!(fields("a&b="), pairs(&[("a", ""), ("b", "")]));
    }

    #[test]
    fn only_the_first_equals_sign_separates_name_and_value() {
        assert_eq!(fields("a=b=c"), pairs(&[("a", "b=c")]));
        assert_eq!(fields("=b"), pairs(&[("", "b")]));
    }

    #[test]
    fn plus_is_a_space_and_percent_escapes_are_bytes() {
        assert_eq!(
            fields("query=Glass+Harbour%20%2B%26%3D"),
            pairs(&[("query", "Glass Harbour +&=")])
        );
        assert_eq!(fields("a%20b=1"), pairs(&[("a b", "1")]));
    }

    #[test]
    fn percent_escapes_accept_both_letter_cases() {
        assert_eq!(fields("a=%c3%a9%C3%A9"), pairs(&[("a", "éé")]));
    }

    #[test]
    fn percent_escapes_decode_to_utf8() {
        assert_eq!(
            fields("artist=Bj%C3%B6rk&title=%E6%97%A5%E6%9C%AC%E8%AA%9E"),
            pairs(&[("artist", "Björk"), ("title", "日本語")])
        );
    }

    #[test]
    fn unescaped_utf8_is_kept() {
        assert_eq!(fields("artist=Björk"), pairs(&[("artist", "Björk")]));
    }

    #[test]
    fn incomplete_percent_escape_stays_as_it_is() {
        assert_eq!(
            fields("a=100%&b=%4&c=%zz&d=%4g%41"),
            pairs(&[("a", "100%"), ("b", "%4"), ("c", "%zz"), ("d", "%4gA")])
        );
    }

    #[test]
    fn bytes_that_are_not_utf8_become_the_replacement_character() {
        assert_eq!(fields("a=%ff"), pairs(&[("a", "\u{fffd}")]));
    }

    #[test]
    fn form_body_values_append_to_query_values() {
        let mut params = Params::from_query("id=1");
        params.append_form(b"id=2");
        assert_eq!(params.all("id"), ["1", "2"]);
    }

    #[test]
    fn query_string_supplies_the_first_value() {
        let mut params = Params::from_query("f=json&id=1");
        params.append_form(b"f=xml&id=2&id=3&name=Mix+1");
        assert_eq!(params.opt_str("f"), Some("json"));
        assert_eq!(params.all("f"), ["json", "xml"]);
        assert_eq!(params.all("id"), ["1", "2", "3"]);
        assert_eq!(params.opt_str("name"), Some("Mix 1"));
    }

    #[test]
    fn form_body_alone_supplies_every_value() {
        let mut params = Params::from_query("");
        params.append_form(b"u=owner&songId=tr-1&songId=tr-2");
        assert_eq!(params.opt_str("u"), Some("owner"));
        assert_eq!(params.all("songId"), ["tr-1", "tr-2"]);
    }

    #[test]
    fn all_is_empty_for_an_absent_name() {
        let params = Params::from_query("id=1");
        assert!(params.all("songId").is_empty());
    }

    #[test]
    fn names_are_case_sensitive() {
        let params = Params::from_query("ID=1");
        assert_eq!(params.opt_str("id"), None);
        assert_eq!(params.opt_str("ID"), Some("1"));
    }

    #[test]
    fn opt_str_is_the_first_value_or_none() {
        let params = Params::from_query("id=1&id=2&empty=");
        assert_eq!(params.opt_str("id"), Some("1"));
        assert_eq!(params.opt_str("empty"), Some(""));
        assert_eq!(params.opt_str("absent"), None);
    }

    #[test]
    fn req_str_is_the_first_value() {
        let params = Params::from_query("id=tr-1&id=tr-2&query=");
        assert_eq!(params.req_str("id"), Ok("tr-1"));
        assert_eq!(params.req_str("query"), Ok(""));
    }

    #[test]
    fn req_str_names_the_missing_parameter_with_code_10() {
        let params = Params::from_query("u=owner");
        let error = params.req_str("id").expect_err("id is missing");
        assert_eq!(error, ApiError::MissingParam("id".to_owned()));
        assert_eq!(error.code(), 10);
        assert_eq!(error.message(), "required parameter is missing: id");
    }

    #[test]
    fn opt_i64_parses_signed_integers() {
        let params = Params::from_query("a=0&b=-5&c=9223372036854775807&d=");
        assert_eq!(params.opt_i64("a"), Ok(Some(0)));
        assert_eq!(params.opt_i64("b"), Ok(Some(-5)));
        assert_eq!(params.opt_i64("c"), Ok(Some(i64::MAX)));
        assert_eq!(params.opt_i64("d"), Ok(None), "an empty value is absent");
        assert_eq!(params.opt_i64("absent"), Ok(None));
    }

    #[test]
    fn opt_i64_rejects_other_values_with_code_0() {
        let params = Params::from_query("a=abc&b=1.5&c=9223372036854775808&d=+1+");
        for name in ["a", "b", "c", "d"] {
            assert_eq!(
                error_0(params.opt_i64(name)),
                format!("parameter {name} must be an integer")
            );
        }
    }

    #[test]
    fn opt_u32_parses_unsigned_integers() {
        let params = Params::from_query("size=500&offset=0&max=4294967295&none=");
        assert_eq!(params.opt_u32("size"), Ok(Some(500)));
        assert_eq!(params.opt_u32("offset"), Ok(Some(0)));
        assert_eq!(params.opt_u32("max"), Ok(Some(u32::MAX)));
        assert_eq!(params.opt_u32("none"), Ok(None), "an empty value is absent");
        assert_eq!(params.opt_u32("absent"), Ok(None));
    }

    #[test]
    fn opt_u32_rejects_other_values_with_code_0() {
        let params = Params::from_query("a=-1&b=4294967296&c=ten");
        for name in ["a", "b", "c"] {
            assert_eq!(
                error_0(params.opt_u32(name)),
                format!("parameter {name} must be an integer from 0 to 4294967295")
            );
        }
    }

    #[test]
    fn opt_bool_parses_true_and_false_in_any_case() {
        let params = Params::from_query("a=true&b=false&c=True&d=FALSE&e=");
        assert_eq!(params.opt_bool("a"), Ok(Some(true)));
        assert_eq!(params.opt_bool("b"), Ok(Some(false)));
        assert_eq!(params.opt_bool("c"), Ok(Some(true)));
        assert_eq!(params.opt_bool("d"), Ok(Some(false)));
        assert_eq!(params.opt_bool("e"), Ok(None), "an empty value is absent");
        assert_eq!(params.opt_bool("absent"), Ok(None));
    }

    #[test]
    fn opt_bool_rejects_other_values_with_code_0() {
        let params = Params::from_query("a=1&b=yes&c=truee");
        for name in ["a", "b", "c"] {
            assert_eq!(
                error_0(params.opt_bool(name)),
                format!("parameter {name} must be true or false")
            );
        }
    }

    #[test]
    fn typed_getters_read_the_first_value() {
        let mut params = Params::from_query("size=10&submission=false");
        params.append_form(b"size=20&submission=true");
        assert_eq!(params.opt_u32("size"), Ok(Some(10)));
        assert_eq!(params.opt_i64("size"), Ok(Some(10)));
        assert_eq!(params.opt_bool("submission"), Ok(Some(false)));
    }

    #[test]
    fn error_message_leaves_the_value_out() {
        let params = Params::from_query("size=secret");
        assert!(!error_0(params.opt_u32("size")).contains("secret"));
    }
}
