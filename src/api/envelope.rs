//! The response envelope that every API method shares, in JSON and XML, and
//! the error type of the API.

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::xml;

/// The Subsonic protocol version the server reports.
const PROTOCOL_VERSION: &str = "1.16.1";

/// The server name in the `type` field.
const SERVER_TYPE: &str = "wn-server";

const JSON_CONTENT_TYPE: &str = "application/json";
const XML_CONTENT_TYPE: &str = "application/xml";

/// The response format. A client selects it with the `f` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Xml,
}

impl Format {
    /// Parses the value of the `f` parameter. `None` is an absent parameter
    /// and selects XML.
    ///
    /// Every value other than `json` and `xml` is an error with code 0.
    /// `jsonp` is such a value. The caller renders that error in XML.
    pub fn parse(f: Option<&str>) -> Result<Self, ApiError> {
        match f {
            None | Some("xml") => Ok(Self::Xml),
            Some("json") => Ok(Self::Json),
            Some(_) => Err(ApiError::Other("unsupported format".to_owned())),
        }
    }
}

/// An error that ends a request. Each variant has a Subsonic error code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// A required parameter is absent. The value is the parameter name.
    /// Code 10.
    #[error("required parameter is missing: {0}")]
    MissingParam(String),
    /// The user is unknown or the credential is wrong. Code 40.
    #[error("wrong username or password")]
    BadCredentials,
    /// The request uses an authentication mechanism that the server does not
    /// support. Code 42.
    #[error("authentication mechanism not supported")]
    AuthUnsupported,
    /// The request combines authentication mechanisms that exclude each
    /// other. Code 43.
    #[error("conflicting authentication mechanisms")]
    AuthConflict,
    /// The user may not perform the operation. The value is the message.
    /// Code 50.
    #[error("{0}")]
    Forbidden(String),
    /// The requested item does not exist. Code 70.
    #[error("not found")]
    NotFound,
    /// Any other failure. The value is the message. Code 0.
    #[error("{0}")]
    Other(String),
}

impl ApiError {
    /// The Subsonic error code.
    pub fn code(&self) -> u32 {
        match self {
            Self::Other(_) => 0,
            Self::MissingParam(_) => 10,
            Self::BadCredentials => 40,
            Self::AuthUnsupported => 42,
            Self::AuthConflict => 43,
            Self::Forbidden(_) => 50,
            Self::NotFound => 70,
        }
    }

    /// The message of the error element.
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// The value of the `status` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Failed,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

/// One API response: the envelope fields and at most one payload entry.
#[derive(Debug, Clone, PartialEq)]
pub struct SubsonicResponse {
    status: Status,
    /// A JSON object. It is empty or it holds the one entry of the response.
    payload: Value,
}

impl SubsonicResponse {
    /// A successful response that carries `value` under `key`.
    pub fn ok(key: &str, value: Value) -> Self {
        Self {
            status: Status::Ok,
            payload: Value::Object(Map::from_iter([(key.to_owned(), value)])),
        }
    }

    /// A successful response without a payload.
    pub fn empty() -> Self {
        Self {
            status: Status::Ok,
            payload: Value::Object(Map::new()),
        }
    }

    /// A failed response that carries the Subsonic error `code` and `message`.
    pub fn error(code: u32, message: &str) -> Self {
        Self {
            status: Status::Failed,
            payload: json!({ "error": { "code": code, "message": message } }),
        }
    }

    /// Renders the response in `format`. Returns the content type and the
    /// body.
    pub fn render(&self, format: Format) -> (&'static str, String) {
        match format {
            Format::Json => (JSON_CONTENT_TYPE, self.json_body()),
            Format::Xml => (XML_CONTENT_TYPE, self.xml_body()),
        }
    }

    fn json_body(&self) -> String {
        let document = JsonDocument {
            response: JsonEnvelope {
                status: self.status.as_str(),
                version: PROTOCOL_VERSION,
                r#type: SERVER_TYPE,
                server_version: crate::VERSION,
                open_subsonic: true,
                payload: &self.payload,
            },
        };
        serde_json::to_string(&document)
            .expect("strings, booleans and JSON values serialise to JSON")
    }

    fn xml_body(&self) -> String {
        let root_attrs = [
            ("status", self.status.as_str()),
            ("version", PROTOCOL_VERSION),
            ("type", SERVER_TYPE),
            ("serverVersion", crate::VERSION),
            ("openSubsonic", "true"),
        ];
        xml::to_xml(&root_attrs, &self.payload)
    }
}

/// The JSON document of a response.
#[derive(Serialize)]
struct JsonDocument<'a> {
    #[serde(rename = "subsonic-response")]
    response: JsonEnvelope<'a>,
}

/// The envelope fields in the order the JSON body holds them, followed by the
/// payload entry.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonEnvelope<'a> {
    status: &'a str,
    version: &'a str,
    r#type: &'a str,
    server_version: &'a str,
    open_subsonic: bool,
    /// Always a JSON object. Its entries become fields of the envelope.
    #[serde(flatten)]
    payload: &'a Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VERSION;

    /// The JSON body that has `status` and then `rest`. `rest` is the text
    /// that follows the envelope fields inside the response object.
    fn json_document(status: &str, rest: &str) -> String {
        format!(
            concat!(
                r#"{{"subsonic-response":{{"status":"{}","version":"1.16.1","type":"wn-server","#,
                r#""serverVersion":"{}","openSubsonic":true{}}}}}"#
            ),
            status, VERSION, rest
        )
    }

    /// The XML body that has `status` and then `rest`. `rest` is the text
    /// that follows the last attribute of the root element.
    fn xml_document(status: &str, rest: &str) -> String {
        format!(
            concat!(
                r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="{}""#,
                r#" version="1.16.1" type="wn-server" serverVersion="{}" openSubsonic="true"{}"#
            ),
            status, VERSION, rest
        )
    }

    fn body(response: &SubsonicResponse, format: Format) -> String {
        response.render(format).1
    }

    /// A model that follows the conventions in `api::model`.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Album {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cover_art: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        song_count: Option<u32>,
        replay_gain: Gain,
        song: Vec<Song>,
        genres: Vec<Genre>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Gain {
        album_peak: f64,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Song {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        disc_number: Option<u32>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Genre {
        value: String,
    }

    fn album() -> Value {
        let album = Album {
            id: "al-1".to_owned(),
            cover_art: None,
            song_count: Some(2),
            replay_gain: Gain { album_peak: 0.5 },
            song: vec![
                Song {
                    id: "tr-1".to_owned(),
                    disc_number: Some(1),
                },
                Song {
                    id: "tr-2".to_owned(),
                    disc_number: None,
                },
            ],
            genres: vec![
                Genre {
                    value: "Rock".to_owned(),
                },
                Genre {
                    value: "Jazz".to_owned(),
                },
            ],
        };
        serde_json::to_value(album).expect("the model serialises")
    }

    #[test]
    fn ok_renders_the_documented_json() {
        let response = SubsonicResponse::ok("license", json!({ "valid": true }));
        assert_eq!(
            body(&response, Format::Json),
            format!(
                concat!(
                    r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","#,
                    r#""type":"wn-server","serverVersion":"{}","openSubsonic":true,"#,
                    r#""license":{{"valid":true}}}}}}"#
                ),
                VERSION
            )
        );
    }

    #[test]
    fn ok_renders_the_documented_xml() {
        let response = SubsonicResponse::ok("license", json!({ "valid": true }));
        assert_eq!(
            body(&response, Format::Xml),
            format!(
                concat!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                    r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok""#,
                    r#" version="1.16.1" type="wn-server" serverVersion="{}""#,
                    r#" openSubsonic="true">"#,
                    r#"<license valid="true"/>"#,
                    r#"</subsonic-response>"#
                ),
                VERSION
            )
        );
    }

    #[test]
    fn empty_renders_the_envelope_without_a_payload() {
        let response = SubsonicResponse::empty();
        assert_eq!(body(&response, Format::Json), json_document("ok", ""));
        assert_eq!(body(&response, Format::Xml), xml_document("ok", "/>"));
    }

    #[test]
    fn error_renders_failed_status_with_code_and_message() {
        let error = ApiError::MissingParam("id".to_owned());
        let response = SubsonicResponse::error(error.code(), &error.message());
        assert_eq!(
            body(&response, Format::Json),
            json_document(
                "failed",
                r#","error":{"code":10,"message":"required parameter is missing: id"}"#
            )
        );
        assert_eq!(
            body(&response, Format::Xml),
            xml_document(
                "failed",
                concat!(
                    r#"><error code="10" message="required parameter is missing: id"/>"#,
                    r#"</subsonic-response>"#
                )
            )
        );
    }

    #[test]
    fn error_message_with_special_characters_stays_well_formed() {
        let message = "no \"Björk\" & <日本語>";
        let response = SubsonicResponse::error(0, message);

        let json: Value = serde_json::from_str(&body(&response, Format::Json))
            .expect("the JSON body is well-formed");
        assert_eq!(json["subsonic-response"]["error"]["message"], message);

        assert_eq!(
            body(&response, Format::Xml),
            xml_document(
                "failed",
                concat!(
                    r#"><error code="0" message="no &quot;Björk&quot; &amp; &lt;日本語&gt;"/>"#,
                    r#"</subsonic-response>"#
                )
            )
        );
    }

    #[test]
    fn model_renders_arrays_objects_text_and_no_absent_field() {
        let response = SubsonicResponse::ok("album", album());

        let json: Value = serde_json::from_str(&body(&response, Format::Json))
            .expect("the JSON body is well-formed");
        assert_eq!(
            json["subsonic-response"]["album"],
            json!({
                "genres": [{ "value": "Rock" }, { "value": "Jazz" }],
                "id": "al-1",
                "replayGain": { "albumPeak": 0.5 },
                "song": [{ "discNumber": 1, "id": "tr-1" }, { "id": "tr-2" }],
                "songCount": 2,
            })
        );

        assert_eq!(
            body(&response, Format::Xml),
            xml_document(
                "ok",
                concat!(
                    r#"><album id="al-1" songCount="2">"#,
                    r#"<genres>Rock</genres><genres>Jazz</genres>"#,
                    r#"<replayGain albumPeak="0.5"/>"#,
                    r#"<song discNumber="1" id="tr-1"/><song id="tr-2"/>"#,
                    r#"</album></subsonic-response>"#
                )
            )
        );
    }

    #[test]
    fn empty_array_is_present_in_json_and_absent_in_xml() {
        let response = SubsonicResponse::ok("playlists", json!({ "playlist": [] }));
        assert_eq!(
            body(&response, Format::Json),
            json_document("ok", r#","playlists":{"playlist":[]}"#)
        );
        assert_eq!(
            body(&response, Format::Xml),
            xml_document("ok", "><playlists/></subsonic-response>")
        );
    }

    #[test]
    fn render_returns_the_content_type_of_the_format() {
        let response = SubsonicResponse::empty();
        assert_eq!(response.render(Format::Json).0, "application/json");
        assert_eq!(response.render(Format::Xml).0, "application/xml");
    }

    #[test]
    fn format_parses_json_and_xml_and_defaults_to_xml() {
        assert_eq!(Format::parse(None), Ok(Format::Xml));
        assert_eq!(Format::parse(Some("xml")), Ok(Format::Xml));
        assert_eq!(Format::parse(Some("json")), Ok(Format::Json));
    }

    #[test]
    fn format_rejects_every_other_value_with_error_0() {
        for value in ["jsonp", "JSON", "Xml", "yaml", "", " json"] {
            let error = Format::parse(Some(value)).expect_err(value);
            assert_eq!(error, ApiError::Other("unsupported format".to_owned()));
            assert_eq!(error.code(), 0);
        }
    }

    #[test]
    fn unsupported_format_error_renders_in_xml() {
        let error = Format::parse(Some("jsonp")).expect_err("jsonp is not supported");
        let response = SubsonicResponse::error(error.code(), &error.message());
        assert_eq!(
            body(&response, Format::Xml),
            xml_document(
                "failed",
                r#"><error code="0" message="unsupported format"/></subsonic-response>"#
            )
        );
    }

    #[test]
    fn api_error_variants_have_their_codes_and_messages() {
        let cases = [
            (
                ApiError::MissingParam("u".to_owned()),
                10,
                "required parameter is missing: u",
            ),
            (ApiError::BadCredentials, 40, "wrong username or password"),
            (
                ApiError::AuthUnsupported,
                42,
                "authentication mechanism not supported",
            ),
            (
                ApiError::AuthConflict,
                43,
                "conflicting authentication mechanisms",
            ),
            (
                ApiError::Forbidden("download is disabled".to_owned()),
                50,
                "download is disabled",
            ),
            (ApiError::NotFound, 70, "not found"),
            (
                ApiError::Other("unknown endpoint".to_owned()),
                0,
                "unknown endpoint",
            ),
        ];
        for (error, code, message) in cases {
            assert_eq!(error.code(), code, "{error:?}");
            assert_eq!(error.message(), message, "{error:?}");
        }
    }
}
