use super::persistence::{
    AnalyzedBookMedia, AnalyzedBookMediaFile, AnalyzedBookPage, analyze_book_input,
    persist_book_analysis,
};
use crate::MediaLibraryJobContext;
use crate::analysis::analyze_book_media_file_with_sources;
use crate::maintenance::updates::adjust_analyzed_book_read_progress;
use komga_application::runtime_sse::RuntimeSseEvent;
use komga_application::task_processing::TaskProcessingError;
use komga_domain::discovery::MediaStatus;
use komga_infrastructure_base::resolve_library_item_path;
use komga_infrastructure_media_core::content::metadata_sources::CapturedMetadataSources;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalyzeBookOutcome {
    pub series_id: String,
    pub media_status: Option<MediaStatus>,
    pub metadata_sources: CapturedMetadataSources,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookAnalysisPurpose {
    AnalysisOnly,
    AnalysisAndMetadata,
}

pub async fn analyze_book(
    runtime: &MediaLibraryJobContext,
    book_id: &str,
    purpose: BookAnalysisPurpose,
) -> Result<AnalyzeBookOutcome, TaskProcessingError> {
    let book_id = book_id.to_string();
    if !runtime.database().owns_main_database() {
        return Ok(AnalyzeBookOutcome {
            series_id: String::new(),
            media_status: None,
            metadata_sources: CapturedMetadataSources::default(),
        });
    }

    let Some(input) = analyze_book_input(runtime.database().task_read_pool(), &book_id)
        .await
        .map_err(TaskProcessingError::runtime)?
    else {
        return Ok(AnalyzeBookOutcome {
            series_id: String::new(),
            media_status: None,
            metadata_sources: CapturedMetadataSources::default(),
        });
    };

    let file_path = resolve_library_item_path(&input.root, &input.url);
    let metadata_sources = match purpose {
        BookAnalysisPurpose::AnalysisOnly => Default::default(),
        BookAnalysisPurpose::AnalysisAndMetadata => input.metadata_sources,
    };
    let analysis = analyze_book_media_file_with_sources(
        &file_path,
        input.analyze_dimensions,
        input.hash_pages,
        metadata_sources,
    )
    .map_err(|error| {
        TaskProcessingError::runtime(format!(
            "failed to analyze media file for '{book_id}' ('{}'): {error}",
            file_path.display(),
        ))
    })?;

    let persisted = AnalyzedBookMedia {
        status: analysis.status,
        media_type: analysis.media_type,
        comment: analysis.comment,
        page_count: analysis.page_count,
        epub_divina_compatible: analysis.epub_divina_compatible,
        epub_is_kepub: analysis.epub_is_kepub,
        pages: analysis
            .pages
            .into_iter()
            .map(|page| AnalyzedBookPage {
                file_name: page.file_name,
                media_type: page.media_type,
                width: page.width,
                height: page.height,
                file_size: page.file_size,
                file_hash: page.file_hash,
            })
            .collect(),
        media_files: analysis
            .media_files
            .into_iter()
            .map(|file| AnalyzedBookMediaFile {
                file_name: file.file_name,
                media_type: file.media_type,
                sub_type: file.sub_type,
                file_size: file.file_size,
            })
            .collect(),
        epub_extension_blob: analysis.epub_extension_blob,
    };
    let current_page_count = persisted.page_count.min(i64::MAX as u64) as i64;

    persist_book_analysis(runtime.database().task_write_pool(), &book_id, &persisted)
        .await
        .map_err(TaskProcessingError::runtime)?;

    adjust_analyzed_book_read_progress(
        runtime.database().task_write_pool(),
        &book_id,
        &input.series_id,
        input.previous_media_status,
        input.previous_page_count,
        current_page_count,
    )
    .await
    .map_err(TaskProcessingError::runtime)?;

    // Notify clients that the book media changed, mirroring Kotlin's
    // BookLifecycle.analyzeAndPersist which unconditionally publishes a
    // BookUpdated event after persisting the analysis.
    runtime
        .runtime_events()
        .register(RuntimeSseEvent::BookChanged {
            book_id: book_id.clone(),
            series_id: input.series_id.clone(),
            library_id: input.library_id.clone(),
        });

    Ok(AnalyzeBookOutcome {
        series_id: input.series_id,
        media_status: Some(persisted.status),
        metadata_sources: analysis.metadata_sources,
    })
}
