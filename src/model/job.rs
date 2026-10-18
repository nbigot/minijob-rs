use crate::gojson::decode::{
    self, DecodeError, Entries, Top, TypeMismatch, entries_of, match_field, set_int, set_map,
    set_string, set_string_slice, set_uint,
};
use serde::Serialize;
use serde::ser::{SerializeMap, SerializeStruct, Serializer};
use serde_json::{Map, Value};
use uuid::Uuid;

/// History event types (`job/job.go`).
pub mod events {
    pub const CREATE: &str = "CREATE";
    pub const DELAY: &str = "DELAY";
    pub const PENDING: &str = "PENDING";
    pub const ENQUEUE: &str = "ENQUEUE";
    pub const START: &str = "START";
    pub const SUCCESS: &str = "SUCCESS";
    pub const CANCEL: &str = "CANCEL";
    pub const FAIL: &str = "FAIL";
    pub const RETRY: &str = "RETRY";
    pub const HIDE: &str = "HIDE";
    pub const TERMINATE: &str = "TERMINATE";
    pub const DELETE: &str = "DELETE";
}

/// Job state, derived from the last history event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum JobState {
    #[default]
    NoState,
    Created,
    Delayed,
    Pending,
    Queued,
    Running,
    Succeeded,
    Failed,
    Canceled,
    Deleted,
    Hidden,
}

impl JobState {
    /// State string of the API (`state` field, filters, callbacks).
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Created => "created",
            JobState::Delayed => "delayed",
            JobState::Pending => "pending",
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Canceled => "canceled",
            JobState::Deleted => "deleted",
            JobState::Hidden => "hidden",
            JobState::NoState => "unknown",
        }
    }

    /// State reached after an event (`GetStateFromEvent`). DELETE has no state.
    pub fn from_event(event_type: &str) -> JobState {
        match event_type {
            events::CREATE => JobState::Created,
            events::DELAY => JobState::Delayed,
            events::PENDING => JobState::Pending,
            events::ENQUEUE => JobState::Queued,
            events::START | events::FAIL | events::RETRY => JobState::Running,
            events::SUCCESS => JobState::Succeeded,
            events::HIDE => JobState::Hidden,
            events::CANCEL => JobState::Canceled,
            events::TERMINATE => JobState::Failed,
            _ => JobState::NoState,
        }
    }

    /// Status filter of `GET /api/v1/jobs/` (case-insensitive, unknown → `NoState`).
    pub fn from_filter(status: &str) -> JobState {
        match status.to_lowercase().as_str() {
            "created" => JobState::Created,
            "delayed" => JobState::Delayed,
            "pending" => JobState::Pending,
            "queued" => JobState::Queued,
            "running" => JobState::Running,
            "succeeded" => JobState::Succeeded,
            "failed" => JobState::Failed,
            "canceled" => JobState::Canceled,
            "deleted" => JobState::Deleted,
            "hidden" => JobState::Hidden,
            _ => JobState::NoState,
        }
    }

    /// Final states: succeeded, canceled, failed.
    pub fn is_completed(self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::Canceled | JobState::Failed
        )
    }
}

/// One entry of the job history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryEvent {
    pub event_type: String,
    /// Unix milliseconds.
    pub timestamp: i64,
}

impl Serialize for HistoryEvent {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("JobHistoryEvent", 2)?;
        st.serialize_field("eventType", &self.event_type)?;
        st.serialize_field("timestamp", &self.timestamp)?;
        st.end()
    }
}

/// A job, as stored by the service and the backends.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Job {
    pub id: Uuid,
    pub topic: String,
    pub priority: i8,
    /// `None` is serialized as `null`, like a nil Go map.
    pub properties: Option<Map<String, Value>>,
    pub history: Vec<HistoryEvent>,
    /// `None` is serialized as `null`, like a nil Go slice.
    pub lock_resources: Option<Vec<String>>,
    pub user_agent: String,
    pub requester: String,
    pub name: String,
    pub session_id: String,
    pub trace_id: String,
    /// Seconds.
    pub visibility_timeout: u64,
    /// Unix milliseconds.
    pub start_after: i64,
    pub callback_url: String,
    pub(crate) state: JobState,
    pub(crate) previous_state: JobState,
}

const JOB_FIELDS: [&str; 14] = [
    "id",
    "topic",
    "priority",
    "properties",
    "history",
    "lockResources",
    "userAgent",
    "requester",
    "name",
    "sessionId",
    "traceId",
    "visibilityTimeout",
    "startAfter",
    "callbackUrl",
];

impl Job {
    pub fn state(&self) -> JobState {
        self.state
    }

    pub fn previous_state(&self) -> JobState {
        self.previous_state
    }

    /// Creation time: timestamp of the first event (0 when the history is empty; Go panics).
    pub fn creation_timestamp(&self) -> i64 {
        self.history.first().map_or(0, |e| e.timestamp)
    }

    /// Time of the last event, 0 without history.
    pub fn last_update(&self) -> i64 {
        self.history.last().map_or(0, |e| e.timestamp)
    }

    /// Recomputes `state` and `previous_state` from the history (`SetStateFromHistory`).
    pub fn set_state_from_history(&mut self) {
        let n = self.history.len();
        self.state = self
            .history
            .last()
            .map_or(JobState::NoState, |e| JobState::from_event(&e.event_type));
        self.previous_state = if n > 1 {
            JobState::from_event(&self.history[n - 2].event_type)
        } else {
            JobState::NoState
        };
    }

    /// Sets the id and the first history event (`Init`).
    pub fn init(&mut self, id: Uuid, creation_ms: i64) {
        self.id = id;
        self.state = JobState::NoState;
        self.previous_state = JobState::NoState;
        self.history = Vec::new();
        self.add_event(events::CREATE, creation_ms);
    }

    /// Appends an event and updates the states (`AddHistoryEvent`).
    pub fn add_event(&mut self, event_type: &str, timestamp: i64) {
        self.history.push(HistoryEvent {
            event_type: event_type.to_string(),
            timestamp,
        });
        self.previous_state = self.state;
        self.state = JobState::from_event(event_type);
    }

    pub fn is_completed(&self) -> bool {
        self.state.is_completed()
    }

    /// Number of FAIL events.
    pub fn count_failures(&self) -> u64 {
        self.history
            .iter()
            .filter(|e| e.event_type == events::FAIL)
            .count() as u64
    }

    pub fn resources(&self) -> &[String] {
        self.lock_resources.as_deref().unwrap_or(&[])
    }

    /// Unix seconds of the most recent ENQUEUE or START event, for queued or running jobs.
    pub fn queued_or_running_timestamp(&self) -> i64 {
        match self.state {
            JobState::Queued | JobState::Running => self
                .history
                .iter()
                .rev()
                .find(|e| e.event_type == events::ENQUEUE || e.event_type == events::START)
                .map_or(0, |e| e.timestamp / 1000),
            _ => 0,
        }
    }

    /// Milliseconds between the first and the last event.
    pub fn duration_first_last(&self) -> i64 {
        if self.history.len() < 2 {
            return 0;
        }
        self.last_update() - self.creation_timestamp()
    }

    /// Copy used by the clone endpoint: everything except the id and the history. Unlike Go,
    /// `properties` and `lockResources` are deep copies, not shared.
    pub fn clone_for_new(&self) -> Job {
        Job {
            id: Uuid::nil(),
            history: Vec::new(),
            state: self.state,
            previous_state: self.previous_state,
            ..self.clone()
        }
    }

    /// Decodes a job written by a backend (`job.NewJob`), state recomputed from the history.
    pub fn decode(bytes: &[u8]) -> Result<Job, DecodeError> {
        let entries = match decode::parse_top(bytes, "job.Job")? {
            Top::Null => Vec::new(),
            Top::Object(e) => e,
        };
        Job::from_entries(&entries)
    }

    /// Builds a job from object members, with Go's decoding rules.
    pub fn from_entries(entries: &Entries) -> Result<Job, DecodeError> {
        let mut job = Job::default();
        let mut failed = None;
        for (key, value) in entries {
            let Some(idx) = match_field(key, &JOB_FIELDS) else {
                continue;
            };
            let r = match idx {
                0 => set_uuid(value, &mut job.id),
                1 => set_string(value, &mut job.topic),
                2 => set_int(value, &mut job.priority),
                3 => set_map(value, &mut job.properties),
                4 => set_history(value, &mut job.history),
                5 => set_string_slice(value, &mut job.lock_resources),
                6 => set_string(value, &mut job.user_agent),
                7 => set_string(value, &mut job.requester),
                8 => set_string(value, &mut job.name),
                9 => set_string(value, &mut job.session_id),
                10 => set_string(value, &mut job.trace_id),
                11 => set_uint(value, &mut job.visibility_timeout),
                12 => set_int(value, &mut job.start_after),
                _ => set_string(value, &mut job.callback_url),
            };
            if r.is_err() && failed.is_none() {
                failed = Some(key.clone());
            }
        }
        if let Some(key) = failed {
            return Err(DecodeError(format!(
                "json: cannot unmarshal into Go struct field Job.{key}"
            )));
        }
        job.set_state_from_history();
        Ok(job)
    }

    /// Go `json.Marshal(job)`.
    pub fn to_json(&self) -> Vec<u8> {
        crate::gojson::to_vec(self)
    }

    fn serialize_fields<S: SerializeStruct>(&self, st: &mut S) -> Result<(), S::Error> {
        st.serialize_field("id", &UuidStr(self.id))?;
        st.serialize_field("topic", &self.topic)?;
        st.serialize_field("priority", &self.priority)?;
        st.serialize_field("properties", &self.properties)?;
        st.serialize_field("history", &self.history)?;
        st.serialize_field("lockResources", &self.lock_resources)?;
        st.serialize_field("userAgent", &self.user_agent)?;
        st.serialize_field("requester", &self.requester)?;
        st.serialize_field("name", &self.name)?;
        st.serialize_field("sessionId", &self.session_id)?;
        st.serialize_field("traceId", &self.trace_id)?;
        st.serialize_field("visibilityTimeout", &self.visibility_timeout)?;
        st.serialize_field("startAfter", &self.start_after)?;
        st.serialize_field("callbackUrl", &self.callback_url)
    }
}

fn set_uuid(v: &Value, out: &mut Uuid) -> Result<(), TypeMismatch> {
    match v {
        Value::Null => Ok(()),
        Value::String(s) => {
            *out = crate::model::uuid::parse_go(s).map_err(|_| TypeMismatch)?;
            Ok(())
        }
        _ => Err(TypeMismatch),
    }
}

fn set_history(v: &Value, out: &mut Vec<HistoryEvent>) -> Result<(), TypeMismatch> {
    const FIELDS: [&str; 2] = ["eventType", "timestamp"];
    match v {
        Value::Null => {
            out.clear();
            Ok(())
        }
        Value::Array(items) => {
            let mut list = Vec::with_capacity(items.len());
            for item in items {
                let mut ev = HistoryEvent::default();
                if let Some(entries) = entries_of(item)? {
                    for (k, val) in &entries {
                        match match_field(k, &FIELDS) {
                            Some(0) => set_string(val, &mut ev.event_type)?,
                            Some(_) => set_int(val, &mut ev.timestamp)?,
                            None => {}
                        }
                    }
                }
                list.push(ev);
            }
            *out = list;
            Ok(())
        }
        _ => Err(TypeMismatch),
    }
}

/// A UUID serialized as Go does (lowercase, hyphenated).
#[derive(Clone, Copy, Debug)]
pub(crate) struct UuidStr(pub Uuid);

impl Serialize for UuidStr {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut buf = uuid::Uuid::encode_buffer();
        s.serialize_str(self.0.hyphenated().encode_lower(&mut buf))
    }
}

impl Serialize for Job {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Job", 14)?;
        self.serialize_fields(&mut st)?;
        st.end()
    }
}

/// `JobResponse`: the job plus its `state` string, as returned by the API.
#[derive(Clone, Copy, Debug)]
pub struct JobResponse<'a>(pub &'a Job);

impl Serialize for JobResponse<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("JobResponse", 15)?;
        self.0.serialize_fields(&mut st)?;
        st.serialize_field("state", self.0.state.as_str())?;
        st.end()
    }
}

/// Serializes a map keyed by UUID with keys sorted like Go (`map[uuid.UUID]...`).
pub(crate) fn serialize_uuid_map<'a, S, V, I>(s: S, items: I) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    V: Serialize + 'a,
    I: IntoIterator<Item = (&'a Uuid, V)>,
{
    let mut items: Vec<(&Uuid, V)> = items.into_iter().collect();
    items.sort_by(|a, b| a.0.cmp(b.0));
    let mut m = s.serialize_map(Some(items.len()))?;
    for (k, v) in items {
        m.serialize_entry(&UuidStr(*k), &v)?;
    }
    m.end()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_with(history: &[(&str, i64)]) -> Job {
        let mut j = Job::default();
        j.init(
            Uuid::parse_str("0190e951-c29a-70aa-ba59-18be4abe9700").unwrap(),
            history[0].1,
        );
        for (e, t) in &history[1..] {
            j.add_event(e, *t);
        }
        j
    }

    // Port of TestJob_UnmarshalJSON.
    #[test]
    fn unmarshal() {
        let data = br#"{
            "topic": "example",
            "properties": {"key1": "value1", "key2": 42, "key3": [1, 2, 3],
                           "key4": {"a": 1, "b": [], "c": {"x": "value1", "y": 42}}},
            "lockResources": ["resource1", "resource2", "resource3:abc:def"],
            "userAgent": "user agent test", "requester": "unit-test",
            "sessionId": "0190e951-c29a-70aa-ba59-18be4abe97a1",
            "traceId": "0190e951-c29a-70aa-ba59-18be4abe97a0"}"#;
        Job::decode(data).unwrap();
        let data2 = br#"{"id": "0190e951-c29a-70aa-ba59-18be4abe97a2", "topic": "example",
            "properties": {"key1": "value1"},
            "history": [{"eventType": "start", "timestamp": 1629876543},
                        {"eventType": "end", "timestamp": 1629876545}]}"#;
        let j = Job::decode(data2).unwrap();
        assert_eq!(j.history.len(), 2);
        assert_eq!(j.state(), JobState::NoState);
        Job::decode(br#"{"properties": {"key1": "value1", "key2": "value2"}}"#).unwrap();
        Job::decode(br#"{"properties": {}}"#).unwrap();
    }

    // Port of TestJob_MarshalJSON.
    #[test]
    fn marshal() {
        let mut props = Map::new();
        props.insert("key1".into(), "value1".into());
        props.insert("key2".into(), 42.into());
        props.insert("key3".into(), true.into());
        props.insert("key4".into(), serde_json::json!(["a", "b", "c"]));
        let j = Job {
            id: Uuid::parse_str("0190e951-c29a-70aa-ba59-18be4abe9700").unwrap(),
            topic: "topic1".into(),
            properties: Some(props),
            lock_resources: Some(vec![
                "resource1".into(),
                "resource2".into(),
                "resource3:abc:def".into(),
            ]),
            user_agent: "user agent test".into(),
            requester: "unit-test".into(),
            session_id: "0190e951-c29a-70aa-ba59-18be4abe97a1".into(),
            trace_id: "0190e951-c29a-70aa-ba59-18be4abe97a0".into(),
            state: JobState::Pending,
            ..Default::default()
        };
        let expected = r#"{"id":"0190e951-c29a-70aa-ba59-18be4abe9700","topic":"topic1","priority":0,"properties":{"key1":"value1","key2":42,"key3":true,"key4":["a","b","c"]},"history":[],"lockResources":["resource1","resource2","resource3:abc:def"],"userAgent":"user agent test","requester":"unit-test","name":"","sessionId":"0190e951-c29a-70aa-ba59-18be4abe97a1","traceId":"0190e951-c29a-70aa-ba59-18be4abe97a0","visibilityTimeout":0,"startAfter":0,"callbackUrl":""}"#;
        assert_eq!(String::from_utf8(j.to_json()).unwrap(), expected);
    }

    #[test]
    fn nil_collections_are_null() {
        let j = Job::default();
        let s = String::from_utf8(j.to_json()).unwrap();
        assert!(s.contains(r#""properties":null"#));
        assert!(s.contains(r#""lockResources":null"#));
    }

    // Port of TestJob_AddHistoryEvent.
    #[test]
    fn add_history_event() {
        use JobState::*;
        use events::*;
        type Case<'a> = (Vec<(&'a str, i64)>, JobState, JobState);
        let cases: Vec<Case> = vec![
            (vec![(CREATE, 1)], NoState, Created),
            (vec![(CREATE, 1), (PENDING, 2)], Created, Pending),
            (
                vec![(CREATE, 1), (PENDING, 2), (ENQUEUE, 3)],
                Pending,
                Queued,
            ),
            (
                vec![(CREATE, 1), (PENDING, 2), (ENQUEUE, 3), (START, 4)],
                Queued,
                Running,
            ),
            (
                vec![
                    (CREATE, 1),
                    (PENDING, 2),
                    (ENQUEUE, 3),
                    (START, 4),
                    (SUCCESS, 5),
                ],
                Running,
                Succeeded,
            ),
            (
                vec![
                    (CREATE, 1),
                    (PENDING, 2),
                    (ENQUEUE, 3),
                    (START, 4),
                    (CANCEL, 5),
                ],
                Running,
                Canceled,
            ),
            (
                vec![
                    (CREATE, 1),
                    (PENDING, 2),
                    (ENQUEUE, 3),
                    (START, 4),
                    (CANCEL, 5),
                    (ENQUEUE, 6),
                ],
                Canceled,
                Queued,
            ),
            (
                vec![
                    (CREATE, 1),
                    (PENDING, 2),
                    (ENQUEUE, 3),
                    (START, 4),
                    (CANCEL, 5),
                    (ENQUEUE, 6),
                    (START, 7),
                    (FAIL, 8),
                ],
                Running,
                Running,
            ),
            (
                vec![
                    (CREATE, 1),
                    (START, 7),
                    (FAIL, 8),
                    (ENQUEUE, 9),
                    (START, 10),
                    (FAIL, 11),
                    (TERMINATE, 12),
                ],
                Running,
                Failed,
            ),
        ];
        for (history, prev, cur) in cases {
            let j = job_with(&history);
            assert_eq!(j.state(), cur, "{history:?}");
            assert_eq!(j.previous_state(), prev, "{history:?}");
        }
    }

    // Port of TestJob_SetStateFromHistory and TestJob_Init.
    #[test]
    fn state_from_history_and_init() {
        let mut j = Job::default();
        j.set_state_from_history();
        assert_eq!(
            (j.state(), j.previous_state()),
            (JobState::NoState, JobState::NoState)
        );
        let mut j = job_with(&[
            (events::CREATE, 1),
            (events::PENDING, 2),
            (events::ENQUEUE, 3),
            (events::START, 4),
            (events::SUCCESS, 5),
        ]);
        j.state = JobState::NoState;
        j.set_state_from_history();
        assert_eq!(
            (j.state(), j.previous_state()),
            (JobState::Succeeded, JobState::Running)
        );
        let j = job_with(&[(events::CREATE, 1735992487270)]);
        assert_eq!(j.history.len(), 1);
        assert_eq!(j.history[0].event_type, events::CREATE);
        assert_eq!(
            (j.state(), j.previous_state()),
            (JobState::Created, JobState::NoState)
        );
    }

    #[test]
    fn response_has_state() {
        let j = job_with(&[(events::CREATE, 1), (events::PENDING, 2)]);
        let s = String::from_utf8(crate::gojson::to_vec(&JobResponse(&j))).unwrap();
        assert!(s.ends_with(r#""callbackUrl":"","state":"pending"}"#));
    }

    #[test]
    fn clone_is_deep() {
        let mut j = job_with(&[(events::CREATE, 1)]);
        j.lock_resources = Some(vec!["a".into()]);
        let mut c = j.clone_for_new();
        c.lock_resources.as_mut().unwrap().push("b".into());
        assert_eq!(j.resources(), ["a"]);
        assert!(c.history.is_empty());
        assert!(c.id.is_nil());
    }
}
