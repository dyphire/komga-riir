use crate::opds::{
    OpdsBookAuthorEntry, OpdsBookFeedEntry, OpdsCollectionDetailPersistedPort,
    OpdsCollectionVisibilityPersistedPort, OpdsFeedUserContext, OpdsLibraryPersistedPort,
    OpdsPublisherPersistedPort, OpdsReadlistDetailPersistedPort,
    OpdsReadlistVisibilityPersistedPort, OpdsSearchPersistedPort, OpdsSeriesPersistedPort,
    PersistedBookSearchRecord, PersistedLibraryRecord, PersistedNamedRecord,
    PersistedReadlistBookRecord, PersistedReadlistRecord, PersistedSeriesBookRecord,
    PersistedSeriesRecord, PersistedSeriesSearchRecord,
};
use komga_domain::discovery::MediaStatus;

#[derive(Debug)]
pub enum OpdsLibraryScopeError {
    Load(anyhow::Error),
    NotFound,
    Forbidden,
}

#[cfg(test)]
impl PartialEq for OpdsLibraryScopeError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Load(left), Self::Load(right)) => left.to_string() == right.to_string(),
            (Self::NotFound, Self::NotFound) | (Self::Forbidden, Self::Forbidden) => true,
            _ => false,
        }
    }
}

#[derive(Debug)]
pub enum OpdsSeriesAccessError {
    Load(anyhow::Error),
    NotFound,
    Forbidden,
}

#[cfg(test)]
impl PartialEq for OpdsSeriesAccessError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Load(left), Self::Load(right)) => left.to_string() == right.to_string(),
            (Self::NotFound, Self::NotFound) | (Self::Forbidden, Self::Forbidden) => true,
            _ => false,
        }
    }
}

pub struct OpdsReadlistDetail {
    pub readlist: PersistedReadlistRecord,
    pub books: Vec<PersistedReadlistBookRecord>,
}

pub struct OpdsCollectionDetail {
    pub collection: PersistedNamedRecord,
    pub series: Vec<PersistedSeriesRecord>,
}

pub struct OpdsUnifiedSearchResults {
    pub series: Vec<PersistedSeriesSearchRecord>,
    pub books: Vec<PersistedBookSearchRecord>,
    pub collections: Vec<PersistedNamedRecord>,
    pub readlists: Vec<PersistedNamedRecord>,
}

pub struct OpdsPersistedService<'a, P: ?Sized> {
    persisted: &'a P,
}

impl<'a, P: ?Sized> OpdsPersistedService<'a, P> {
    pub fn new(persisted: &'a P) -> Self {
        Self { persisted }
    }
}

impl<'a, P> OpdsPersistedService<'a, P>
where
    P: OpdsLibraryPersistedPort + ?Sized,
{
    pub async fn libraries(&self) -> anyhow::Result<Vec<PersistedLibraryRecord>> {
        self.persisted.load_libraries().await
    }

    pub async fn visible_libraries(
        &self,
        user: &OpdsFeedUserContext,
    ) -> anyhow::Result<Vec<PersistedLibraryRecord>> {
        Ok(self
            .libraries()
            .await?
            .into_iter()
            .filter(|library| user.can_access_library(&library.id))
            .collect())
    }

    pub async fn library(
        &self,
        library_id: &str,
    ) -> anyhow::Result<Option<PersistedLibraryRecord>> {
        self.persisted.load_library(library_id).await
    }

    pub async fn visible_library_scope(
        &self,
        user: &OpdsFeedUserContext,
        library_id: Option<&str>,
    ) -> Result<Option<PersistedLibraryRecord>, OpdsLibraryScopeError> {
        let Some(library_id) = library_id else {
            return Ok(None);
        };

        let library = self
            .library(library_id)
            .await
            .map_err(OpdsLibraryScopeError::Load)?
            .ok_or(OpdsLibraryScopeError::NotFound)?;

        if !user.can_access_library(library_id) {
            return Err(OpdsLibraryScopeError::Forbidden);
        }

        Ok(Some(library))
    }
}

impl<'a, P> OpdsPersistedService<'a, P>
where
    P: OpdsPublisherPersistedPort + ?Sized,
{
    pub async fn publishers(&self, user: &OpdsFeedUserContext) -> anyhow::Result<Vec<String>> {
        self.persisted
            .load_publishers(user.allowed_library_ids())
            .await
    }
}

impl<'a, P> OpdsPersistedService<'a, P>
where
    P: OpdsCollectionVisibilityPersistedPort + ?Sized,
{
    pub async fn all_collections(
        &self,
        user: &OpdsFeedUserContext,
        library_id: Option<&str>,
        keep_empty_global_collections: bool,
    ) -> anyhow::Result<Vec<PersistedNamedRecord>> {
        let collections = self.persisted.load_collections(library_id).await?;
        let mut visible_collections = Vec::new();

        for collection in collections {
            let series = self
                .persisted
                .load_collection_series(&collection.id, collection.ordered)
                .await?;
            // Kotlin's findAll(libs, libs) keeps a collection when at least one
            // of its series belongs to an authorized library; content
            // restrictions are applied only at the entry (series) level, not
            // to the collection itself.
            let has_library_visible_series = series
                .iter()
                .any(|series| user.can_access_library(&series.library_id));
            let keep_empty = library_id.is_none()
                && keep_empty_global_collections
                && series.is_empty()
                && user.allowed_library_ids().is_none();

            if has_library_visible_series || keep_empty {
                visible_collections.push(collection);
            }
        }

        Ok(visible_collections)
    }

    pub async fn has_visible_collections_for_scope(
        &self,
        user: &OpdsFeedUserContext,
        library_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        let collections = self.persisted.load_collections(library_id).await?;

        for collection in collections {
            let series = self
                .persisted
                .load_collection_series(&collection.id, collection.ordered)
                .await?;
            if series.iter().any(|series| user.can_access_library(&series.library_id)) {
                return Ok(true);
            }
        }

        Ok(false)
    }
}

impl<'a, P> OpdsPersistedService<'a, P>
where
    P: OpdsCollectionDetailPersistedPort + ?Sized,
{
    pub async fn collection_detail(
        &self,
        user: &OpdsFeedUserContext,
        collection_id: &str,
    ) -> anyhow::Result<Option<OpdsCollectionDetail>> {
        let Some(collection) = self.persisted.load_collection(collection_id).await? else {
            return Ok(None);
        };

        let all_series = self
            .persisted
            .load_collection_series(collection_id, collection.ordered)
            .await?;

        // Kotlin's findByIdOrNull(id, libs) returns 404 only when no series of
        // the collection belongs to an authorized library; otherwise the
        // detail is returned even when every series is filtered out by
        // content restrictions (empty feed, HTTP 200).
        if !all_series
            .iter()
            .any(|series| user.can_access_library(&series.library_id))
        {
            return Ok(None);
        }

        let series = all_series
            .into_iter()
            .filter(|series| series_is_visible(user, series))
            .collect();

        Ok(Some(OpdsCollectionDetail { collection, series }))
    }

    pub async fn collection_books(
        &self,
        collection_id: &str,
    ) -> anyhow::Result<Vec<crate::opds::PersistedBookFeedRecord>> {
        self.persisted.load_collection_books(collection_id).await
    }
}

impl<'a, P> OpdsPersistedService<'a, P>
where
    P: OpdsReadlistVisibilityPersistedPort + ?Sized,
{
    pub async fn all_readlists(
        &self,
        user: &OpdsFeedUserContext,
        library_id: Option<&str>,
    ) -> anyhow::Result<Vec<PersistedReadlistRecord>> {
        if let Some(library_id) = library_id {
            return self.persisted.load_readlists_for_library(library_id).await;
        }

        let readlists = self.persisted.load_all_readlists().await?;
        let mut visible_readlists = Vec::new();

        for readlist in readlists {
            let books = self.persisted.load_readlist_books(&readlist.id).await?;
            // Kotlin keeps a read list when at least one of its books belongs
            // to an authorized library; content restrictions apply only at
            // the entry (book) level.
            if books
                .iter()
                .any(|book| user.can_access_library(&book.library_id))
            {
                visible_readlists.push(readlist);
            }
        }

        Ok(visible_readlists)
    }

    pub async fn has_visible_readlists_for_scope(
        &self,
        user: &OpdsFeedUserContext,
        library_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        let readlists = match library_id {
            Some(library_id) => self.persisted.load_readlists_for_library(library_id).await,
            None => self.persisted.load_all_readlists().await,
        }?;

        for readlist in readlists {
            let books = self.persisted.load_readlist_books(&readlist.id).await?;
            if books
                .iter()
                .any(|book| user.can_access_library(&book.library_id))
            {
                return Ok(true);
            }
        }

        Ok(false)
    }
}

impl<'a, P> OpdsPersistedService<'a, P>
where
    P: OpdsReadlistDetailPersistedPort + ?Sized,
{
    pub async fn readlist_detail(
        &self,
        user: &OpdsFeedUserContext,
        readlist_id: &str,
    ) -> anyhow::Result<Option<OpdsReadlistDetail>> {
        let Some(readlist) = self.persisted.load_readlist(readlist_id).await? else {
            return Ok(None);
        };

        let all_books = self
            .persisted
            .load_readlist_books(readlist_id)
            .await?;

        // Kotlin's findByIdOrNull(id, libs) returns 404 only when no book of
        // the read list belongs to an authorized library; otherwise the
        // detail is returned even when every book is filtered out by content
        // restrictions (empty feed, HTTP 200).
        if !all_books
            .iter()
            .any(|book| user.can_access_library(&book.library_id))
        {
            return Ok(None);
        }

        let mut books = all_books
            .into_iter()
            .filter(|book| {
                user.can_access_library(&book.library_id)
                    && book.media_status == Some(MediaStatus::Ready)
                    && user.content_allowed(book.age_rating, &book.sharing_labels)
            })
            .collect::<Vec<_>>();
        if !readlist.ordered {
            books.sort_by_key(|book| book.release_date.clone());
        }

        Ok(Some(OpdsReadlistDetail { readlist, books }))
    }
}

impl<'a, P> OpdsPersistedService<'a, P>
where
    P: OpdsSeriesPersistedPort + ?Sized,
{
    pub async fn series(
        &self,
        user: &OpdsFeedUserContext,
        series_id: &str,
    ) -> anyhow::Result<Option<PersistedSeriesRecord>> {
        Ok(self
            .persisted
            .load_series(series_id)
            .await?
            .filter(|series| series_is_visible(user, series)))
    }

    pub async fn visible_series(
        &self,
        user: &OpdsFeedUserContext,
        series_id: &str,
    ) -> Result<PersistedSeriesRecord, OpdsSeriesAccessError> {
        let series = self
            .persisted
            .load_series(series_id)
            .await
            .map_err(OpdsSeriesAccessError::Load)?
            .ok_or(OpdsSeriesAccessError::NotFound)?;

        if !series_is_visible(user, &series) {
            return Err(OpdsSeriesAccessError::Forbidden);
        }

        Ok(series)
    }

    pub async fn series_books_page(
        &self,
        user: &OpdsFeedUserContext,
        series_id: &str,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> anyhow::Result<Vec<PersistedSeriesBookRecord>> {
        Ok(self
            .persisted
            .load_series_books_paged(series_id, user_id, offset, limit)
            .await?
            .into_iter()
            .filter(|book| {
                user.can_access_library(&book.library_id)
                    && user.content_allowed(book.age_rating, &book.sharing_labels)
            })
            .collect())
    }

    pub async fn series_tags(&self, series_id: &str) -> anyhow::Result<Vec<String>> {
        self.persisted.load_series_tags(series_id).await
    }
}

impl<'a, P> OpdsPersistedService<'a, P>
where
    P: OpdsSearchPersistedPort + ?Sized,
{
    pub async fn unified_search(
        &self,
        user: &OpdsFeedUserContext,
        query: &str,
    ) -> anyhow::Result<OpdsUnifiedSearchResults> {
        let records = self.persisted.load_unified_search_results(query).await?;

        let mut visible_series = records
            .series
            .into_iter()
            .filter(|series| {
                user.can_access_library(&series.library_id)
                    && user.content_allowed(series.age_rating, &series.sharing_labels)
            })
            .collect::<Vec<_>>();
        visible_series.truncate(20);

        let mut visible_books = records
            .books
            .into_iter()
            .filter(|book| {
                user.can_access_library(&book.library_id)
                    && user.content_allowed(book.age_rating, &book.sharing_labels)
            })
            .collect::<Vec<_>>();
        visible_books.truncate(20);

        let mut visible_collections = Vec::new();
        for collection in records.collections {
            let books = self.persisted.load_collection_books(&collection.id).await?;
            if books.iter().any(|book| {
                user.can_access_library(&book.library_id)
                    && user.content_allowed(book.age_rating, &book.sharing_labels)
            }) {
                visible_collections.push(collection);
            }
            if visible_collections.len() >= 20 {
                break;
            }
        }

        let mut visible_readlists = Vec::new();
        for readlist in records.readlists {
            let books = self.persisted.load_readlist_books(&readlist.id).await?;
            if books
                .iter()
                .any(|book| readlist_book_is_visible(user, book))
            {
                visible_readlists.push(readlist);
            }
            if visible_readlists.len() >= 20 {
                break;
            }
        }

        Ok(OpdsUnifiedSearchResults {
            series: visible_series,
            books: visible_books,
            collections: visible_collections,
            readlists: visible_readlists,
        })
    }
}

impl From<PersistedSeriesBookRecord> for OpdsBookFeedEntry {
    fn from(book: PersistedSeriesBookRecord) -> Self {
        Self {
            id: book.id,
            series_id: book.series_id,
            title: book.title,
            series_title: book.series_title,
            number: book.number,
            number_sort: book.number_sort,
            summary: book.summary,
            isbn: book.isbn,
            authors: book
                .authors
                .into_iter()
                .map(|author| OpdsBookAuthorEntry {
                    name: author.name,
                    role: author.role,
                })
                .collect(),
            tags: book.tags,
            file_name: book.file_name,
            file_size: book.file_size,
            media_type: book.media_type,
            page_count: book.page_count,
            epub_divina_compatible: book.epub_divina_compatible,
            last_read: book.last_read,
            last_read_date: book.last_read_date,
            library_id: book.library_id,
            age_rating: book.age_rating,
            sharing_labels: book.sharing_labels,
            last_modified: book.last_modified,
            release_date: book.release_date,
        }
    }
}

impl From<PersistedReadlistBookRecord> for OpdsBookFeedEntry {
    fn from(book: PersistedReadlistBookRecord) -> Self {
        Self {
            id: book.id,
            series_id: book.series_id,
            title: book.title,
            series_title: book.series_title,
            number: book.number,
            number_sort: book.number_sort,
            summary: book.summary,
            isbn: book.isbn,
            authors: book
                .authors
                .into_iter()
                .map(|author| OpdsBookAuthorEntry {
                    name: author.name,
                    role: author.role,
                })
                .collect(),
            tags: book.tags,
            file_name: book.file_name,
            file_size: book.file_size,
            media_type: book.media_type,
            page_count: book.page_count,
            epub_divina_compatible: book.epub_divina_compatible,
            last_read: None,
            last_read_date: None,
            library_id: book.library_id,
            age_rating: book.age_rating,
            sharing_labels: book.sharing_labels,
            last_modified: book.last_modified,
            release_date: book.release_date,
        }
    }
}

impl From<PersistedBookSearchRecord> for OpdsBookFeedEntry {
    fn from(book: PersistedBookSearchRecord) -> Self {
        Self {
            id: book.id,
            series_id: book.series_id,
            title: book.title,
            series_title: book.series_title,
            number: book.number,
            number_sort: book.number_sort,
            summary: book.summary,
            isbn: book.isbn,
            authors: book
                .authors
                .into_iter()
                .map(|author| OpdsBookAuthorEntry {
                    name: author.name,
                    role: author.role,
                })
                .collect(),
            tags: book.tags,
            file_name: book.file_name,
            file_size: book.file_size,
            media_type: book.media_type,
            page_count: book.page_count,
            epub_divina_compatible: book.epub_divina_compatible,
            last_read: None,
            last_read_date: None,
            library_id: book.library_id,
            age_rating: book.age_rating,
            sharing_labels: book.sharing_labels,
            last_modified: book.last_modified,
            release_date: book.release_date,
        }
    }
}

fn series_is_visible(user: &OpdsFeedUserContext, series: &PersistedSeriesRecord) -> bool {
    user.can_access_library(&series.library_id)
        && user.content_allowed(series.age_rating, &series.sharing_labels)
}

fn readlist_book_is_visible(
    user: &OpdsFeedUserContext,
    book: &PersistedReadlistBookRecord,
) -> bool {
    user.can_access_library(&book.library_id)
        && user.content_allowed(book.age_rating, &book.sharing_labels)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use komga_domain::discovery::{AgeRestrictionKind, QueryRestrictions};

    use super::*;
    use crate::opds::{
        OpdsCollectionVisibilityPersistedPort, OpdsPersistedUnifiedSearchRecords,
        OpdsReadlistDetailPersistedPort, OpdsReadlistVisibilityPersistedPort,
        OpdsSearchPersistedPort, OpdsSeriesPersistedPort, PersistedBookFeedRecord,
    };
    #[derive(Default)]
    struct TestPersistedPort {
        series: HashMap<String, PersistedSeriesRecord>,
        series_books: HashMap<String, Vec<PersistedSeriesBookRecord>>,
        readlists: HashMap<String, PersistedReadlistRecord>,
        all_readlists: Vec<PersistedReadlistRecord>,
        readlist_books: HashMap<String, Vec<PersistedReadlistBookRecord>>,
        readlist_book_errors: HashSet<String>,
        collections: Vec<PersistedNamedRecord>,
        collection_series: HashMap<String, Vec<PersistedSeriesRecord>>,
        collection_series_errors: HashSet<String>,
        search_series: Vec<PersistedSeriesSearchRecord>,
        search_books: Vec<PersistedBookSearchRecord>,
        search_collections: Vec<PersistedNamedRecord>,
        search_readlists: Vec<PersistedNamedRecord>,
        collection_books: HashMap<String, Vec<PersistedBookFeedRecord>>,
        collection_book_errors: HashSet<String>,
    }

    impl TestPersistedPort {
        fn readlist_books_for(&self, readlist_id: &str) -> Vec<PersistedReadlistBookRecord> {
            self.readlist_books
                .get(readlist_id)
                .cloned()
                .unwrap_or_default()
        }

        fn load_readlist_books_for(
            &self,
            readlist_id: &str,
        ) -> anyhow::Result<Vec<PersistedReadlistBookRecord>> {
            if self.readlist_book_errors.contains(readlist_id) {
                return Err(anyhow::anyhow!(format!(
                    "load readlist books {readlist_id}"
                )));
            }

            Ok(self.readlist_books_for(readlist_id))
        }

        fn collection_series_for(&self, collection_id: &str) -> Vec<PersistedSeriesRecord> {
            self.collection_series
                .get(collection_id)
                .cloned()
                .unwrap_or_default()
        }

        fn load_collection_series_for(
            &self,
            collection_id: &str,
        ) -> anyhow::Result<Vec<PersistedSeriesRecord>> {
            if self.collection_series_errors.contains(collection_id) {
                return Err(anyhow::anyhow!(format!(
                    "load collection series {collection_id}"
                )));
            }

            Ok(self.collection_series_for(collection_id))
        }

        fn collection_books_for(&self, collection_id: &str) -> Vec<PersistedBookFeedRecord> {
            self.collection_books
                .get(collection_id)
                .cloned()
                .unwrap_or_default()
        }

        fn load_collection_books_for(
            &self,
            collection_id: &str,
        ) -> anyhow::Result<Vec<PersistedBookFeedRecord>> {
            if self.collection_book_errors.contains(collection_id) {
                return Err(anyhow::anyhow!(format!(
                    "load collection books {collection_id}"
                )));
            }

            Ok(self.collection_books_for(collection_id))
        }
    }

    #[async_trait::async_trait]
    impl OpdsCollectionVisibilityPersistedPort for TestPersistedPort {
        async fn load_collections(
            &self,
            _library_id: Option<&str>,
        ) -> anyhow::Result<Vec<PersistedNamedRecord>> {
            Ok(self.collections.clone())
        }

        async fn load_collection_series(
            &self,
            collection_id: &str,
            _ordered: bool,
        ) -> anyhow::Result<Vec<PersistedSeriesRecord>> {
            self.load_collection_series_for(collection_id)
        }
    }

    #[async_trait::async_trait]
    impl OpdsReadlistVisibilityPersistedPort for TestPersistedPort {
        async fn load_readlists_for_library(
            &self,
            _library_id: &str,
        ) -> anyhow::Result<Vec<PersistedReadlistRecord>> {
            Ok(Vec::new())
        }

        async fn load_all_readlists(&self) -> anyhow::Result<Vec<PersistedReadlistRecord>> {
            Ok(self.all_readlists.clone())
        }

        async fn load_readlist_books(
            &self,
            readlist_id: &str,
        ) -> anyhow::Result<Vec<PersistedReadlistBookRecord>> {
            self.load_readlist_books_for(readlist_id)
        }
    }

    #[async_trait::async_trait]
    impl OpdsReadlistDetailPersistedPort for TestPersistedPort {
        async fn load_readlist(
            &self,
            readlist_id: &str,
        ) -> anyhow::Result<Option<PersistedReadlistRecord>> {
            Ok(self.readlists.get(readlist_id).cloned())
        }

        async fn load_readlist_books(
            &self,
            readlist_id: &str,
        ) -> anyhow::Result<Vec<PersistedReadlistBookRecord>> {
            self.load_readlist_books_for(readlist_id)
        }
    }

    #[async_trait::async_trait]
    impl OpdsSeriesPersistedPort for TestPersistedPort {
        async fn load_series(
            &self,
            series_id: &str,
        ) -> anyhow::Result<Option<PersistedSeriesRecord>> {
            Ok(self.series.get(series_id).cloned())
        }

        async fn load_series_books_paged(
            &self,
            series_id: &str,
            _user_id: &str,
            _offset: i64,
            _limit: i64,
        ) -> anyhow::Result<Vec<PersistedSeriesBookRecord>> {
            Ok(self
                .series_books
                .get(series_id)
                .cloned()
                .unwrap_or_default())
        }

        async fn load_series_tags(&self, _series_id: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl OpdsSearchPersistedPort for TestPersistedPort {
        async fn load_unified_search_results(
            &self,
            _query: &str,
        ) -> anyhow::Result<OpdsPersistedUnifiedSearchRecords> {
            Ok(OpdsPersistedUnifiedSearchRecords {
                series: self.search_series.clone(),
                books: self.search_books.clone(),
                collections: self.search_collections.clone(),
                readlists: self.search_readlists.clone(),
            })
        }

        async fn load_collection_books(
            &self,
            collection_id: &str,
        ) -> anyhow::Result<Vec<PersistedBookFeedRecord>> {
            self.load_collection_books_for(collection_id)
        }

        async fn load_readlist_books(
            &self,
            readlist_id: &str,
        ) -> anyhow::Result<Vec<PersistedReadlistBookRecord>> {
            self.load_readlist_books_for(readlist_id)
        }
    }

    #[tokio::test]
    async fn readlist_detail_keeps_visible_scope_when_books_fail_content_filters() {
        let readlist = readlist("readlist-a", true);
        let port = TestPersistedPort {
            readlists: HashMap::from([(readlist.id.clone(), readlist)]),
            readlist_books: HashMap::from([(
                "readlist-a".to_string(),
                vec![readlist_book("book-a", "lib-a", Some(18), &["adult"])],
            )]),
            ..Default::default()
        };
        let service = OpdsPersistedService::new(&port);
        let user = restricted_user();

        let detail = service
            .readlist_detail(&user, "readlist-a")
            .await
            .expect("readlist detail should load")
            .expect("library-visible readlist should remain visible");

        assert_eq!(detail.readlist.id, "readlist-a");
        assert!(detail.books.is_empty());
    }

    #[tokio::test]
    async fn readlist_detail_keeps_only_media_ready_books() {
        let readlist = readlist("readlist-a", true);
        let ready_book = readlist_book("book-ready", "lib-a", Some(12), &["kids"]);
        let mut failed_book = readlist_book("book-error", "lib-a", Some(12), &["kids"]);
        failed_book.media_status = Some(MediaStatus::Error);
        let port = TestPersistedPort {
            readlists: HashMap::from([(readlist.id.clone(), readlist)]),
            readlist_books: HashMap::from([(
                "readlist-a".to_string(),
                vec![ready_book, failed_book],
            )]),
            ..Default::default()
        };
        let service = OpdsPersistedService::new(&port);

        let detail = service
            .readlist_detail(&restricted_user(), "readlist-a")
            .await
            .expect("readlist detail should load")
            .expect("readlist should be visible");

        assert_eq!(
            detail
                .books
                .iter()
                .map(|book| book.id.as_str())
                .collect::<Vec<_>>(),
            vec!["book-ready"]
        );
    }

    #[tokio::test]
    async fn all_readlists_uses_library_visibility_without_content_filtering() {
        let readlist = readlist("readlist-a", false);
        let port = TestPersistedPort {
            all_readlists: vec![readlist.clone()],
            readlist_books: HashMap::from([(
                readlist.id.clone(),
                vec![readlist_book("book-a", "lib-a", Some(18), &["adult"])],
            )]),
            ..Default::default()
        };
        let service = OpdsPersistedService::new(&port);
        let user = restricted_user();

        let readlists = service
            .all_readlists(&user, None)
            .await
            .expect("readlists should load");

        assert_eq!(
            readlists
                .iter()
                .map(|readlist| readlist.id.as_str())
                .collect::<Vec<_>>(),
            vec!["readlist-a"]
        );
    }

    #[tokio::test]
    async fn collection_visibility_propagates_series_load_errors() {
        let port = TestPersistedPort {
            collections: vec![named("collection-a")],
            collection_series_errors: HashSet::from(["collection-a".to_string()]),
            ..Default::default()
        };
        let service = OpdsPersistedService::new(&port);
        let user = restricted_user();

        assert_error_contains(
            service.all_collections(&user, None, true).await,
            "collection-a",
            "collection series load errors should propagate",
        );
        assert_error_contains(
            service.has_visible_collections_for_scope(&user, None).await,
            "collection-a",
            "collection scope series load errors should propagate",
        );
    }

    #[tokio::test]
    async fn readlist_visibility_propagates_book_load_errors() {
        let port = TestPersistedPort {
            all_readlists: vec![readlist("readlist-a", false)],
            readlist_book_errors: HashSet::from(["readlist-a".to_string()]),
            ..Default::default()
        };
        let service = OpdsPersistedService::new(&port);
        let user = restricted_user();

        assert_error_contains(
            service.all_readlists(&user, None).await,
            "readlist-a",
            "readlist book load errors should propagate",
        );
        assert_error_contains(
            service.has_visible_readlists_for_scope(&user, None).await,
            "readlist-a",
            "readlist scope book load errors should propagate",
        );
    }

    #[tokio::test]
    async fn unified_search_filters_relation_results_by_visible_content() {
        let port = TestPersistedPort {
            search_series: vec![
                series_search("series-a", "lib-a", Some(12), &["kids"]),
                series_search("series-b", "lib-b", Some(12), &["kids"]),
            ],
            search_books: vec![
                book_search("book-a", "lib-a", Some(12), &["kids"]),
                book_search("book-b", "lib-a", Some(18), &["adult"]),
            ],
            search_collections: vec![
                named("collection-a"),
                named("collection-b"),
                named("collection-c"),
            ],
            search_readlists: vec![
                named("readlist-a"),
                named("readlist-b"),
                named("readlist-c"),
            ],
            collection_books: HashMap::from([
                (
                    "collection-a".to_string(),
                    vec![collection_book("book-a", "lib-a", Some(12), &["kids"])],
                ),
                (
                    "collection-b".to_string(),
                    vec![collection_book("book-b", "lib-a", Some(18), &["adult"])],
                ),
                (
                    "collection-c".to_string(),
                    vec![collection_book("book-c", "lib-b", Some(12), &["kids"])],
                ),
            ]),
            readlist_books: HashMap::from([
                (
                    "readlist-a".to_string(),
                    vec![readlist_book("book-a", "lib-a", Some(12), &["kids"])],
                ),
                (
                    "readlist-b".to_string(),
                    vec![readlist_book("book-b", "lib-a", Some(18), &["adult"])],
                ),
                (
                    "readlist-c".to_string(),
                    vec![readlist_book("book-c", "lib-b", Some(12), &["kids"])],
                ),
            ]),
            ..Default::default()
        };
        let service = OpdsPersistedService::new(&port);
        let user = restricted_user();

        let results = service
            .unified_search(&user, "term")
            .await
            .expect("search results should load");

        assert_eq!(
            results
                .series
                .iter()
                .map(|series| series.id.as_str())
                .collect::<Vec<_>>(),
            vec!["series-a"]
        );
        assert_eq!(
            results
                .books
                .iter()
                .map(|book| book.id.as_str())
                .collect::<Vec<_>>(),
            vec!["book-a"]
        );
        assert_eq!(
            results
                .collections
                .iter()
                .map(|collection| collection.id.as_str())
                .collect::<Vec<_>>(),
            vec!["collection-a"]
        );
        assert_eq!(
            results
                .readlists
                .iter()
                .map(|readlist| readlist.id.as_str())
                .collect::<Vec<_>>(),
            vec!["readlist-a"]
        );
    }

    #[tokio::test]
    async fn unified_search_propagates_relation_load_errors() {
        let collection_port = TestPersistedPort {
            search_collections: vec![named("collection-a")],
            collection_book_errors: HashSet::from(["collection-a".to_string()]),
            ..Default::default()
        };
        let service = OpdsPersistedService::new(&collection_port);
        assert_error_contains(
            service.unified_search(&restricted_user(), "term").await,
            "collection-a",
            "collection candidate book load errors should propagate",
        );

        let readlist_port = TestPersistedPort {
            search_readlists: vec![named("readlist-a")],
            readlist_book_errors: HashSet::from(["readlist-a".to_string()]),
            ..Default::default()
        };
        let service = OpdsPersistedService::new(&readlist_port);
        assert_error_contains(
            service.unified_search(&restricted_user(), "term").await,
            "readlist-a",
            "readlist candidate book load errors should propagate",
        );
    }

    #[tokio::test]
    async fn visible_series_reports_forbidden_for_content_hidden_series() {
        let port = TestPersistedPort {
            series: HashMap::from([(
                "series-a".to_string(),
                series("series-a", "lib-a", Some(18), &["adult"]),
            )]),
            ..Default::default()
        };
        let service = OpdsPersistedService::new(&port);

        let result = service.visible_series(&restricted_user(), "series-a").await;
        let Err(error) = result else {
            panic!("content-hidden series should be forbidden");
        };

        assert_eq!(error, OpdsSeriesAccessError::Forbidden);
    }

    #[tokio::test]
    async fn series_books_page_filters_books_by_library_and_content_rules() {
        let port = TestPersistedPort {
            series_books: HashMap::from([(
                "series-a".to_string(),
                vec![
                    series_book("book-a", "lib-a", Some(12), &["kids"]),
                    series_book("book-b", "lib-a", Some(18), &["adult"]),
                    series_book("book-c", "lib-b", Some(12), &["kids"]),
                ],
            )]),
            ..Default::default()
        };
        let service = OpdsPersistedService::new(&port);

        let books = service
            .series_books_page(&restricted_user(), "series-a", "user-a", 0, 10)
            .await
            .expect("series books should load");

        assert_eq!(
            books
                .iter()
                .map(|book| book.id.as_str())
                .collect::<Vec<_>>(),
            vec!["book-a"]
        );
    }

    fn restricted_user() -> OpdsFeedUserContext {
        OpdsFeedUserContext {
            user_id: "user-a".to_string(),
            allowed_library_ids: Some(HashSet::from(["lib-a".to_string()])),
            restrictions: QueryRestrictions {
                age: Some(15),
                age_restriction: Some(AgeRestrictionKind::AllowOnly),
                labels_allow: vec!["kids".to_string()],
                labels_exclude: vec!["adult".to_string()],
            },
        }
    }

    fn assert_error_contains<T>(result: anyhow::Result<T>, expected: &str, context: &str) {
        let Err(error) = result else {
            panic!("{context}");
        };
        assert!(error.to_string().contains(expected), "{error}");
    }

    fn readlist(id: &str, ordered: bool) -> PersistedReadlistRecord {
        PersistedReadlistRecord {
            id: id.to_string(),
            name: id.to_string(),
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            ordered,
        }
    }

    fn named(id: &str) -> PersistedNamedRecord {
        PersistedNamedRecord {
            id: id.to_string(),
            name: id.to_string(),
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            ordered: false,
        }
    }

    fn series_search(
        id: &str,
        library_id: &str,
        age_rating: Option<u32>,
        sharing_labels: &[&str],
    ) -> PersistedSeriesSearchRecord {
        PersistedSeriesSearchRecord {
            id: id.to_string(),
            title: id.to_string(),
            library_id: library_id.to_string(),
            age_rating,
            sharing_labels: labels(sharing_labels),
            last_modified: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn series(
        id: &str,
        library_id: &str,
        age_rating: Option<u32>,
        sharing_labels: &[&str],
    ) -> PersistedSeriesRecord {
        PersistedSeriesRecord {
            id: id.to_string(),
            library_id: library_id.to_string(),
            title: id.to_string(),
            summary: String::new(),
            age_rating,
            sharing_labels: labels(sharing_labels),
            last_modified: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn series_book(
        id: &str,
        library_id: &str,
        age_rating: Option<u32>,
        sharing_labels: &[&str],
    ) -> PersistedSeriesBookRecord {
        PersistedSeriesBookRecord {
            id: id.to_string(),
            series_id: "series-a".to_string(),
            title: id.to_string(),
            series_title: "Series".to_string(),
            number: String::new(),
            number_sort: 0.0,
            summary: String::new(),
            isbn: None,
            authors: Vec::new(),
            tags: Vec::new(),
            file_name: format!("{id}.epub"),
            file_size: 1,
            media_type: "application/epub+zip".to_string(),
            media_status: Some(MediaStatus::Ready),
            page_count: 1,
            epub_divina_compatible: false,
            last_read: None,
            last_read_date: None,
            library_id: library_id.to_string(),
            age_rating,
            sharing_labels: labels(sharing_labels),
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            release_date: None,
        }
    }

    fn book_search(
        id: &str,
        library_id: &str,
        age_rating: Option<u32>,
        sharing_labels: &[&str],
    ) -> PersistedBookSearchRecord {
        PersistedBookSearchRecord {
            id: id.to_string(),
            series_id: "series-a".to_string(),
            title: id.to_string(),
            series_title: "Series".to_string(),
            number: String::new(),
            number_sort: 0.0,
            summary: String::new(),
            isbn: None,
            authors: Vec::new(),
            tags: Vec::new(),
            file_name: format!("{id}.epub"),
            file_size: 1,
            media_type: "application/epub+zip".to_string(),
            page_count: 1,
            epub_divina_compatible: false,
            library_id: library_id.to_string(),
            age_rating,
            sharing_labels: labels(sharing_labels),
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            release_date: None,
        }
    }

    fn readlist_book(
        id: &str,
        library_id: &str,
        age_rating: Option<u32>,
        sharing_labels: &[&str],
    ) -> PersistedReadlistBookRecord {
        PersistedReadlistBookRecord {
            id: id.to_string(),
            series_id: "series-a".to_string(),
            title: id.to_string(),
            series_title: "Series".to_string(),
            number: String::new(),
            number_sort: 0.0,
            summary: String::new(),
            isbn: None,
            authors: Vec::new(),
            tags: Vec::new(),
            file_name: format!("{id}.epub"),
            file_size: 1,
            media_type: "application/epub+zip".to_string(),
            media_status: Some(MediaStatus::Ready),
            page_count: 1,
            epub_divina_compatible: false,
            library_id: library_id.to_string(),
            age_rating,
            sharing_labels: labels(sharing_labels),
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            release_date: None,
        }
    }

    fn collection_book(
        id: &str,
        library_id: &str,
        age_rating: Option<u32>,
        sharing_labels: &[&str],
    ) -> PersistedBookFeedRecord {
        PersistedBookFeedRecord {
            id: id.to_string(),
            title: id.to_string(),
            file_name: format!("{id}.epub"),
            media_type: "application/epub+zip".to_string(),
            library_id: library_id.to_string(),
            age_rating,
            sharing_labels: labels(sharing_labels),
            last_modified: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }
}
