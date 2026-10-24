//! API errors (`web/apierror/apierror.go`) and error codes (`constants/constants.go`).

use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};
use std::fmt;
use uuid::Uuid;

/// Application error codes, as returned in the `code` field.
pub mod codes {
    pub const INVALID_JOB_UUID: i32 = 1000;
    pub const JOB_UUID_NOT_FOUND: i32 = 1001;
    pub const CANT_CREATE_JOB: i32 = 1002;
    pub const CANT_DELETE_JOB: i32 = 1003;
    pub const CANT_PULL_ANY_JOB: i32 = 1004;
    pub const CANT_PULL_SPECIFIC_JOB: i32 = 1005;
    pub const CANT_START_JOB: i32 = 1006;
    pub const CANT_CLONE_JOB: i32 = 1007;
    pub const CANT_CANCEL_JOB: i32 = 1008;
    pub const CANT_SET_JOB_AS_SUCCESSFUL: i32 = 1009;
    pub const CANT_FAIL_JOB: i32 = 1010;
    pub const CANT_ENQUEUE_JOB: i32 = 1011;
    pub const CANT_GET_JOBS: i32 = 1012;
    pub const INVALID_PARAMETER_VALUE: i32 = 1021;
    pub const CANT_DESERIALIZE_JSON: i32 = 1022;
    pub const INVALID_JSON_DATA: i32 = 1023;
    pub const INVALID_CONTENT_TYPE: i32 = 1024;
    /// New in the Rust port: unknown resource in `POST /api/v1/resource/{name}/unlock`
    /// (Go answered 500 with an empty object).
    pub const RESOURCE_NOT_FOUND: i32 = 1030;
    pub const DATABASE_UNAVAILABLE: i32 = 1440;
}

/// One entry of `validationErrors` (all fields `omitempty`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValidationError {
    pub failed_field: String,
    pub tag: String,
    pub value: String,
}

impl Serialize for ValidationError {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let n = [&self.failed_field, &self.tag, &self.value]
            .iter()
            .filter(|f| !f.is_empty())
            .count();
        let mut st = s.serialize_struct("ValidationError", n)?;
        if !self.failed_field.is_empty() {
            st.serialize_field("failedField", &self.failed_field)?;
        }
        if !self.tag.is_empty() {
            st.serialize_field("tag", &self.tag)?;
        }
        if !self.value.is_empty() {
            st.serialize_field("value", &self.value)?;
        }
        st.end()
    }
}

/// An error answered by the API: HTTP status + JSON body
/// `{"error","details","code","jobUUID","validationErrors"}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub http: u16,
    pub message: String,
    pub details: String,
    pub code: i32,
    /// Always serialized, the zero UUID when not set (Go's `[16]byte` is never "empty").
    pub job_uuid: Uuid,
    pub validation_errors: Vec<ValidationError>,
    /// Low-level cause, logged only (Go's `Err`, `json:"-"`).
    pub cause: Option<String>,
}

impl ApiError {
    pub fn new(http: u16, code: i32, message: impl Into<String>) -> ApiError {
        ApiError {
            http,
            message: message.into(),
            details: String::new(),
            code,
            job_uuid: Uuid::nil(),
            validation_errors: Vec::new(),
            cause: None,
        }
    }

    pub fn job(mut self, id: Uuid) -> ApiError {
        self.job_uuid = id;
        self
    }

    pub fn cause(mut self, cause: impl Into<String>) -> ApiError {
        self.cause = Some(cause.into());
        self
    }

    pub fn validation(mut self, errors: Vec<ValidationError>) -> ApiError {
        self.validation_errors = errors;
        self
    }

    pub fn body(&self) -> Vec<u8> {
        crate::gojson::to_vec(self)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl Serialize for ApiError {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("APIError", 5)?;
        if !self.message.is_empty() {
            st.serialize_field("error", &self.message)?;
        }
        if !self.details.is_empty() {
            st.serialize_field("details", &self.details)?;
        }
        st.serialize_field("code", &self.code)?;
        st.serialize_field("jobUUID", &crate::model::UuidStr(self.job_uuid))?;
        if !self.validation_errors.is_empty() {
            st.serialize_field("validationErrors", &self.validation_errors)?;
        }
        st.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_like_go() {
        let e = ApiError::new(400, codes::INVALID_JOB_UUID, "invalid job uuid").validation(vec![
            ValidationError {
                failed_field: "jobuuid".into(),
                tag: "parameter".into(),
                value: "pull".into(),
            },
        ]);
        assert_eq!(
            String::from_utf8(e.body()).unwrap(),
            r#"{"error":"invalid job uuid","code":1000,"jobUUID":"00000000-0000-0000-0000-000000000000","validationErrors":[{"failedField":"jobuuid","tag":"parameter","value":"pull"}]}"#
        );
    }

    #[test]
    fn empty_fields_are_omitted_like_go() {
        // omitempty on error, details, failedField, tag and value
        let id = Uuid::parse_str("01a16efe-bf98-7ac6-a375-a8352a9e16e5").unwrap();
        let mut e = ApiError::new(500, 7, "").job(id).validation(vec![
            ValidationError::default(),
            ValidationError {
                tag: "required".into(),
                ..Default::default()
            },
        ]);
        e.details = "more".into();
        assert_eq!(
            String::from_utf8(e.body()).unwrap(),
            r#"{"details":"more","code":7,"jobUUID":"01a16efe-bf98-7ac6-a375-a8352a9e16e5","validationErrors":[{},{"tag":"required"}]}"#
        );
        let e = ApiError::new(404, 1001, "job not found").cause("io");
        assert_eq!(e.to_string(), "job not found");
        assert_eq!(e.cause.as_deref(), Some("io"));
    }
}
