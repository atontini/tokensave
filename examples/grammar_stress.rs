//! Stress-parses a corpus with every compiled tree-sitter grammar (#958).
//!
//! Every corpus file is parsed by every grammar, not only its own: real code
//! fed to the wrong grammar reaches error-recovery paths in the generated
//! parsers and external scanners. Each input is then mutated deterministically
//! (byte flips, truncation, deletion, insertion of delimiter and UTF-8 lead
//! bytes, duplication, splicing from other files, deep nesting), parsed again,
//! edited and re-parsed incrementally so that scanner state is serialized and
//! restored, and walked node by node.
//!
//! The harness is built to run under AddressSanitizer, which turns a memory
//! error in the C code into an abort with a report. See
//! `scripts/sanitizers/README.md` and `docs/GRAMMAR-SANITIZERS.md`. Without a
//! sanitizer it still detects hangs (parses that exceed `--timeout-ms`) and
//! crashes.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --example grammar_stress -- [OPTIONS] <PATH>...
//!
//!   --mutations N     mutated variants per corpus file and grammar (default 24)
//!   --seed N          RNG seed (default 958)
//!   --languages A,B   only these grammar keys (default: every compiled grammar)
//!   --timeout-ms N    per-parse deadline before a parse counts as a hang (default 10000)
//!   --slow-ms N       report parses slower than this as SLOW (default 1000)
//!   --max-bytes N     skip corpus files larger than this (default 131072)
//!   --jobs N          worker threads (default: available parallelism)
//!   --extract         also run each file's own extractor on the original and mutated text
//!   --case-dir DIR    before each parse, write the input to DIR/<grammar>.bin so the
//!                     input that crashed the process can be recovered
//! ```
//!
//! Seeds derive from `--seed`, the grammar key and the file path, so rerunning
//! with `--languages <key>` on the same corpus rebuilds the same inputs. The
//! exit status is 0 when every parse finished, 2 when a parse hung, and 64
//! for a usage error. Slow parses are reported but do not change the status. A memory error aborts the process from the sanitizer.

use std::collections::BTreeSet;
use std::io::Write;
use std::ops::ControlFlow;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;
use tokensave::extraction::ts_provider;
use tokensave::extraction::LanguageRegistry;
use tree_sitter::InputEdit;
use tree_sitter::ParseOptions;
use tree_sitter::Parser;
use tree_sitter::Point;
use tree_sitter::Tree;

/// Bytes that change tokenization in most grammars: delimiters, quotes,
/// escapes, comment and heredoc starters, line ends, NUL, and UTF-8 lead and
/// continuation bytes that start invalid or truncated sequences.
const INTERESTING: &[u8] =
    b"(){}[]<>\"'`\\/*#$@%&|;:,.=!?-+~^\n\r\t \0\x7f\xc0\xc3\xe2\xf0\xf4\xff\x80\xbf";

/// Openers repeated to build deeply nested input.
const NESTING: &[&[u8]] = &[
    b"(", b"[", b"{", b"<", b"\"", b"/*", b"#{", b"${", b"<<", b"((", b"begin ",
];

struct Options {
    mutations: usize,
    seed: u64,
    languages: Option<BTreeSet<String>>,
    timeout: Duration,
    slow: Duration,
    max_bytes: u64,
    jobs: Option<usize>,
    extract: bool,
    case_dir: Option<PathBuf>,
    paths: Vec<PathBuf>,
}

struct CorpusFile {
    /// Path as given, used for reporting and extractor dispatch.
    name: String,
    bytes: Vec<u8>,
}

/// A parse that was slow (`SLOW`) or did not finish before the deadline
/// (`HANG`).
struct Finding {
    kind: &'static str,
    language: String,
    case: String,
    millis: u128,
}

#[derive(Default)]
struct Counters {
    parses: AtomicU64,
    nodes: AtomicU64,
    error_trees: AtomicU64,
    extractions: AtomicU64,
}

/// xorshift64* — deterministic, seedable, no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // A zero state would stay zero forever.
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform-enough index in `0..n`; `n` must be non-zero.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n as u64)).unwrap_or(0)
    }

    fn byte(&mut self) -> u8 {
        if self.next() & 1 == 0 {
            INTERESTING[self.below(INTERESTING.len())]
        } else {
            self.next().to_le_bytes()[0]
        }
    }
}

fn main() {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("grammar_stress: {message}");
            std::process::exit(64);
        }
    };
    if let Some(jobs) = options.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global()
            .expect("the global rayon pool is configured once, before any parallel work");
    }
    if let Some(dir) = &options.case_dir {
        std::fs::create_dir_all(dir).expect("--case-dir must be creatable");
    }

    let corpus = load_corpus(&options);
    if corpus.is_empty() {
        eprintln!("grammar_stress: the corpus is empty");
        std::process::exit(64);
    }
    let all_keys = ts_provider::language_keys();
    let keys: Vec<&str> = match &options.languages {
        Some(wanted) => {
            let unknown: Vec<&String> = wanted
                .iter()
                .filter(|w| !all_keys.contains(&w.as_str()))
                .collect();
            if !unknown.is_empty() {
                eprintln!(
                    "grammar_stress: unknown grammar keys {unknown:?}; compiled: {all_keys:?}"
                );
                std::process::exit(64);
            }
            all_keys
                .iter()
                .copied()
                .filter(|k| wanted.contains(*k))
                .collect()
        }
        None => all_keys,
    };
    let corpus_bytes: usize = corpus.iter().map(|f| f.bytes.len()).sum();
    eprintln!(
        "grammar_stress: {} grammars x {} files ({} bytes), {} mutations each, seed {}",
        keys.len(),
        corpus.len(),
        corpus_bytes,
        options.mutations,
        options.seed
    );

    let started = Instant::now();
    let counters = Counters::default();
    let findings = Mutex::new(Vec::new());
    keys.par_iter().for_each(|key| {
        let lang_started = Instant::now();
        stress_language(key, &corpus, &options, &counters, &findings);
        eprintln!(
            "grammar_stress: {key} done in {:.1?}",
            lang_started.elapsed()
        );
    });
    if options.extract {
        stress_extractors(&corpus, &options, &counters);
    }

    let mut findings = findings
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    findings.sort_by(|a, b| (a.kind, &a.language, &a.case).cmp(&(b.kind, &b.language, &b.case)));
    let hangs = findings.iter().filter(|f| f.kind == "HANG").count();
    let slow = findings.len() - hangs;
    println!(
        "grammars={} files={} parses={} nodes={} error_trees={} extractions={} hangs={} slow={} elapsed={:.1?}",
        keys.len(),
        corpus.len(),
        counters.parses.load(Ordering::Relaxed),
        counters.nodes.load(Ordering::Relaxed),
        counters.error_trees.load(Ordering::Relaxed),
        counters.extractions.load(Ordering::Relaxed),
        hangs,
        slow,
        started.elapsed()
    );
    for f in &findings {
        println!(
            "{} grammar={} ms={} case={}",
            f.kind, f.language, f.millis, f.case
        );
    }
    if hangs > 0 {
        std::process::exit(2);
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        mutations: 24,
        seed: 958,
        languages: None,
        timeout: Duration::from_millis(10_000),
        slow: Duration::from_millis(1000),
        max_bytes: 128 * 1024,
        jobs: None,
        extract: false,
        case_dir: None,
        paths: Vec::new(),
    };
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--mutations" => options.mutations = parse_num(&value(&arg)?, &arg)?,
            "--seed" => options.seed = parse_num(&value(&arg)?, &arg)?,
            "--timeout-ms" => {
                options.timeout = Duration::from_millis(parse_num(&value(&arg)?, &arg)?);
            }
            "--slow-ms" => {
                options.slow = Duration::from_millis(parse_num(&value(&arg)?, &arg)?);
            }
            "--max-bytes" => options.max_bytes = parse_num(&value(&arg)?, &arg)?,
            "--jobs" => options.jobs = Some(parse_num(&value(&arg)?, &arg)?),
            "--languages" => {
                options.languages = Some(
                    value(&arg)?
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                );
            }
            "--extract" => options.extract = true,
            "--case-dir" => options.case_dir = Some(PathBuf::from(value(&arg)?)),
            "-h" | "--help" => {
                return Err("see the usage in examples/grammar_stress.rs".to_owned());
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            path => options.paths.push(PathBuf::from(path)),
        }
    }
    if options.paths.is_empty() {
        return Err("give at least one corpus file or directory".to_owned());
    }
    Ok(options)
}

fn parse_num<T: std::str::FromStr>(text: &str, flag: &str) -> Result<T, String> {
    text.parse()
        .map_err(|_| format!("{flag}: '{text}' is not a valid number"))
}

/// Reads every regular file under the given paths, sorted for a stable order.
fn load_corpus(options: &Options) -> Vec<CorpusFile> {
    let mut files = Vec::new();
    for root in &options.paths {
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() == 0 || meta.len() > options.max_bytes {
                continue;
            }
            if let Ok(bytes) = std::fs::read(entry.path()) {
                files.push(CorpusFile {
                    name: entry.path().to_string_lossy().replace('\\', "/"),
                    bytes,
                });
            }
        }
    }
    files
}

fn stress_language(
    key: &str,
    corpus: &[CorpusFile],
    options: &Options,
    counters: &Counters,
    findings: &Mutex<Vec<Finding>>,
) {
    let language = ts_provider::language(key);
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .expect("every compiled grammar matches the linked tree-sitter ABI");
    let case_path = options
        .case_dir
        .as_ref()
        .map(|dir| dir.join(format!("{key}.bin")));
    let mut ctx = ParseCtx {
        parser: &mut parser,
        key,
        timeout: options.timeout,
        slow: options.slow,
        case_path: case_path.as_deref(),
        counters,
        findings,
    };

    for file in corpus {
        let seed =
            options.seed ^ fnv1a(key.as_bytes()) ^ fnv1a(file.name.as_bytes()).rotate_left(17);
        let mut rng = Rng::new(seed);
        ctx.run(&file.bytes, &format!("{} original", file.name), &mut rng);
        for m in 0..options.mutations {
            let (mutated, how) = mutate(&file.bytes, corpus, &mut rng);
            ctx.run(
                &mutated,
                &format!("{} seed={seed} mutation={m} {how}", file.name),
                &mut rng,
            );
        }
    }
}

struct ParseCtx<'a> {
    parser: &'a mut Parser,
    key: &'a str,
    timeout: Duration,
    slow: Duration,
    case_path: Option<&'a Path>,
    counters: &'a Counters,
    findings: &'a Mutex<Vec<Finding>>,
}

impl ParseCtx<'_> {
    /// Parses `input`, walks the tree, then applies one random edit and
    /// re-parses incrementally from the edited tree.
    fn run(&mut self, input: &[u8], case: &str, rng: &mut Rng) {
        let Some(mut tree) = self.parse(input, None, case) else {
            return;
        };
        self.walk(&tree);

        let (edited, edit) = random_edit(input, rng);
        tree.edit(&edit);
        if let Some(new_tree) = self.parse(&edited, Some(&tree), &format!("{case} +incremental")) {
            self.walk(&new_tree);
            // Exercises the range diff between the two trees.
            let _ = tree.changed_ranges(&new_tree).count();
        }
    }

    fn parse(&mut self, input: &[u8], old: Option<&Tree>, case: &str) -> Option<Tree> {
        if let Some(path) = self.case_path {
            // Best effort: losing a case file must not stop the run.
            if let Ok(mut f) = std::fs::File::create(path) {
                let _ = f.write_all(input);
            }
        }
        let started = Instant::now();
        let deadline = started + self.timeout;
        let mut timed_out = false;
        let mut progress = |_: &tree_sitter::ParseState| {
            if Instant::now() > deadline {
                timed_out = true;
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let len = input.len();
        let tree = self.parser.parse_with_options(
            &mut |offset, _| &input[offset.min(len)..],
            old,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        );
        let elapsed = started.elapsed();
        self.counters.parses.fetch_add(1, Ordering::Relaxed);
        if timed_out {
            // A cancelled parse leaves state behind; reset before reuse.
            self.parser.reset();
            self.report("HANG", case, elapsed);
            return None;
        }
        if elapsed >= self.slow {
            self.report("SLOW", case, elapsed);
        }
        tree
    }

    fn report(&self, kind: &'static str, case: &str, elapsed: Duration) {
        let finding = Finding {
            kind,
            language: self.key.to_owned(),
            case: case.to_owned(),
            millis: elapsed.as_millis(),
        };
        eprintln!(
            "grammar_stress: {kind} grammar={} ms={} case={}",
            finding.language, finding.millis, finding.case
        );
        self.findings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(finding);
    }

    /// Visits every node, reading the fields a consumer reads.
    fn walk(&self, tree: &Tree) {
        let root = tree.root_node();
        if root.has_error() {
            self.counters.error_trees.fetch_add(1, Ordering::Relaxed);
        }
        let mut cursor = tree.walk();
        let mut nodes = 0u64;
        let mut checksum = 0usize;
        loop {
            let node = cursor.node();
            nodes += 1;
            checksum = checksum
                .wrapping_add(node.kind_id().into())
                .wrapping_add(node.end_byte() - node.start_byte())
                .wrapping_add(usize::from(node.is_missing()))
                .wrapping_add(cursor.field_id().map_or(0, |f| f.get().into()));
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    self.counters.nodes.fetch_add(nodes, Ordering::Relaxed);
                    std::hint::black_box(checksum);
                    return;
                }
            }
        }
    }
}

/// Returns a mutated copy of `input` and a short description of the mutation.
fn mutate(input: &[u8], corpus: &[CorpusFile], rng: &mut Rng) -> (Vec<u8>, String) {
    let mut out = input.to_vec();
    let len = out.len();
    let how = match rng.below(8) {
        0 => {
            let flips = 1 + rng.below(8);
            for _ in 0..flips {
                if !out.is_empty() {
                    let at = rng.below(out.len());
                    out[at] = rng.byte();
                }
            }
            format!("flip x{flips}")
        }
        1 => {
            let at = rng.below(len + 1);
            out.truncate(at);
            format!("truncate@{at}")
        }
        2 => {
            let start = rng.below(len + 1);
            let end = (start + 1 + rng.below(256)).min(len);
            out.drain(start..end.max(start));
            format!("delete {start}..{end}")
        }
        3 => {
            let at = rng.below(len + 1);
            let n = 1 + rng.below(16);
            let bytes: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            out.splice(at..at, bytes);
            format!("insert {n}@{at}")
        }
        4 => {
            let start = rng.below(len + 1);
            let end = (start + rng.below(512)).min(len);
            let slice = out[start..end].to_vec();
            let at = rng.below(len + 1);
            out.splice(at..at, slice);
            format!("duplicate {start}..{end}@{at}")
        }
        5 => {
            let other = &corpus[rng.below(corpus.len())];
            let start = rng.below(other.bytes.len() + 1);
            let end = (start + rng.below(1024)).min(other.bytes.len());
            let at = rng.below(len + 1);
            out.splice(at..at, other.bytes[start..end].iter().copied());
            format!("splice {}:{start}..{end}@{at}", other.name)
        }
        6 => {
            let opener = NESTING[rng.below(NESTING.len())];
            let depth = 64 + rng.below(4096);
            let at = rng.below(len + 1);
            let nested: Vec<u8> = opener
                .iter()
                .copied()
                .cycle()
                .take(opener.len() * depth)
                .collect();
            out.splice(at..at, nested);
            format!("nest {}x{depth}@{at}", String::from_utf8_lossy(opener))
        }
        _ => {
            // Two stacked mutations: compound damage reaches deeper states.
            let (first, a) = mutate(input, corpus, rng);
            let (second, b) = mutate(&first, corpus, rng);
            out = second;
            format!("[{a}] then [{b}]")
        }
    };
    (out, how)
}

/// Applies one small random replacement to `input` and returns the new text
/// and the matching tree-sitter edit.
fn random_edit(input: &[u8], rng: &mut Rng) -> (Vec<u8>, InputEdit) {
    let len = input.len();
    let start = rng.below(len + 1);
    let old_end = (start + rng.below(64)).min(len);
    let n = rng.below(24);
    let insert: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
    let mut edited = Vec::with_capacity(len - (old_end - start) + n);
    edited.extend_from_slice(&input[..start]);
    edited.extend_from_slice(&insert);
    edited.extend_from_slice(&input[old_end..]);
    let new_end = start + n;
    let edit = InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point_at(input, start),
        old_end_position: point_at(input, old_end),
        new_end_position: point_at(&edited, new_end),
    };
    (edited, edit)
}

/// Row and byte column of `offset`, as tree-sitter counts them.
fn point_at(text: &[u8], offset: usize) -> Point {
    let before = &text[..offset];
    let row = before.iter().filter(|&&b| b == b'\n').count();
    let column = before
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(offset, |nl| offset - nl - 1);
    Point { row, column }
}

/// Runs each corpus file's own extractor on the original text and on mutated
/// variants, covering the Rust walk on top of the C parse.
fn stress_extractors(corpus: &[CorpusFile], options: &Options, counters: &Counters) {
    let registry = LanguageRegistry::new();
    corpus.par_iter().for_each(|file| {
        let Some(extractor) = registry.extractor_for_file(&file.name) else {
            return;
        };
        let mut rng = Rng::new(options.seed ^ 0x00e7_0000 ^ fnv1a(file.name.as_bytes()));
        let text = String::from_utf8_lossy(&file.bytes);
        let _ = extractor.extract(&file.name, &text);
        counters.extractions.fetch_add(1, Ordering::Relaxed);
        for _ in 0..options.mutations {
            let (mutated, _) = mutate(&file.bytes, corpus, &mut rng);
            let text = String::from_utf8_lossy(&mutated);
            let _ = extractor.extract(&file.name, &text);
            counters.extractions.fetch_add(1, Ordering::Relaxed);
        }
    });
}

/// FNV-1a, for seeds that stay stable when the grammar or corpus set changes.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}
