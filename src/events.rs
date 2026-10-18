//! Service events (`event/serviceevents.go`), dispatched to metrics, the event log, callbacks
//! and the storage backend.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceEvent {
    JobCreated,
    JobDelayed,
    JobPending,
    JobEnqueued,
    JobStarted,
    JobSucceeded,
    JobCanceled,
    JobFailed,
    JobHidden,
    JobDeleted,
    JobTimeout,
    JobTerminated,
    JobLoaded,
    JobDeletedAll,
    JobUnlockedAllResources,
    JobHealthcheck,
    JobMetrics,
    EmptyPollReceived,
    Ready,
    Shutdown,
}

impl ServiceEvent {
    /// Event name used by the event log and the Prometheus `event` label.
    ///
    /// Go has no name for `JobHidden` and prints `other`; the port prints `Hidden`.
    pub fn name(self) -> &'static str {
        match self {
            ServiceEvent::JobCreated => "Created",
            ServiceEvent::JobDelayed => "Delayed",
            ServiceEvent::JobPending => "Pending",
            ServiceEvent::JobEnqueued => "Enqueued",
            ServiceEvent::JobStarted => "Started",
            ServiceEvent::JobSucceeded => "Succeeded",
            ServiceEvent::JobCanceled => "Canceled",
            ServiceEvent::JobFailed => "Failed",
            ServiceEvent::JobHidden => "Hidden",
            ServiceEvent::JobDeleted => "Deleted",
            ServiceEvent::JobTimeout => "Timeout",
            ServiceEvent::JobTerminated => "Terminated",
            ServiceEvent::JobLoaded => "Loaded",
            ServiceEvent::JobDeletedAll => "DeletedAll",
            ServiceEvent::JobUnlockedAllResources => "UnlockedAllResources",
            ServiceEvent::JobHealthcheck => "Healthcheck",
            ServiceEvent::JobMetrics => "Metrics",
            ServiceEvent::EmptyPollReceived => "EmptyPollReceived",
            ServiceEvent::Ready => "Ready",
            ServiceEvent::Shutdown => "Shutdown",
        }
    }
}
