use std::fs;
use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion};
use localfiles::indexer::FileIndex;
use tempfile::TempDir;

const NUM_FILES: usize = 1000;
const EXTENSIONS: &[&str] = &["rs", "py", "js", "md", "txt", "toml", "yaml", "json"];
const NUM_DIRS: usize = 10;

/// Generate a synthetic dataset of files in a temporary directory.
fn generate_dataset(dir: &Path) {
    for i in 0..NUM_FILES {
        let subdir = format!("dir_{}", i % NUM_DIRS);
        let ext = EXTENSIONS[i % EXTENSIONS.len()];
        let rel_path = format!("{}/file_{}.{}", subdir, i, ext);
        let full_path = dir.join(&rel_path);
        fs::create_dir_all(full_path.parent().unwrap()).unwrap();

        // Size classes: ~1KB, ~10KB, ~100KB
        let repeats = match i % 3 {
            0 => 20,
            1 => 200,
            _ => 2000,
        };

        let mut content = String::with_capacity(repeats * 50);
        for r in 0..repeats {
            content.push_str(&format!(
                "Line {} of file {}: keyword_{} searchterm_{} common_word data\n",
                r, i, i, i
            ));
        }
        fs::write(&full_path, &content).unwrap();
    }
}

fn bench_index_directory(c: &mut Criterion) {
    let dataset_dir = TempDir::new().unwrap();
    generate_dataset(dataset_dir.path());

    c.bench_function("index_directory_1000_files", |b| {
        b.iter(|| {
            let index_dir = TempDir::new().unwrap();
            let mut idx = FileIndex::new(Some(index_dir.path().join("index"))).unwrap();
            idx.index_directory(dataset_dir.path()).unwrap();
            idx.commit().unwrap();
        });
    });
}

fn bench_commit(c: &mut Criterion) {
    let dataset_dir = TempDir::new().unwrap();
    generate_dataset(dataset_dir.path());

    c.bench_function("commit_500_files", |b| {
        b.iter_with_setup(
            || {
                let index_dir = TempDir::new().unwrap();
                let mut idx = FileIndex::new(Some(index_dir.path().join("index"))).unwrap();
                // Index only 500 files
                for i in 0..500 {
                    let ext = EXTENSIONS[i % EXTENSIONS.len()];
                    let subdir = format!("dir_{}", i % NUM_DIRS);
                    let path = dataset_dir.path().join(format!("{}/file_{}.{}", subdir, i, ext));
                    if path.exists() {
                        idx.index_file(&path).unwrap();
                    }
                }
                (idx, index_dir)
            },
            |(mut idx, _index_dir)| {
                idx.commit().unwrap();
            },
        );
    });
}

fn bench_search(c: &mut Criterion) {
    // Build a shared index once
    let dataset_dir = TempDir::new().unwrap();
    generate_dataset(dataset_dir.path());

    let index_dir = TempDir::new().unwrap();
    let mut idx = FileIndex::new(Some(index_dir.path().join("index"))).unwrap();
    idx.index_directory(dataset_dir.path()).unwrap();
    idx.commit().unwrap();

    let mut group = c.benchmark_group("search");

    group.bench_function("keyword_simple", |b| {
        b.iter(|| {
            idx.search("keyword_42", 10, None, None).unwrap();
        });
    });

    group.bench_function("keyword_with_file_type", |b| {
        b.iter(|| {
            idx.search("keyword_42", 10, Some("rs"), None).unwrap();
        });
    });

    group.bench_function("keyword_with_path_prefix", |b| {
        b.iter(|| {
            idx.search("keyword_42", 10, None, Some("dir_3")).unwrap();
        });
    });

    group.bench_function("keyword_with_both_filters", |b| {
        b.iter(|| {
            idx.search("keyword_42", 10, Some("rs"), Some("dir_3")).unwrap();
        });
    });

    group.bench_function("empty_query_file_type_only", |b| {
        b.iter(|| {
            idx.search("", 10, Some("rs"), None).unwrap();
        });
    });

    group.bench_function("broad_query_limit_100", |b| {
        b.iter(|| {
            idx.search("common_word", 100, None, None).unwrap();
        });
    });

    group.finish();
}

criterion_group!(benches, bench_index_directory, bench_commit, bench_search);
criterion_main!(benches);
