use crate::error::{ApiError, ApiErrorKind, ApiResult};
use crate::server::headers::crypto_key::CryptoKeyHeader;
use crate::server::headers::util::{get_header, get_owned_header};
use actix_web::HttpRequest;
use lazy_static::lazy_static;
use regex::Regex;
use std::cmp::min;
use validator::Validate;
use validator_derive::Validate;

lazy_static! {
    static ref VALID_BASE64_URL: Regex = Regex::new(r"^[0-9A-Za-z\-_]+=*$").unwrap();
}

const MAX_TTL: i64 = 60 * 60 * 24 * 60;

/// Extractor and validator for notification headers
#[derive(Debug, Eq, PartialEq, Validate)]
pub struct NotificationHeaders {
    // TTL is a signed value so that validation can catch negative inputs
    #[validate(range(min = 0, message = "TTL must be greater than 0", code = "114"))]
    pub ttl: Option<i64>,

    #[validate(
        length(
            max = 32,
            message = "Topic must be no greater than 32 characters",
            code = "113"
        ),
        regex(
            path = "VALID_BASE64_URL",
            message = "Topic must be URL and Filename safe Base64 alphabet",
            code = "113"
        )
    )]
    pub topic: Option<String>,

    // These fields are validated separately, because the validation is complex
    // and based upon the content encoding
    pub content_encoding: Option<String>,
    pub encryption: Option<String>,
    pub encryption_key: Option<String>,
    pub crypto_key: Option<String>,
}

impl NotificationHeaders {
    /// Extract the notification headers from a request.
    /// This can not be implemented as a `FromRequest` impl because we need to
    /// know if the payload has data, without actually advancing the payload
    /// stream.
    pub fn from_request(req: &HttpRequest, has_data: bool) -> ApiResult<Self> {
        // Collect raw headers
        let ttl = get_header(req, "ttl")
            .and_then(|ttl| ttl.parse().ok())
            // Enforce a maximum TTL, but don't error
            .map(|ttl| min(ttl, MAX_TTL));
        let topic = get_owned_header(req, "topic");
        let content_encoding = get_owned_header(req, "content-encoding");
        let encryption = get_owned_header(req, "encryption");
        let encryption_key = get_owned_header(req, "encryption-key");
        let crypto_key = get_owned_header(req, "crypto-key");

        let headers = NotificationHeaders {
            ttl,
            topic,
            content_encoding,
            encryption,
            encryption_key,
            crypto_key,
        };

        // Validate encryption if there is a message body
        if has_data {
            headers.validate_encryption()?;
        }

        // Validate the other headers
        match headers.validate() {
            Ok(_) => Ok(headers),
            Err(e) => Err(ApiError::from(e)),
        }
    }

    /// Validate the encryption headers according to the various WebPush
    /// standard versions
    fn validate_encryption(&self) -> ApiResult<()> {
        let content_encoding = self.content_encoding.as_deref().ok_or_else(|| {
            ApiErrorKind::InvalidEncryption("Missing Content-Encoding header".to_string())
        })?;

        match content_encoding {
            "aesgcm128" => self.validate_encryption_01_rules()?,
            "aesgcm" => self.validate_encryption_04_rules()?,
            "aes128gcm" => self.validate_encryption_06_rules()?,
            _ => {
                return Err(ApiErrorKind::InvalidEncryption(
                    "Unknown Content-Encoding header".to_string(),
                )
                .into());
            }
        }

        Ok(())
    }

    /// Validates encryption headers according to
    /// draft-ietf-webpush-encryption-01
    fn validate_encryption_01_rules(&self) -> ApiResult<()> {
        Self::assert_base64_item_exists("Encryption", self.encryption.as_deref(), "salt")?;
        Self::assert_base64_item_exists("Encryption-Key", self.encryption_key.as_deref(), "dh")?;
        Self::assert_not_exists("aesgcm128 Crypto-Key", self.crypto_key.as_deref(), "dh")?;

        Ok(())
    }

    /// Validates encryption headers according to
    /// draft-ietf-webpush-encryption-04
    fn validate_encryption_04_rules(&self) -> ApiResult<()> {
        Self::assert_base64_item_exists("Encryption", self.encryption.as_deref(), "salt")?;

        if self.encryption_key.is_some() {
            return Err(ApiErrorKind::InvalidEncryption(
                "Encryption-Key header is not valid for webpush draft 02 or later".to_string(),
            )
            .into());
        }

        if self.crypto_key.is_some() {
            Self::assert_base64_item_exists("Crypto-Key", self.crypto_key.as_deref(), "dh")?;
        }

        Ok(())
    }

    /// Validates encryption headers according to
    /// draft-ietf-httpbis-encryption-encoding-06
    /// (the encryption values are in the payload, so there shouldn't be any in
    /// the headers)
    fn validate_encryption_06_rules(&self) -> ApiResult<()> {
        Self::assert_not_exists("aes128gcm Encryption", self.encryption.as_deref(), "salt")?;
        Self::assert_not_exists("aes128gcm Crypto-Key", self.crypto_key.as_deref(), "dh")?;

        Ok(())
    }

    /// Assert that the given key exists in the header and is valid base64.
    fn assert_base64_item_exists(
        header_name: &str,
        header: Option<&str>,
        key: &str,
    ) -> ApiResult<()> {
        let header = header.ok_or_else(|| {
            ApiErrorKind::InvalidEncryption(format!("Missing {} header", header_name))
        })?;
        let header_data = CryptoKeyHeader::parse(header).ok_or_else(|| {
            ApiErrorKind::InvalidEncryption(format!("Invalid {} header", header_name))
        })?;
        let salt = header_data.get_by_key(key).ok_or_else(|| {
            ApiErrorKind::InvalidEncryption(format!(
                "Missing {} value in {} header",
                key, header_name
            ))
        })?;

        if !VALID_BASE64_URL.is_match(salt) {
            return Err(ApiErrorKind::InvalidEncryption(format!(
                "Invalid {} value in {} header",
                key, header_name
            ))
            .into());
        }

        Ok(())
    }

    /// Assert that the given key does not exist in the header.
    fn assert_not_exists(header_name: &str, header: Option<&str>, key: &str) -> ApiResult<()> {
        let header = match header {
            Some(header) => header,
            None => return Ok(()),
        };

        let header_data = CryptoKeyHeader::parse(header).ok_or_else(|| {
            ApiErrorKind::InvalidEncryption(format!("Invalid {} header", header_name))
        })?;

        if header_data.get_by_key(key).is_some() {
            return Err(ApiErrorKind::InvalidEncryption(format!(
                "Do not include '{}' header in {} header",
                key, header_name
            ))
            .into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::NotificationHeaders;
    use super::MAX_TTL;
    use crate::error::{ApiErrorKind, ApiResult};
    use actix_web::test::TestRequest;

    /// Assert that a result is a validation error and check its serialization
    /// against the JSON value.
    fn assert_validation_error(
        result: ApiResult<NotificationHeaders>,
        expected_json: serde_json::Value,
    ) {
        assert!(result.is_err());
        let errors = match result.unwrap_err().kind {
            ApiErrorKind::Validation(errors) => errors,
            _ => panic!("Expected a validation error"),
        };

        assert_eq!(serde_json::to_value(errors).unwrap(), expected_json);
    }

    /// Assert that a result is a specific encryption error
    fn assert_encryption_error(result: ApiResult<NotificationHeaders>, expected_error: &str) {
        assert!(result.is_err());
        let error = match result.unwrap_err().kind {
            ApiErrorKind::InvalidEncryption(error) => error,
            _ => panic!("Expected an ecryption error"),
        };

        assert_eq!(error, expected_error);
    }

    /// A valid TTL results in no errors or adjustment
    #[test]
    fn valid_ttl() {
        let req = TestRequest::post().header("TTL", "10").to_http_request();
        let result = NotificationHeaders::from_request(&req, false);

        assert!(result.is_ok());
        assert_eq!(result.unwrap().ttl, Some(10));
    }

    /// Negative TTL values are not allowed
    #[test]
    fn negative_ttl() {
        let req = TestRequest::post().header("TTL", "-1").to_http_request();
        let result = NotificationHeaders::from_request(&req, false);

        assert_validation_error(
            result,
            serde_json::json!({
                "ttl": [{
                    "code": "114",
                    "message": "TTL must be greater than 0",
                    "params": {
                        "min": 0.0,
                        "value": -1
                    }
                }]
            }),
        );
    }

    /// TTL values above the max are silently reduced to the max
    #[test]
    fn maximum_ttl() {
        let req = TestRequest::post()
            .header("TTL", (MAX_TTL + 1).to_string())
            .to_http_request();
        let result = NotificationHeaders::from_request(&req, false);

        assert!(result.is_ok());
        assert_eq!(result.unwrap().ttl, Some(MAX_TTL));
    }

    /// A valid topic results in no errors
    #[test]
    fn valid_topic() {
        let req = TestRequest::post()
            .header("TOPIC", "test-topic")
            .to_http_request();
        let result = NotificationHeaders::from_request(&req, false);

        assert!(result.is_ok());
        assert_eq!(result.unwrap().topic, Some("test-topic".to_string()));
    }

    /// Topic names which are too long return an error
    #[test]
    fn too_long_topic() {
        let req = TestRequest::post()
            .header("TOPIC", "test-topic-which-is-too-long-1234")
            .to_http_request();
        let result = NotificationHeaders::from_request(&req, false);

        assert_validation_error(
            result,
            serde_json::json!({
                "topic": [{
                    "code": "113",
                    "message": "Topic must be no greater than 32 characters",
                    "params": {
                        "max": 32,
                        "value": "test-topic-which-is-too-long-1234"
                    }
                }]
            }),
        );
    }

    /// If there is a payload, there must be a content encoding header
    #[test]
    fn payload_without_content_encoding() {
        let req = TestRequest::post().to_http_request();
        let result = NotificationHeaders::from_request(&req, true);

        assert_encryption_error(result, "Missing Content-Encoding header");
    }

    /// Valid 01 draft encryption passes validation
    #[test]
    fn valid_01_encryption() {
        let req = TestRequest::post()
            .header("Content-Encoding", "aesgcm128")
            .header("Encryption", "salt=foo")
            .header("Encryption-Key", "dh=bar")
            .to_http_request();
        let result = NotificationHeaders::from_request(&req, true);

        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            NotificationHeaders {
                ttl: None,
                topic: None,
                content_encoding: Some("aesgcm128".to_string()),
                encryption: Some("salt=foo".to_string()),
                encryption_key: Some("dh=bar".to_string()),
                crypto_key: None
            }
        );
    }

    /// Valid 04 draft encryption passes validation
    #[test]
    fn valid_04_encryption() {
        let req = TestRequest::post()
            .header("Content-Encoding", "aesgcm")
            .header("Encryption", "salt=foo")
            .header("Crypto-Key", "dh=bar")
            .to_http_request();
        let result = NotificationHeaders::from_request(&req, true);

        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            NotificationHeaders {
                ttl: None,
                topic: None,
                content_encoding: Some("aesgcm".to_string()),
                encryption: Some("salt=foo".to_string()),
                encryption_key: None,
                crypto_key: Some("dh=bar".to_string())
            }
        );
    }

    /// Valid 06 draft encryption passes validation
    #[test]
    fn valid_06_encryption() {
        let req = TestRequest::post()
            .header("Content-Encoding", "aes128gcm")
            .header("Encryption", "notsalt=foo")
            .header("Crypto-Key", "notdh=bar")
            .to_http_request();
        let result = NotificationHeaders::from_request(&req, true);

        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            NotificationHeaders {
                ttl: None,
                topic: None,
                content_encoding: Some("aes128gcm".to_string()),
                encryption: Some("notsalt=foo".to_string()),
                encryption_key: None,
                crypto_key: Some("notdh=bar".to_string())
            }
        );
    }

    // TODO: Add negative test cases for encryption validation?
}
