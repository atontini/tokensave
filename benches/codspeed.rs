//! Self-contained CPU benchmarks for CodSpeed.
//!
//! Unlike `large_repos` (which clones real-world repositories) these
//! benchmarks only use sources shipped in this repository, so they run
//! offline and deterministically in CI:
//!
//! - `extraction/*`: tree-sitter parse + symbol/edge extraction per language.
//! - `registry/*`: building the language registry and routing paths to it.
//! - `language_detection/*`: content-based language detection on a buffer.
//! - `indexing/*`: full `init` + `index_all` of a small generated project.
//! - `query/*`: graph queries against a pre-indexed project.
//!
//! Run locally with `cargo bench --bench codspeed`, or under CodSpeed with
//! `cargo codspeed build --bench codspeed && cargo codspeed run --bench codspeed`.

use std::hint::black_box;
use std::path::Path;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use tokio::runtime::Runtime;

use tokensave::extraction::LanguageRegistry;
use tokensave::language_detection::{detect, DetectionConfig};
use tokensave::tokensave::TokenSave;
use tokensave::types::BuildContextOptions;

/// Real source files from this repository and its test fixtures.
const SOURCES: &[(&str, &str)] = &[
    (
        "src/extraction/rust_extractor.rs",
        include_str!("../src/extraction/rust_extractor.rs"),
    ),
    (
        "src/tokensave/query.rs",
        include_str!("../src/tokensave/query.rs"),
    ),
    ("sample.py", include_str!("../tests/fixtures/sample.py")),
    ("sample.ts", include_str!("../tests/fixtures/sample.ts")),
    ("sample.js", include_str!("../tests/fixtures/sample.js")),
    ("sample.c", include_str!("../tests/fixtures/sample.c")),
    ("sample.cpp", include_str!("../tests/fixtures/sample.cpp")),
    ("sample.cs", include_str!("../tests/fixtures/sample.cs")),
    ("sample.kt", include_str!("../tests/fixtures/sample.kt")),
    (
        "sample.swift",
        include_str!("../tests/fixtures/sample.swift"),
    ),
];

fn bench_extraction(c: &mut Criterion) {
    let registry = LanguageRegistry::new();
    let mut group = c.benchmark_group("extraction");
    for (path, source) in SOURCES {
        let extractor = registry
            .extractor_for_file(path)
            .unwrap_or_else(|| panic!("no extractor for {path}"));
        group.bench_with_input(BenchmarkId::from_parameter(path), source, |b, source| {
            b.iter(|| black_box(extractor.extract(path, black_box(source))));
        });
    }
    group.finish();
}

fn bench_registry(c: &mut Criterion) {
    let mut group = c.benchmark_group("registry");
    group.bench_function("new", |b| b.iter(|| black_box(LanguageRegistry::new())));

    let registry = LanguageRegistry::new();
    let paths: Vec<&str> = SOURCES.iter().map(|(p, _)| *p).collect();
    group.bench_function("extractor_for_file", |b| {
        b.iter(|| {
            for path in &paths {
                black_box(registry.extractor_for_file(black_box(path)));
            }
        });
    });
    group.finish();
}

fn bench_language_detection(c: &mut Criterion) {
    let registry = LanguageRegistry::new();
    let config = DetectionConfig {
        enabled: true,
        ..DetectionConfig::default()
    };
    let mut group = c.benchmark_group("language_detection");
    let cases: &[(&str, &[u8])] = &[
        (
            "rust",
            include_bytes!("../src/extraction/rust_extractor.rs").as_slice(),
        ),
        ("python", include_bytes!("../tests/fixtures/sample.py")),
        ("shell", include_bytes!("../tests/fixtures/sample.sh")),
        (
            "plain_text",
            b"ordinary text with no programming syntax\nanother line of prose\n",
        ),
    ];
    for (name, bytes) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(name), bytes, |b, bytes| {
            b.iter(|| black_box(detect(&registry, black_box(bytes), config)));
        });
    }
    group.finish();
}

/// Writes a small multi-language project with cross-file calls.
fn write_project(root: &Path, modules: usize) {
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("create src");
    let mut lib = String::new();
    for i in 0..modules {
        lib.push_str(&format!("pub mod module_{i};\n"));
        let next = (i + 1) % modules;
        let body = format!(
            "use crate::module_{next}::helper_{next};\n\n\
             /// Shape number {i}.\n\
             pub struct Shape{i} {{\n    pub width: f64,\n    pub height: f64,\n}}\n\n\
             impl Shape{i} {{\n    pub fn area(&self) -> f64 {{\n        self.width * self.height\n    }}\n\n\
             \x20   pub fn scaled(&self, factor: f64) -> Self {{\n        Self {{ width: self.width * factor, height: self.height * factor }}\n    }}\n}}\n\n\
             pub trait Describe{i} {{\n    fn describe(&self) -> String;\n}}\n\n\
             impl Describe{i} for Shape{i} {{\n    fn describe(&self) -> String {{\n        format!(\"shape {i}: {{}}\", self.area())\n    }}\n}}\n\n\
             pub fn helper_{i}(x: u64) -> u64 {{\n    if x == 0 {{ 0 }} else {{ compute_{i}(x) + 1 }}\n}}\n\n\
             pub fn compute_{i}(x: u64) -> u64 {{\n    helper_{next}(x - 1) * 2\n}}\n"
        );
        std::fs::write(src.join(format!("module_{i}.rs")), body).expect("write module");
    }
    std::fs::write(src.join("lib.rs"), lib).expect("write lib");
    let py = root.join("scripts");
    std::fs::create_dir_all(&py).expect("create scripts");
    std::fs::write(
        py.join("sample.py"),
        include_str!("../tests/fixtures/sample.py"),
    )
    .expect("write py");
    std::fs::write(
        py.join("sample.ts"),
        include_str!("../tests/fixtures/sample.ts"),
    )
    .expect("write ts");
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn bench_indexing(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("indexing");
    group.sample_size(10);
    group.bench_function("index_all_20_modules", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().expect("tempdir");
                write_project(dir.path(), 20);
                let cg = rt.block_on(TokenSave::init(dir.path())).expect("init");
                (dir, cg)
            },
            |(dir, cg)| {
                let result = rt.block_on(cg.index_all()).expect("index_all");
                black_box(result);
                (dir, cg)
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn bench_queries(c: &mut Criterion) {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    write_project(dir.path(), 20);
    let cg = rt.block_on(async {
        let cg = TokenSave::init(dir.path()).await.expect("init");
        cg.index_all().await.expect("index_all");
        cg
    });
    let target = rt
        .block_on(cg.search("helper_5", 1))
        .expect("search")
        .into_iter()
        .next()
        .expect("helper_5 indexed")
        .node
        .id;

    let mut group = c.benchmark_group("query");
    group.bench_function("search", |b| {
        b.iter(|| {
            black_box(
                rt.block_on(cg.search(black_box("Shape"), 20))
                    .expect("search"),
            )
        });
    });
    group.bench_function("get_callers", |b| {
        b.iter(|| black_box(rt.block_on(cg.get_callers(&target, 3)).expect("callers")));
    });
    group.bench_function("get_callees", |b| {
        b.iter(|| black_box(rt.block_on(cg.get_callees(&target, 3)).expect("callees")));
    });
    let options = BuildContextOptions::default();
    group.bench_function("build_context", |b| {
        b.iter(|| {
            black_box(
                rt.block_on(cg.build_context("compute the area of a shape", &options))
                    .expect("context"),
            )
        });
    });
    group.finish();
    drop(cg);
    drop(dir);
}

criterion_group!(
    benches,
    bench_extraction,
    bench_registry,
    bench_language_detection,
    bench_indexing,
    bench_queries
);
criterion_main!(benches);
