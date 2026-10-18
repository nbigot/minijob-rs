use super::Job;
use crate::gojson::decode::{
    self, DecodeError, Top, match_field, set_int, set_map, set_string, set_string_slice,
};
use serde_json::{Map, Value};

/// Body of `POST /api/v1/job/` (`job.JobRequest`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobRequest {
    pub topic: String,
    pub priority: i8,
    pub properties: Option<Map<String, Value>>,
    pub lock_resources: Option<Vec<String>>,
    pub user_agent: String,
    pub requester: String,
    pub name: String,
    pub session_id: String,
    pub trace_id: String,
    /// Unix milliseconds.
    pub start_after: i64,
    /// Seconds.
    pub delay: i64,
    pub callback_url: String,
}

const FIELDS: [&str; 12] = [
    "topic",
    "priority",
    "properties",
    "lockResources",
    "userAgent",
    "requester",
    "name",
    "sessionId",
    "traceId",
    "startAfter",
    "delay",
    "callbackUrl",
];

impl JobRequest {
    /// Decodes a request body with Go's `json.Unmarshal` rules.
    pub fn decode(bytes: &[u8]) -> Result<JobRequest, DecodeError> {
        let entries = match decode::parse_top(bytes, "job.JobRequest")? {
            Top::Null => return Ok(JobRequest::default()),
            Top::Object(e) => e,
        };
        let mut req = JobRequest::default();
        let mut failed = None;
        for (key, value) in &entries {
            let Some(idx) = match_field(key, &FIELDS) else {
                continue;
            };
            let r = match idx {
                0 => set_string(value, &mut req.topic),
                1 => set_int(value, &mut req.priority),
                2 => set_map(value, &mut req.properties),
                3 => set_string_slice(value, &mut req.lock_resources),
                4 => set_string(value, &mut req.user_agent),
                5 => set_string(value, &mut req.requester),
                6 => set_string(value, &mut req.name),
                7 => set_string(value, &mut req.session_id),
                8 => set_string(value, &mut req.trace_id),
                9 => set_int(value, &mut req.start_after),
                10 => set_int(value, &mut req.delay),
                _ => set_string(value, &mut req.callback_url),
            };
            if r.is_err() && failed.is_none() {
                failed = Some(key.clone());
            }
        }
        match failed {
            Some(key) => Err(DecodeError(format!(
                "json: cannot unmarshal into Go struct field JobRequest.{key}"
            ))),
            None => Ok(req),
        }
    }

    /// Converts the request into a job without id nor history (`NewJobFromRequest`).
    /// `delay > 0` moves `startAfter` (or now) by `delay` seconds, with Go's wrapping arithmetic.
    pub fn into_job(self, now_ms: i64) -> Job {
        let mut start_after = self.start_after;
        if self.delay > 0 {
            if start_after == 0 {
                start_after = now_ms;
            }
            start_after = start_after.wrapping_add(self.delay.wrapping_mul(1000));
        }
        Job {
            topic: self.topic,
            priority: self.priority,
            properties: self.properties,
            lock_resources: self.lock_resources,
            user_agent: self.user_agent,
            requester: self.requester,
            name: self.name,
            session_id: self.session_id,
            trace_id: self.trace_id,
            start_after,
            callback_url: self.callback_url,
            ..Job::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decode_like_go() {
        let r = JobRequest::decode(
            br#"{"TOPIC":"a","topic":"b","Priority":3,"properties":{"x":1},"lockResources":null,"unknown":[1]}"#,
        )
        .unwrap();
        assert_eq!(r.topic, "b");
        assert_eq!(r.priority, 3);
        assert_eq!(r.properties.map(Value::Object), Some(json!({"x": 1})));
        assert_eq!(r.lock_resources, None);
        assert_eq!(JobRequest::decode(b"null").unwrap(), JobRequest::default());
        assert!(JobRequest::decode(br#"{"priority":"5"}"#).is_err());
        assert!(JobRequest::decode(br#"{"priority":128}"#).is_err());
        assert!(JobRequest::decode(br#"{"properties":[]}"#).is_err());
        assert!(JobRequest::decode(br#"{"topic":"a"} {}"#).is_err());
        let r = JobRequest::decode(br#"{"topic":"a","topic":null}"#).unwrap();
        assert_eq!(r.topic, "a");
    }

    #[test]
    fn delay_moves_start_after() {
        let j = JobRequest {
            delay: 2,
            ..Default::default()
        }
        .into_job(1000);
        assert_eq!(j.start_after, 3000);
        let j = JobRequest {
            delay: 2,
            start_after: 10_000,
            ..Default::default()
        }
        .into_job(1000);
        assert_eq!(j.start_after, 12_000);
        let j = JobRequest {
            delay: -5,
            start_after: 7,
            ..Default::default()
        }
        .into_job(1000);
        assert_eq!(j.start_after, 7);
    }
}
