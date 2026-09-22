use anyhow::Context;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use komga_application::task_processing::{PersistedTaskRowShape, TaskQueueRecord};
use sqlx::Row;
use sqlx::SqlitePool;

use komga_infrastructure_base::sqlite::{connect_shared_pool, default_read_max_connections};

#[derive(Clone, Debug)]
pub(super) struct PersistedTaskStoreRecord {
    pub id: String,
    pub simple_type: String,
    pub priority: i32,
    pub group: Option<String>,
    pub payload: Option<String>,
    pub owner: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct SqliteTaskQueueStore {
    tasks_pool: SqlitePool,
}

fn persisted_row_from_runtime_record(
    task: &PersistedTaskStoreRecord,
) -> anyhow::Result<PersistedTaskRowShape> {
    PersistedTaskRowShape::from_queue_record(runtime_record_from_store_record(task.clone()))
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!("build persisted task row for '{}'", task.id))
        })
}

impl SqliteTaskQueueStore {
    pub(super) async fn new(tasks_db_file: PathBuf) -> anyhow::Result<Option<Self>> {
        match fs::metadata(&tasks_db_file) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow::anyhow!(format!(
                    "inspect tasks sqlite file '{}': {error}",
                    tasks_db_file.display()
                )));
            }
        }

        let tasks_pool = connect_shared_pool(&tasks_db_file, default_read_max_connections())
            .await
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "open tasks sqlite pool '{}'",
                    tasks_db_file.display()
                ))
            })?;

        Ok(Some(Self { tasks_pool }))
    }

    pub(super) async fn load_records(&self) -> anyhow::Result<Vec<PersistedTaskStoreRecord>> {
        let rows = sqlx::query(
            r#"SELECT
                ID,
                PRIORITY,
                GROUP_ID,
                CLASS,
                SIMPLE_TYPE,
                PAYLOAD,
                OWNER
            FROM TASK
            ORDER BY PRIORITY DESC, LAST_MODIFIED_DATE ASC, ID ASC"#,
        )
        .fetch_all(&self.tasks_pool)
        .await
        .context("persisted task queue rows should be readable")?;

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            records.push(store_record_from_runtime_record(
                persisted_row_shape(row).into_queue_record(),
            ));
        }
        Ok(records)
    }

    pub(super) async fn persist_task(&self, task: &PersistedTaskStoreRecord) -> anyhow::Result<()> {
        let row = persisted_row_from_runtime_record(task)?;
        sqlx::query(
            r#"INSERT INTO TASK (
                ID,
                PRIORITY,
                GROUP_ID,
                CLASS,
                SIMPLE_TYPE,
                PAYLOAD
            ) VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(ID) DO UPDATE
            SET PRIORITY = excluded.PRIORITY,
                GROUP_ID = excluded.GROUP_ID,
                CLASS = excluded.CLASS,
                SIMPLE_TYPE = excluded.SIMPLE_TYPE,
                PAYLOAD = excluded.PAYLOAD,
                LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
            WHERE OWNER IS NULL"#,
        )
        .bind(row.id)
        .bind(row.priority)
        .bind(row.group)
        .bind(row.class_name)
        .bind(row.simple_type)
        .bind(row.payload)
        .execute(&self.tasks_pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error)
                .context(format!("persist queued task '{}' to TASK table: ", task.id))
        })?;
        Ok(())
    }

    pub(super) async fn claim_task(&self, task_id: &str, owner: &str) -> anyhow::Result<()> {
        let task_id = task_id.to_string();
        let owner = owner.to_string();
        sqlx::query(
            r#"UPDATE TASK
            SET OWNER = ?, LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
            WHERE ID = ?"#,
        )
        .bind(owner)
        .bind(task_id)
        .execute(&self.tasks_pool)
        .await
        .context("persist claimed task owner to TASK table")?;
        Ok(())
    }

    pub(super) async fn delete_task(&self, task_id: &str) -> anyhow::Result<bool> {
        let task_id = task_id.to_string();
        let deleted = sqlx::query("DELETE FROM TASK WHERE ID = ?")
            .bind(task_id)
            .execute(&self.tasks_pool)
            .await
            .context("delete completed task row from TASK table")?
            .rows_affected();
        Ok(deleted > 0)
    }

    pub(super) async fn disown_all(&self) -> anyhow::Result<()> {
        sqlx::query(
            r#"UPDATE TASK
            SET OWNER = NULL, LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
            WHERE OWNER IS NOT NULL"#,
        )
        .execute(&self.tasks_pool)
        .await
        .context("disown owned task rows in TASK table")?;
        Ok(())
    }

    pub(super) async fn clear_unowned(&self) -> anyhow::Result<usize> {
        let deleted = sqlx::query("DELETE FROM TASK WHERE OWNER IS NULL")
            .execute(&self.tasks_pool)
            .await
            .context("delete unowned task rows from TASK table")?
            .rows_affected() as usize;
        Ok(deleted)
    }
}

fn store_record_from_runtime_record(task: TaskQueueRecord) -> PersistedTaskStoreRecord {
    PersistedTaskStoreRecord {
        id: task.id,
        simple_type: task.simple_type,
        priority: task.priority,
        group: task.group,
        payload: task.payload,
        owner: task.owner,
    }
}

fn runtime_record_from_store_record(record: PersistedTaskStoreRecord) -> TaskQueueRecord {
    let mut task = TaskQueueRecord::new(record.id, record.priority, record.group)
        .with_simple_type(record.simple_type);
    if let Some(payload) = record.payload {
        task = task.with_payload(payload);
    }
    task.owner = record.owner;
    task
}

fn persisted_row_shape(row: sqlx::sqlite::SqliteRow) -> PersistedTaskRowShape {
    PersistedTaskRowShape {
        id: row.get::<String, _>("ID"),
        priority: row.get::<i64, _>("PRIORITY") as i32,
        group: row.get::<Option<String>, _>("GROUP_ID"),
        class_name: row.get::<String, _>("CLASS"),
        simple_type: row.get::<String, _>("SIMPLE_TYPE"),
        payload: row.get::<String, _>("PAYLOAD"),
        owner: row.get::<Option<String>, _>("OWNER"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komga_infrastructure_base::sqlite::{bootstrap_tasks_pool, connect_task_write_pool};

    #[test]
    fn known_persisted_row_uses_kotlin_class_name() {
        let task = PersistedTaskStoreRecord {
            id: "AnalyzeBook_book-1".to_string(),
            simple_type: "AnalyzeBook".to_string(),
            priority: 6,
            group: None,
            payload: None,
            owner: None,
        };
        let row = persisted_row_from_runtime_record(&task).expect("known task row should build");
        assert_eq!(
            row.class_name,
            "org.gotson.komga.application.tasks.Task$AnalyzeBook"
        );
        assert_eq!(row.simple_type, "AnalyzeBook");
    }

    #[tokio::test]
    async fn task_store_initialization_reports_tasks_db_open_errors() {
        let root = unique_temp_dir("tasks-db-open-error");
        std::fs::create_dir_all(&root).expect("task store fixture root should exist");
        let tasks_db_file = root.join("tasks.sqlite");
        std::fs::create_dir(&tasks_db_file).expect("directory at tasks db path should be created");

        let error = SqliteTaskQueueStore::new(tasks_db_file)
            .await
            .expect_err("tasks db open error should be reported");
        assert!(
            error.to_string().contains("open tasks sqlite pool"),
            "unexpected error: {error}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persist_task_keeps_owned_duplicate_payload() {
        let tasks_db_file = unique_temp_dir("owned-duplicate-payload");
        let bootstrap_pool = connect_task_write_pool(&tasks_db_file)
            .await
            .expect("tasks db should open");
        bootstrap_tasks_pool(&bootstrap_pool)
            .await
            .expect("tasks db should bootstrap");

        let store = SqliteTaskQueueStore::new(tasks_db_file.clone())
            .await
            .expect("task store should open")
            .expect("task store should exist");
        let original = PersistedTaskStoreRecord {
            id: "RefreshBookMetadata_book-1".to_string(),
            simple_type: "RefreshBookMetadata".to_string(),
            priority: 6,
            group: None,
            payload: Some(r#"{"bookId":"book-1","capabilities":["TITLE"]}"#.to_string()),
            owner: None,
        };
        store
            .persist_task(&original)
            .await
            .expect("original task should persist");
        store
            .claim_task(&original.id, "rust-worker")
            .await
            .expect("original task should be claimed");

        let duplicate = PersistedTaskStoreRecord {
            payload: Some(r#"{"bookId":"book-1","capabilities":["AUTHORS"]}"#.to_string()),
            ..original.clone()
        };
        store
            .persist_task(&duplicate)
            .await
            .expect("owned duplicate should be accepted as a no-op");

        let records = store
            .load_records()
            .await
            .expect("task rows should remain readable");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].owner.as_deref(), Some("rust-worker"));
        let payload = records[0]
            .payload
            .as_deref()
            .expect("owned task payload should remain present");
        assert!(payload.contains("TITLE"));
        assert!(!payload.contains("AUTHORS"));

        store.tasks_pool.close().await;
        bootstrap_pool.close().await;
        let _ = std::fs::remove_file(&tasks_db_file);
        let _ = std::fs::remove_file(format!("{}-wal", tasks_db_file.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tasks_db_file.display()));
    }

    fn unique_temp_dir(case_id: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("komga-rust-task-store-{case_id}-{nanos}"))
    }
}
