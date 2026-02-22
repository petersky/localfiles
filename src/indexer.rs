use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, STORED, STRING, TEXT};
use tantivy::schema::Value;
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};
use walkdir::WalkDir;

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10MB
const SCHEMA_VERSION: u32 = 2;

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "txt", "md", "rs", "py", "js", "ts", "jsx", "tsx", "json", "toml", "yaml", "yml", "html",
    "css", "scss", "sh", "bash", "zsh", "c", "cpp", "h", "hpp", "java", "go", "rb", "php",
    "sql", "xml", "csv", "log", "cfg", "conf", "ini", "env", "makefile", "dockerfile",
];

pub struct SearchResult {
    pub file_path: String,
    pub file_name: String,
    pub snippet: String,
    pub score: f32,
    pub line_number: Option<usize>,
}

pub struct SearchOutput {
    pub results: Vec<SearchResult>,
    pub total_count: usize,
}

pub struct IndexStatus {
    pub num_files: usize,
    pub watched_paths: Vec<String>,
    pub index_path: String,
}

pub struct FileIndex {
    index: Index,
    writer: IndexWriter,
    reader: IndexReader,
    field_path: Field,
    field_name: Field,
    field_content: Field,
    field_modified: Field,
    field_extension: Field,
    field_directory: Field,
    indexed_paths: HashSet<PathBuf>,
    watched_roots: Vec<PathBuf>,
    index_path: PathBuf,
}

impl FileIndex {
    pub fn new(index_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let index_path = index_path.unwrap_or_else(|| {
            let mut p = std::env::temp_dir();
            p.push("localfiles_index");
            p
        });

        // Schema version migration: delete stale index if version mismatches
        let version_file = index_path.join("schema_version");
        if index_path.exists() {
            let needs_recreate = match std::fs::read_to_string(&version_file) {
                Ok(v) => v.trim().parse::<u32>().unwrap_or(0) != SCHEMA_VERSION,
                Err(_) => true, // missing version file means old schema
            };
            if needs_recreate {
                tracing::info!("Schema version changed, recreating index at {}", index_path.display());
                std::fs::remove_dir_all(&index_path)?;
            }
        }

        let mut schema_builder = Schema::builder();
        let field_path = schema_builder.add_text_field("file_path", STRING | STORED);
        let field_name = schema_builder.add_text_field("file_name", TEXT | STORED);
        let field_content = schema_builder.add_text_field("content", TEXT | STORED);
        let field_modified = schema_builder.add_text_field("last_modified", STRING | STORED);
        let field_extension = schema_builder.add_text_field("extension", TEXT | STORED);
        let field_directory = schema_builder.add_text_field("directory", TEXT | STORED);
        let schema = schema_builder.build();

        let index = if index_path.exists() {
            match Index::open_in_dir(&index_path) {
                Ok(idx) => idx,
                Err(_) => {
                    tracing::warn!("Corrupted index, recreating at {}", index_path.display());
                    std::fs::remove_dir_all(&index_path)?;
                    std::fs::create_dir_all(&index_path)?;
                    Index::create_in_dir(&index_path, schema.clone())?
                }
            }
        } else {
            std::fs::create_dir_all(&index_path)?;
            Index::create_in_dir(&index_path, schema.clone())?
        };

        // Write schema version file
        std::fs::write(&version_file, SCHEMA_VERSION.to_string())?;

        let writer = index.writer(50_000_000)?; // 50MB heap
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        Ok(Self {
            index,
            writer,
            reader,
            field_path,
            field_name,
            field_content,
            field_modified,
            field_extension,
            field_directory,
            indexed_paths: HashSet::new(),
            watched_roots: Vec::new(),
            index_path,
        })
    }

    pub fn index_file(&mut self, path: &Path) -> anyhow::Result<()> {
        if !Self::is_supported(path) {
            return Ok(());
        }

        let metadata = std::fs::metadata(path)?;
        if metadata.len() > MAX_FILE_SIZE {
            return Ok(());
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Ok(()), // skip binary / unreadable files
        };

        let modified = metadata
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let file_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let file_path_str = path.to_string_lossy().to_string();
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let directory = path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        // Upsert: remove existing then add
        self.remove_file(path)?;

        self.writer.add_document(doc!(
            self.field_path => file_path_str,
            self.field_name => file_name,
            self.field_content => content,
            self.field_modified => format!("{}s", modified.as_secs()),
            self.field_extension => extension,
            self.field_directory => directory,
        ))?;
        self.indexed_paths.insert(path.to_path_buf());
        Ok(())
    }

    pub fn remove_file(&mut self, path: &Path) -> anyhow::Result<()> {
        let path_str = path.to_string_lossy().to_string();
        self.writer
            .delete_term(Term::from_field_text(self.field_path, &path_str));
        self.indexed_paths.remove(path);
        Ok(())
    }

    pub fn index_directory(&mut self, dir: &Path) -> anyhow::Result<u64> {
        let mut count = 0u64;
        for entry in WalkDir::new(dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                if self.index_file(entry.path()).is_ok() {
                    count += 1;
                }
            }
        }
        if !self.watched_roots.contains(&dir.to_path_buf()) {
            self.watched_roots.push(dir.to_path_buf());
        }
        Ok(count)
    }

    pub fn commit(&mut self) -> anyhow::Result<()> {
        self.writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    pub fn search(
        &self,
        query_str: &str,
        limit: usize,
        file_type: Option<&str>,
        path_prefix: Option<&str>,
    ) -> anyhow::Result<SearchOutput> {
        let has_text_query = !query_str.trim().is_empty();
        let has_filters = file_type.is_some() || path_prefix.is_some();

        if !has_text_query && !has_filters {
            return Ok(SearchOutput {
                results: vec![],
                total_count: 0,
            });
        }

        let searcher = self.reader.searcher();

        // Build query clauses
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();

        // Text query parsed by QueryParser (supports field:value syntax for all fields)
        if has_text_query {
            let query_parser = QueryParser::for_index(
                &self.index,
                vec![self.field_content, self.field_name],
            );
            let parsed = query_parser.parse_query(query_str)?;
            clauses.push((Occur::Must, parsed));
        }

        // file_type param -> TermQuery on extension field
        if let Some(ext) = file_type {
            let term = Term::from_field_text(self.field_extension, &ext.to_lowercase());
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        // path_prefix param -> TermQuery per path component on directory field
        if let Some(prefix) = path_prefix {
            for segment in prefix.split('/').filter(|s| !s.is_empty()) {
                let term = Term::from_field_text(self.field_directory, &segment.to_lowercase());
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
        }

        let query = BooleanQuery::new(clauses);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        // Build query terms for snippet extraction (only from text query, not field filters)
        let query_terms: Vec<String> = if has_text_query {
            query_str
                .split_whitespace()
                .filter(|s| !s.contains(':'))
                .map(|s| s.to_lowercase())
                .collect()
        } else {
            vec![]
        };

        let mut results = Vec::new();
        for (score, doc_address) in top_docs {
            let doc: TantivyDocument = searcher.doc(doc_address)?;
            let file_path = doc
                .get_first(self.field_path)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let file_name = doc
                .get_first(self.field_name)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = doc
                .get_first(self.field_content)
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let snippet = Self::extract_snippet(content, &query_terms, 200);
            let line_number = Self::find_match_line(content, &query_terms);

            results.push(SearchResult {
                file_path,
                file_name,
                snippet,
                score,
                line_number,
            });
        }

        let total_count = results.len();

        Ok(SearchOutput {
            results,
            total_count,
        })
    }

    pub fn read_file(&self, path: &str) -> anyhow::Result<String> {
        let path = std::path::Path::new(path).canonicalize()?;
        if !self.indexed_paths.contains(&path) {
            anyhow::bail!("File is not in the index: {}", path.display());
        }
        let content = std::fs::read_to_string(&path)?;
        Ok(content)
    }

    pub fn list_files(&self, extension: Option<&str>, path_prefix: Option<&str>) -> Vec<String> {
        let mut files: Vec<String> = self
            .indexed_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|p| {
                if let Some(ext) = extension {
                    let matches = Path::new(p)
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case(ext))
                        .unwrap_or(false);
                    if !matches {
                        return false;
                    }
                }
                if let Some(prefix) = path_prefix {
                    if !p.contains(prefix) {
                        return false;
                    }
                }
                true
            })
            .collect();
        files.sort();
        files
    }

    pub fn status(&self) -> IndexStatus {
        IndexStatus {
            num_files: self.indexed_paths.len(),
            watched_paths: self.watched_roots.iter().map(|p| p.display().to_string()).collect(),
            index_path: self.index_path.display().to_string(),
        }
    }

    fn is_supported(path: &Path) -> bool {
        // Check known extensionless filenames
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_lowercase();
            if lower == "makefile" || lower == "dockerfile" {
                return true;
            }
        }
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| SUPPORTED_EXTENSIONS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false)
    }

    fn find_match_line(content: &str, query_terms: &[String]) -> Option<usize> {
        let content_lower = content.to_lowercase();
        for term in query_terms {
            if let Some(pos) = content_lower.find(&term.to_lowercase()) {
                // Count newlines before the match position (1-indexed)
                let line = content[..pos].matches('\n').count() + 1;
                return Some(line);
            }
        }
        None
    }

    fn extract_snippet(content: &str, query_terms: &[String], window: usize) -> String {
        let content_lower = content.to_lowercase();
        let mut best_pos = 0;
        for term in query_terms {
            if let Some(pos) = content_lower.find(&term.to_lowercase()) {
                best_pos = pos;
                break;
            }
        }
        let start = best_pos.saturating_sub(window / 2);
        let end = (best_pos + window / 2).min(content.len());

        // Align to char boundaries
        let start = {
            let mut s = start;
            while s > 0 && !content.is_char_boundary(s) {
                s -= 1;
            }
            s
        };
        let end = {
            let mut e = end.min(content.len());
            while e < content.len() && !content.is_char_boundary(e) {
                e += 1;
            }
            e
        };

        let snippet = &content[start..end];
        format!("...{}...", snippet.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn test_index(dir: &TempDir) -> FileIndex {
        let index_path = dir.path().join("index");
        FileIndex::new(Some(index_path)).expect("failed to create test index")
    }

    fn write_fixture(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    // -- Index creation & migration --

    #[test]
    fn test_new_creates_index() {
        let dir = TempDir::new().unwrap();
        let index_path = dir.path().join("index");
        let _idx = FileIndex::new(Some(index_path.clone())).unwrap();
        let version = fs::read_to_string(index_path.join("schema_version")).unwrap();
        assert_eq!(version.trim(), "2");
    }

    #[test]
    fn test_new_opens_existing_index() {
        let dir = TempDir::new().unwrap();
        let index_path = dir.path().join("index");
        let _idx1 = FileIndex::new(Some(index_path.clone())).unwrap();
        drop(_idx1);
        let _idx2 = FileIndex::new(Some(index_path)).unwrap();
    }

    #[test]
    fn test_schema_version_migration() {
        let dir = TempDir::new().unwrap();
        let index_path = dir.path().join("index");
        let _idx = FileIndex::new(Some(index_path.clone())).unwrap();
        drop(_idx);
        // Overwrite version to trigger migration
        fs::write(index_path.join("schema_version"), "1").unwrap();
        let _idx2 = FileIndex::new(Some(index_path.clone())).unwrap();
        let version = fs::read_to_string(index_path.join("schema_version")).unwrap();
        assert_eq!(version.trim(), "2");
    }

    // -- is_supported --

    #[test]
    fn test_is_supported_common_extensions() {
        for ext in &["rs", "py", "js", "md", "yaml"] {
            let p = PathBuf::from(format!("test.{}", ext));
            assert!(FileIndex::is_supported(&p), "expected {} to be supported", ext);
        }
    }

    #[test]
    fn test_is_supported_makefile_dockerfile() {
        assert!(FileIndex::is_supported(Path::new("Makefile")));
        assert!(FileIndex::is_supported(Path::new("Dockerfile")));
    }

    #[test]
    fn test_is_supported_unsupported() {
        for ext in &["png", "jpg", "exe"] {
            let p = PathBuf::from(format!("test.{}", ext));
            assert!(!FileIndex::is_supported(&p), "expected {} to be unsupported", ext);
        }
    }

    #[test]
    fn test_is_supported_no_extension() {
        assert!(!FileIndex::is_supported(Path::new("README")));
    }

    #[test]
    fn test_is_supported_case_insensitive() {
        assert!(FileIndex::is_supported(Path::new("test.RS")));
        assert!(FileIndex::is_supported(Path::new("test.Py")));
    }

    // -- index_file --

    #[test]
    fn test_index_file_basic() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "hello.rs", "fn main() {}");
        idx.index_file(&f).unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.status().num_files, 1);
    }

    #[test]
    fn test_index_file_unsupported_skipped() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "image.png", "not really an image");
        idx.index_file(&f).unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.status().num_files, 0);
    }

    #[test]
    fn test_index_file_binary_skipped() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = fixtures.path().join("binary.rs");
        // Invalid UTF-8 bytes cause read_to_string to fail, so the file is skipped
        fs::write(&f, b"hello\xff\xfeworld").unwrap();
        idx.index_file(&f).unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.status().num_files, 0);
    }

    #[test]
    fn test_index_file_upsert() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "data.rs", "old_unique_content");
        idx.index_file(&f).unwrap();
        idx.commit().unwrap();

        // Overwrite with new content
        fs::write(&f, "new_unique_content").unwrap();
        idx.index_file(&f).unwrap();
        idx.commit().unwrap();

        let old = idx.search("old_unique_content", 10, None, None).unwrap();
        assert_eq!(old.results.len(), 0);
        let new = idx.search("new_unique_content", 10, None, None).unwrap();
        assert_eq!(new.results.len(), 1);
    }

    // -- index_directory --

    #[test]
    fn test_index_directory_recursive() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        write_fixture(fixtures.path(), "a.rs", "aaa");
        write_fixture(fixtures.path(), "sub/b.py", "bbb");
        write_fixture(fixtures.path(), "sub/deep/c.js", "ccc");
        let count = idx.index_directory(fixtures.path()).unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn test_index_directory_skips_unsupported() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        write_fixture(fixtures.path(), "good.rs", "code");
        write_fixture(fixtures.path(), "bad.png", "pixels");
        write_fixture(fixtures.path(), "also_good.md", "docs");
        let _count = idx.index_directory(fixtures.path()).unwrap();
        // count includes all files walked (supported or not) since index_file returns Ok(())
        // but num_files only tracks actually indexed ones
        idx.commit().unwrap();
        assert_eq!(idx.status().num_files, 2);
    }

    #[test]
    fn test_index_directory_adds_watched_root() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        write_fixture(fixtures.path(), "a.rs", "code");
        idx.index_directory(fixtures.path()).unwrap();
        let status = idx.status();
        assert!(status.watched_paths.contains(&fixtures.path().display().to_string()));
    }

    // -- search: keyword --

    #[test]
    fn test_search_keyword_match() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "greet.rs", "hello world");
        idx.index_file(&f).unwrap();
        idx.commit().unwrap();
        let res = idx.search("hello", 10, None, None).unwrap();
        assert_eq!(res.results.len(), 1);
    }

    #[test]
    fn test_search_empty_query_no_filters() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "a.rs", "content");
        idx.index_file(&f).unwrap();
        idx.commit().unwrap();
        let res = idx.search("", 10, None, None).unwrap();
        assert_eq!(res.results.len(), 0);
    }

    #[test]
    fn test_search_limit() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        for i in 0..5 {
            let f = write_fixture(fixtures.path(), &format!("f{}.rs", i), "shared_keyword_xyz");
            idx.index_file(&f).unwrap();
        }
        idx.commit().unwrap();
        let res = idx.search("shared_keyword_xyz", 2, None, None).unwrap();
        assert!(res.results.len() <= 2);
    }

    #[test]
    fn test_search_no_match() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "a.rs", "some content");
        idx.index_file(&f).unwrap();
        idx.commit().unwrap();
        let res = idx.search("nonexistent_term_xyz", 10, None, None).unwrap();
        assert_eq!(res.results.len(), 0);
    }

    // -- search: field-based --

    #[test]
    fn test_search_empty_query_with_file_type() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f1 = write_fixture(fixtures.path(), "a.rs", "rust code");
        let f2 = write_fixture(fixtures.path(), "b.py", "python code");
        idx.index_file(&f1).unwrap();
        idx.index_file(&f2).unwrap();
        idx.commit().unwrap();
        let res = idx.search("", 10, Some("rs"), None).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].file_path.ends_with("a.rs"));
    }

    #[test]
    fn test_search_file_type_filter() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f1 = write_fixture(fixtures.path(), "a.rs", "shared_token_abc");
        let f2 = write_fixture(fixtures.path(), "b.py", "shared_token_abc");
        idx.index_file(&f1).unwrap();
        idx.index_file(&f2).unwrap();
        idx.commit().unwrap();
        let res = idx.search("shared_token_abc", 10, Some("rs"), None).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].file_path.ends_with("a.rs"));
    }

    #[test]
    fn test_search_path_prefix_filter() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f1 = write_fixture(fixtures.path(), "src/a.rs", "unique_path_token");
        let f2 = write_fixture(fixtures.path(), "tests/b.rs", "unique_path_token");
        idx.index_file(&f1).unwrap();
        idx.index_file(&f2).unwrap();
        idx.commit().unwrap();
        let res = idx.search("unique_path_token", 10, None, Some("src")).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].file_path.contains("src"));
    }

    #[test]
    fn test_search_combined_filters() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f1 = write_fixture(fixtures.path(), "src/a.rs", "combo_token");
        let f2 = write_fixture(fixtures.path(), "src/b.py", "combo_token");
        let f3 = write_fixture(fixtures.path(), "tests/c.rs", "combo_token");
        idx.index_file(&f1).unwrap();
        idx.index_file(&f2).unwrap();
        idx.index_file(&f3).unwrap();
        idx.commit().unwrap();
        let res = idx.search("combo_token", 10, Some("rs"), Some("src")).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].file_path.contains("src"));
        assert!(res.results[0].file_path.ends_with("a.rs"));
    }

    // -- remove_file --

    #[test]
    fn test_remove_file_from_search() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "rm.rs", "removable_content");
        idx.index_file(&f).unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.search("removable_content", 10, None, None).unwrap().results.len(), 1);
        idx.remove_file(&f).unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.search("removable_content", 10, None, None).unwrap().results.len(), 0);
    }

    #[test]
    fn test_remove_file_updates_indexed_paths() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "gone.rs", "content");
        idx.index_file(&f).unwrap();
        idx.commit().unwrap();
        assert!(idx.list_files(None, None).iter().any(|p| p.contains("gone.rs")));
        idx.remove_file(&f).unwrap();
        assert!(!idx.list_files(None, None).iter().any(|p| p.contains("gone.rs")));
    }

    // -- list_files --

    #[test]
    fn test_list_files_all() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f1 = write_fixture(fixtures.path(), "a.rs", "aaa");
        let f2 = write_fixture(fixtures.path(), "b.py", "bbb");
        idx.index_file(&f1).unwrap();
        idx.index_file(&f2).unwrap();
        let files = idx.list_files(None, None);
        assert_eq!(files.len(), 2);
        // Should be sorted
        assert!(files[0] < files[1]);
    }

    #[test]
    fn test_list_files_extension_filter() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f1 = write_fixture(fixtures.path(), "a.rs", "aaa");
        let f2 = write_fixture(fixtures.path(), "b.py", "bbb");
        idx.index_file(&f1).unwrap();
        idx.index_file(&f2).unwrap();
        let files = idx.list_files(Some("rs"), None);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("a.rs"));
    }

    #[test]
    fn test_list_files_path_prefix_filter() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f1 = write_fixture(fixtures.path(), "src/a.rs", "aaa");
        let f2 = write_fixture(fixtures.path(), "tests/b.rs", "bbb");
        idx.index_file(&f1).unwrap();
        idx.index_file(&f2).unwrap();
        let files = idx.list_files(None, Some("src"));
        assert_eq!(files.len(), 1);
        assert!(files[0].contains("src"));
    }

    // -- read_file --

    #[test]
    fn test_read_file_indexed() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "readable.rs", "fn hello() {}");
        // Use canonical path since read_file calls canonicalize()
        let canonical = f.canonicalize().unwrap();
        idx.index_file(&canonical).unwrap();
        idx.commit().unwrap();
        let content = idx.read_file(canonical.to_str().unwrap()).unwrap();
        assert_eq!(content, "fn hello() {}");
    }

    #[test]
    fn test_read_file_not_indexed() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "unindexed.rs", "content");
        let canonical = f.canonicalize().unwrap();
        let result = idx.read_file(canonical.to_str().unwrap());
        assert!(result.is_err());
    }

    // -- status --

    #[test]
    fn test_status_fields() {
        let dir = TempDir::new().unwrap();
        let fixtures = TempDir::new().unwrap();
        let mut idx = test_index(&dir);
        let f = write_fixture(fixtures.path(), "s.rs", "code");
        idx.index_file(&f).unwrap();
        idx.index_directory(fixtures.path()).unwrap();
        idx.commit().unwrap();
        let status = idx.status();
        assert!(status.num_files >= 1);
        assert!(!status.index_path.is_empty());
        assert!(!status.watched_paths.is_empty());
    }

    // -- extract_snippet --

    #[test]
    fn test_extract_snippet_centered() {
        let content = "aaaa bbbb cccc target_word dddd eeee ffff";
        let terms = vec!["target_word".to_string()];
        let snippet = FileIndex::extract_snippet(content, &terms, 30);
        assert!(snippet.contains("target_word"));
    }

    #[test]
    fn test_extract_snippet_at_start() {
        let content = "target_word is at the very beginning of this text";
        let terms = vec!["target_word".to_string()];
        let snippet = FileIndex::extract_snippet(content, &terms, 40);
        assert!(snippet.contains("target_word"));
    }

    #[test]
    fn test_extract_snippet_utf8_safe() {
        // Multi-byte chars (emoji) near window boundary — ensure no panic
        let content = "🎉🎊🎈 target_word 🎉🎊🎈";
        let terms = vec!["target_word".to_string()];
        let snippet = FileIndex::extract_snippet(content, &terms, 60);
        assert!(snippet.contains("target_word"));
    }

    // -- find_match_line --

    #[test]
    fn test_find_match_line_found() {
        let content = "line1\nline2\ntarget";
        let terms = vec!["target".to_string()];
        assert_eq!(FileIndex::find_match_line(content, &terms), Some(3));
    }

    #[test]
    fn test_find_match_line_first_line() {
        let content = "target on first line\nsecond line";
        let terms = vec!["target".to_string()];
        assert_eq!(FileIndex::find_match_line(content, &terms), Some(1));
    }

    #[test]
    fn test_find_match_line_not_found() {
        let content = "nothing here";
        let terms = vec!["absent".to_string()];
        assert_eq!(FileIndex::find_match_line(content, &terms), None);
    }
}
