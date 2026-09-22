use std::collections::VecDeque;
use std::sync::Mutex;

const MAX_RUNTIME_SSE_EVENTS: usize = 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeSseEvent {
    LibraryAdded {
        library_id: String,
    },
    LibraryChanged {
        library_id: String,
    },
    LibraryDeleted {
        library_id: String,
    },
    SeriesAdded {
        series_id: String,
        library_id: String,
    },
    SeriesChanged {
        series_id: String,
        library_id: String,
    },
    BookAdded {
        book_id: String,
        series_id: String,
        library_id: String,
    },
    BookChanged {
        book_id: String,
        series_id: String,
        library_id: String,
    },
    BookImported {
        book_id: Option<String>,
        source_file: String,
        success: bool,
        message: Option<String>,
    },
    CollectionAdded {
        collection_id: String,
        series_ids: Vec<String>,
    },
    CollectionChanged {
        collection_id: String,
        series_ids: Vec<String>,
    },
    CollectionDeleted {
        collection_id: String,
        series_ids: Vec<String>,
    },
    ReadListAdded {
        readlist_id: String,
        book_ids: Vec<String>,
    },
    ReadListChanged {
        readlist_id: String,
        book_ids: Vec<String>,
    },
    ReadListDeleted {
        readlist_id: String,
        book_ids: Vec<String>,
    },
    ReadProgressChanged {
        book_id: String,
        user_id: String,
    },
    ReadProgressDeleted {
        book_id: String,
        user_id: String,
    },
    ReadProgressSeriesChanged {
        series_id: String,
        user_id: String,
    },
    ReadProgressSeriesDeleted {
        series_id: String,
        user_id: String,
    },
    ThumbnailBookAdded {
        book_id: String,
        series_id: String,
        selected: bool,
    },
    ThumbnailBookDeleted {
        book_id: String,
        series_id: String,
        selected: bool,
    },
    ThumbnailSeriesAdded {
        series_id: String,
        selected: bool,
    },
    ThumbnailSeriesDeleted {
        series_id: String,
        selected: bool,
    },
    ThumbnailReadListAdded {
        readlist_id: String,
        selected: bool,
    },
    ThumbnailReadListDeleted {
        readlist_id: String,
        selected: bool,
    },
    ThumbnailCollectionAdded {
        collection_id: String,
        selected: bool,
    },
    ThumbnailCollectionDeleted {
        collection_id: String,
        selected: bool,
    },
    SessionExpired {
        user_id: String,
    },
}

impl RuntimeSseEvent {
    fn admin_only(&self) -> bool {
        matches!(self, Self::BookImported { .. })
    }

    fn user_id_only(&self) -> Option<&str> {
        match self {
            Self::ReadProgressChanged { user_id, .. }
            | Self::ReadProgressDeleted { user_id, .. }
            | Self::ReadProgressSeriesChanged { user_id, .. }
            | Self::ReadProgressSeriesDeleted { user_id, .. }
            | Self::SessionExpired { user_id } => Some(user_id.as_str()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSseEventRecord {
    pub id: u64,
    pub event: RuntimeSseEvent,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSseEventBatch {
    pub current_cursor: u64,
    pub events: Vec<RuntimeSseEventRecord>,
}

#[derive(Default)]
struct RuntimeSseEventState {
    last_event_id: u64,
    events: VecDeque<RuntimeSseEventRecord>,
}

#[derive(Default)]
pub struct RuntimeSseEventStore {
    state: Mutex<RuntimeSseEventState>,
}

impl RuntimeSseEventStore {
    pub fn register(&self, event: RuntimeSseEvent) -> u64 {
        let mut state = self
            .state
            .lock()
            .expect("runtime sse event state lock should not be poisoned");
        state.last_event_id += 1;
        let event_id = state.last_event_id;
        tracing::debug!(event_id, ?event, "runtime sse event registered");
        state.events.push_back(RuntimeSseEventRecord {
            id: event_id,
            event,
        });
        while state.events.len() > MAX_RUNTIME_SSE_EVENTS {
            state.events.pop_front();
        }
        event_id
    }
}

pub trait RuntimeSseEventSink: Send + Sync {
    fn register(&self, event: RuntimeSseEvent);
}

pub trait RuntimeSseEventLog: Send + Sync {
    fn current_cursor(&self) -> u64;

    fn pending_events(
        &self,
        last_seen_event_id: u64,
        user_id: &str,
        admin: bool,
    ) -> RuntimeSseEventBatch;
}

#[async_trait::async_trait]
pub trait RuntimeSseEventSubscription: Send {
    async fn changed(&mut self) -> bool;
}

pub trait RuntimeSseEventSource: RuntimeSseEventSink + RuntimeSseEventLog {
    fn subscribe(&self) -> Box<dyn RuntimeSseEventSubscription>;
}

impl RuntimeSseEventSink for RuntimeSseEventStore {
    fn register(&self, event: RuntimeSseEvent) {
        RuntimeSseEventStore::register(self, event);
    }
}

impl RuntimeSseEventLog for RuntimeSseEventStore {
    fn current_cursor(&self) -> u64 {
        self.state
            .lock()
            .expect("runtime sse event state lock should not be poisoned")
            .last_event_id
    }

    fn pending_events(
        &self,
        last_seen_event_id: u64,
        user_id: &str,
        admin: bool,
    ) -> RuntimeSseEventBatch {
        let state = self
            .state
            .lock()
            .expect("runtime sse event state lock should not be poisoned");
        let current_cursor = state.last_event_id;
        let events = state
            .events
            .iter()
            .filter(|event| event.id > last_seen_event_id)
            .filter(|event| !event.event.admin_only() || admin)
            .filter(|event| {
                event
                    .event
                    .user_id_only()
                    .is_none_or(|expected_user_id| expected_user_id == user_id)
            })
            .cloned()
            .collect::<Vec<_>>();

        RuntimeSseEventBatch {
            current_cursor,
            events,
        }
    }
}
