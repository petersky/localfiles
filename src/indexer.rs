use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, STORED, STRING, TEXT};
use tantivy::schema::Value;
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};
use walkdir::WalkDir;

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10MB

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

        let mut schema_builder = Schema::builder();
        let field_path = schema_builder.add_text_field("file_path", STRING | STORED);
        let field_name = schema_builder.add_text_field("file_name", TEXT | STORED);
        let field_content = schema_builder.add_text_field("content", TEXT | STORED);
        let field_modified = schema_builder.add_text_field("last_modified", STRING | STORED);
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

        // Upsert: remove existing then add
        self.remove_file(path)?;

        self.writer.add_document(doc!(
            self.field_path => file_path_str,
            self.field_name => file_name,
            self.field_content => content,
            self.field_modified => format!("{}s", modified.as_secs()),
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

    pub fn search(&self, query_str: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
        if query_str.trim().is_empty() {
            return Ok(vec![]);
        }

        let searcher = self.reader.searcher();
        let query_parser =
            QueryParser::for_index(&self.index, vec![self.field_content, self.field_name]);
        let query = query_parser.parse_query(query_str)?;
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        let query_terms: Vec<String> = query_str
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .collect();

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

            results.push(SearchResult {
                file_path,
                file_name,
                snippet,
                score,
            });
        }
        Ok(results)
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

        // Align to char boundaries (stable alternative)
        let start = if start == 0 || content.is_char_boundary(start) {
            start
        } else {
            content[..start]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0)
        };
        let end = if end >= content.len() || content.is_char_boundary(end) {
            end.min(content.len())
        } else {
            content[end..]
                .char_indices()
                .next()
                .map(|(i, _)| end + i)
                .unwrap_or(content.len())
        };

        let snippet = &content[start..end];
        format!("...{}...", snippet.trim())
    }
}
