use std::path::{Path, PathBuf};

use anyhow::Context;
use sqlx::SqlitePool;

use crate::{connect_read_pool, connect_write_pool, evict_shared_pools_for_paths};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./sqlx-migrations/riir");

#[derive(Clone, Debug)]
pub struct RiirDatabase {
    path: PathBuf,
    read_pool: SqlitePool,
    write_pool: SqlitePool,
}

impl RiirDatabase {
    pub async fn file_backed(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create RIIR database directory '{}'",
                    parent.display()
                )
            })?;
        }

        Self::open_once(path).await
    }

    async fn open_once(path: &Path) -> anyhow::Result<Self> {
        let write_pool = connect_write_pool(path).await.with_context(|| {
            format!(
                "failed to open RIIR database write pool '{}'",
                path.display()
            )
        })?;
        if let Err(error) = MIGRATOR.run(&write_pool).await {
            release_riir_database_pools(path).await;
            return Err(anyhow::Error::new(error).context(format!(
                "failed to migrate RIIR database '{}'",
                path.display()
            )));
        }
        let read_pool = match connect_read_pool(path).await {
            Ok(pool) => pool,
            Err(error) => {
                release_riir_database_pools(path).await;
                return Err(anyhow::Error::new(error).context(format!(
                    "failed to open RIIR database read pool '{}'",
                    path.display()
                )));
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            read_pool,
            write_pool,
        })
    }

    pub fn read_pool(&self) -> &SqlitePool {
        &self.read_pool
    }

    pub fn write_pool(&self) -> &SqlitePool {
        &self.write_pool
    }

    pub async fn close(self) {
        release_riir_database_pools(&self.path).await;
    }
}

async fn release_riir_database_pools(path: &Path) {
    for pool in evict_shared_pools_for_paths(&[path.to_path_buf()]) {
        pool.close().await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use sqlx::Row;

    use super::RiirDatabase;

    fn riir_db_path(case_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("{case_name}-{}-{nonce}", std::process::id()))
            .join("riir.sqlite")
    }

    #[tokio::test]
    async fn creates_and_migrates_missing_database() {
        let path = riir_db_path("missing");

        let database = RiirDatabase::file_backed(&path)
            .await
            .expect("missing RIIR database should be initialized");
        let row = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind("SERIES_METADATA_CONTRIBUTION")
            .fetch_one(database.read_pool())
            .await
            .expect("contribution table should exist");

        assert_eq!(row.get::<String, _>("name"), "SERIES_METADATA_CONTRIBUTION");
        database.close().await;
        let _ = std::fs::remove_dir_all(path.parent().expect("RIIR path should have a parent"));
    }
}
