use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskQueueRecord {
    pub id: String,
    pub simple_type: String,
    pub priority: i32,
    pub group: Option<String>,
    pub payload: Option<String>,
    pub owner: Option<String>,
    pub order: usize,
}

impl TaskQueueRecord {
    pub fn new(id: impl Into<String>, priority: i32, group: Option<String>) -> Self {
        let id = id.into();
        let simple_type = id
            .split_once(':')
            .map(|(task_type, _)| task_type)
            .unwrap_or(id.as_str())
            .to_string();

        Self {
            id,
            simple_type,
            priority,
            group,
            payload: None,
            owner: None,
            order: 0,
        }
    }

    pub fn with_payload(mut self, payload: impl Into<String>) -> Self {
        self.payload = Some(payload.into());
        self
    }

    pub fn with_simple_type(mut self, simple_type: impl Into<String>) -> Self {
        self.simple_type = simple_type.into();
        self
    }

    pub fn target(&self) -> Option<&str> {
        self.id.strip_prefix(&self.simple_type).and_then(|suffix| {
            suffix
                .strip_prefix(':')
                .or_else(|| suffix.strip_prefix('_'))
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskProcessingError {
    pub message: String,
}

impl TaskProcessingError {
    pub fn invalid_task(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }

    pub fn unsupported_task(task_type: &str) -> Self {
        Self {
            message: format!("unsupported runtime task type: {task_type}"),
        }
    }

    pub fn runtime(message: impl std::fmt::Display) -> Self {
        Self {
            // `{:#}` renders the full anyhow error chain (context: source: ...),
            // while `{}` keeps only the outermost context and drops the root cause.
            message: format!("{message:#}"),
        }
    }
}

impl std::fmt::Display for TaskProcessingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TaskProcessingError {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskExecutionOutcome {
    follow_up_tasks: Vec<TaskQueueRecord>,
}

impl TaskExecutionOutcome {
    pub fn completed() -> Self {
        Self::default()
    }

    pub fn with_follow_up_tasks(follow_up_tasks: Vec<TaskQueueRecord>) -> Self {
        Self { follow_up_tasks }
    }

    pub fn follow_up_tasks(self) -> Vec<TaskQueueRecord> {
        self.follow_up_tasks
    }
}

#[derive(Debug)]
pub struct TaskExecutionResult {
    pub task: TaskQueueRecord,
    pub outcome: Result<TaskExecutionOutcome, TaskProcessingError>,
}

#[async_trait::async_trait]
pub trait TaskExecutionFinalizationPort: Sync {
    async fn enqueue_follow_up_task(
        &self,
        task: TaskQueueRecord,
    ) -> Result<(), TaskProcessingError>;

    async fn complete_task(&self, task_id: &str) -> Result<(), TaskProcessingError>;

    async fn fail_task(
        &self,
        task: &TaskQueueRecord,
        error: &TaskProcessingError,
    ) -> Result<(), TaskProcessingError>;
}

pub async fn finalize_task_execution(
    port: &dyn TaskExecutionFinalizationPort,
    task_result: TaskExecutionResult,
) -> Result<(), TaskProcessingError> {
    match task_result.outcome {
        Ok(outcome) => {
            for task in outcome.follow_up_tasks() {
                port.enqueue_follow_up_task(task).await?;
            }
            port.complete_task(&task_result.task.id).await?;
            Ok(())
        }
        Err(error) => {
            port.fail_task(&task_result.task, &error).await?;
            Err(error)
        }
    }
}

/// Execution hook for processing claimed tasks. Used internally by the orchestrator.
pub trait TaskQueueExecutionPort {
    fn execute_claimed_task(&mut self, task: &TaskQueueRecord) -> Result<(), TaskProcessingError>;
}

#[derive(Clone, Debug)]
pub struct TaskQueueOrchestrator {
    consumer_owner: String,
    consumes_queue: bool,
    task_pool_size: usize,
    tasks: Vec<TaskQueueRecord>,
    next_order: usize,
}

impl TaskQueueOrchestrator {
    pub fn new(consumer_owner: impl Into<String>, consumes_queue: bool) -> Self {
        Self {
            consumer_owner: consumer_owner.into(),
            consumes_queue,
            task_pool_size: 1,
            tasks: Vec::new(),
            next_order: 0,
        }
    }

    pub fn tasks(&self) -> &[TaskQueueRecord] {
        &self.tasks
    }

    pub fn take_available_batch(&mut self) -> Vec<TaskQueueRecord> {
        if !self.consumes_queue {
            return Vec::new();
        }

        if self.task_pool_size <= 1 {
            return self.take_next().into_iter().collect();
        }

        let mut selected = Vec::new();
        while selected.len() < self.task_pool_size {
            let Some(task) = self.take_next() else {
                break;
            };
            selected.push(task);
        }
        selected
    }

    pub fn process_available<E>(&mut self, executor: &mut E) -> Result<usize, TaskProcessingError>
    where
        E: TaskQueueExecutionPort,
    {
        if !self.consumes_queue {
            return Ok(0);
        }

        let mut processed = 0usize;
        loop {
            let batch = self.take_available_batch();
            if batch.is_empty() {
                return Ok(processed);
            }

            let mut iter = batch.into_iter();
            while let Some(task) = iter.next() {
                match executor.execute_claimed_task(&task) {
                    Ok(()) => {
                        let _ = self.complete(&task.id);
                        processed += 1;
                    }
                    Err(error) => {
                        let _ = self.disown(&task.id);
                        for remaining in iter {
                            let _ = self.disown(&remaining.id);
                        }
                        return Err(error);
                    }
                }
            }
        }
    }

    pub fn claim(&mut self, task_id: &str, owner: &str) -> bool {
        match self.tasks.iter_mut().find(|task| task.id == task_id) {
            Some(task) => {
                task.owner = Some(owner.to_string());
                true
            }
            None => false,
        }
    }

    pub fn enqueue(&mut self, mut task: TaskQueueRecord) {
        task.order = self.next_order;
        self.next_order += 1;
        if let Some(existing) = self.tasks.iter_mut().find(|t| t.id == task.id) {
            if existing.owner.is_none() {
                *existing = task;
            }
        } else {
            self.tasks.push(task);
        }
    }

    pub fn take_available(&mut self, owner: &str) -> Option<TaskQueueRecord> {
        let mut locked_groups = BTreeSet::new();
        for task in &self.tasks {
            if task.owner.is_some()
                && let Some(group) = &task.group
            {
                locked_groups.insert(group.clone());
            }
        }

        let selected_index = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| {
                task.owner.is_none()
                    && task
                        .group
                        .as_ref()
                        .is_none_or(|group| !locked_groups.contains(group))
            })
            .max_by(|(_, left), (_, right)| {
                left.priority
                    .cmp(&right.priority)
                    .then_with(|| right.order.cmp(&left.order))
            })
            .map(|(index, _)| index)?;

        let task = self.tasks.get_mut(selected_index)?;
        task.owner = Some(owner.to_string());
        Some(task.clone())
    }

    pub fn complete(&mut self, task_id: &str) -> bool {
        let original = self.tasks.len();
        self.tasks.retain(|task| task.id != task_id);
        self.tasks.len() != original
    }

    pub fn disown(&mut self, task_id: &str) -> bool {
        let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) else {
            return false;
        };
        task.owner = None;
        true
    }

    pub fn disown_all(&mut self) -> usize {
        let mut disowned = 0;
        for task in &mut self.tasks {
            if task.owner.take().is_some() {
                disowned += 1;
            }
        }
        disowned
    }

    pub fn clear_unowned(&mut self) -> usize {
        let original = self.tasks.len();
        self.tasks.retain(|task| task.owner.is_some());
        original - self.tasks.len()
    }

    pub fn count_by_simple_type(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for task in &self.tasks {
            *counts.entry(task.simple_type.clone()).or_insert(0) += 1;
        }
        counts
    }

    fn take_next(&mut self) -> Option<TaskQueueRecord> {
        self.take_available(&self.consumer_owner.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingFinalizationPort {
        events: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl TaskExecutionFinalizationPort for RecordingFinalizationPort {
        async fn enqueue_follow_up_task(
            &self,
            task: TaskQueueRecord,
        ) -> Result<(), TaskProcessingError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("enqueue:{}", task.id));
            Ok(())
        }

        async fn complete_task(&self, task_id: &str) -> Result<(), TaskProcessingError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("complete:{task_id}"));
            Ok(())
        }

        async fn fail_task(
            &self,
            task: &TaskQueueRecord,
            error: &TaskProcessingError,
        ) -> Result<(), TaskProcessingError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("fail:{}:{}", task.id, error.message));
            Ok(())
        }
    }

    #[test]
    fn task_queue_record_target_reads_kotlin_and_runtime_id_shapes() {
        let kotlin_task =
            TaskQueueRecord::new("AnalyzeBook_book-1", 6, None).with_simple_type("AnalyzeBook");
        let runtime_task =
            TaskQueueRecord::new("RuntimeTask:book-2", 6, None).with_simple_type("RuntimeTask");
        let targetless_task = TaskQueueRecord::new("UpgradeIndex", 2, None);

        assert_eq!(kotlin_task.target(), Some("book-1"));
        assert_eq!(runtime_task.target(), Some("book-2"));
        assert_eq!(targetless_task.target(), None);
    }

    #[test]
    fn enqueue_owned_duplicate_keeps_original_task_payload() {
        let mut queue = TaskQueueOrchestrator::new("worker", true);
        queue.enqueue(TaskQueueRecord::new("Task_task-1", 1, None).with_payload("original"));
        assert!(queue.claim("Task_task-1", "worker"));

        queue.enqueue(TaskQueueRecord::new("Task_task-1", 9, None).with_payload("duplicate"));

        assert_eq!(queue.tasks().len(), 1);
        assert_eq!(queue.tasks()[0].payload.as_deref(), Some("original"));
        assert_eq!(queue.tasks()[0].priority, 1);
        assert_eq!(queue.tasks()[0].owner.as_deref(), Some("worker"));
    }

    #[tokio::test]
    async fn finalize_task_execution_enqueues_follow_ups_before_completing_claimed_task() {
        let port = RecordingFinalizationPort::default();
        let task = TaskQueueRecord::new("ImportBook_book-1", 100, None);
        let first_follow_up = TaskQueueRecord::new("AnalyzeBook_book-1", 101, None);
        let second_follow_up = TaskQueueRecord::new("RefreshBookMetadata_book-1", 4, None);

        finalize_task_execution(
            &port,
            TaskExecutionResult {
                task,
                outcome: Ok(TaskExecutionOutcome::with_follow_up_tasks(vec![
                    first_follow_up,
                    second_follow_up,
                ])),
            },
        )
        .await
        .expect("successful task finalization should complete");

        assert_eq!(
            port.events.lock().unwrap().as_slice(),
            [
                "enqueue:AnalyzeBook_book-1",
                "enqueue:RefreshBookMetadata_book-1",
                "complete:ImportBook_book-1",
            ]
        );
    }

    #[tokio::test]
    async fn finalize_task_execution_marks_failed_task_without_enqueuing_follow_ups() {
        let port = RecordingFinalizationPort::default();
        let task = TaskQueueRecord::new("ImportBook_book-1", 100, None);
        let error = TaskProcessingError::runtime("import failed");

        let result = finalize_task_execution(
            &port,
            TaskExecutionResult {
                task,
                outcome: Err(error.clone()),
            },
        )
        .await;

        assert_eq!(result, Err(error));
        assert_eq!(
            port.events.lock().unwrap().as_slice(),
            ["fail:ImportBook_book-1:import failed"]
        );
    }
}
