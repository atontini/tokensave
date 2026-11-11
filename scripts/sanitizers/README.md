# Grammar sanitizer and fuzzing harnesses

TokenSave links about 50 C and C++ tree-sitter grammars and external scanners.
Worker subprocesses contain crashes in them, but they do not detect memory
errors that leave the process running. These harnesses run the grammar C code
under AddressSanitizer (#958). Results and costs are in
[docs/GRAMMAR-SANITIZERS.md](../../docs/GRAMMAR-SANITIZERS.md).

Both harnesses need Linux, `clang` and a nightly Rust toolchain. The pinned
toolchain in `rust-toolchain.toml` is stable and does not support
`-Zsanitizer`.

## AddressSanitizer stress run

[`examples/grammar_stress.rs`](../../examples/grammar_stress.rs) parses every
corpus file with every compiled grammar, not only its own, and with mutated
variants of each file. It also re-parses each input incrementally after an
edit and walks every tree. [`asan.sh`](asan.sh) builds it, the tree-sitter
runtime and all grammars with `-fsanitize=address` and runs it.

```sh
rustup toolchain install nightly-2026-10-01 --profile minimal
scripts/sanitizers/asan.sh                       # full grammars, tests/fixtures, --extract
scripts/sanitizers/asan.sh --asserts             # keep C assert() (production uses -DNDEBUG)
scripts/sanitizers/asan.sh -- --mutations 100 --languages python,bash tests/fixtures src
```

- `ASAN_TOOLCHAIN` selects another nightly. `CC` and `CXX` default to `clang`
  and `clang++`.
- The run log, any sanitizer report (`asan.*`) and the last input of each
  grammar (`cases/<grammar>.bin`) are written to `target/asan-run/`. When
  ASan aborts, the input that crashed is the newest file in `cases/`.
- Exit status 2 means a parse exceeded `--timeout-ms` and was cancelled. The
  `HANG` lines name the grammar and how to rebuild the input (file, seed and
  mutation index). Under ASan, parses run several times slower than in a
  release build, so use a generous timeout.
- The harness also builds without a sanitizer
  (`cargo run --release --example grammar_stress -- tests/fixtures`) to check
  for hangs and crashes on any platform.

The **Grammar sanitizers** workflow
([`.github/workflows/sanitizers.yml`](../../.github/workflows/sanitizers.yml))
runs `asan.sh` on `ubuntu-latest`. It is started by hand from the Actions tab
and uploads `target/asan-run/` when it fails.

## cargo-fuzz target

[`fuzz/`](../../fuzz) is a separate cargo workspace with one libFuzzer target,
`parse`. The first input byte selects the grammar, the next two place an edit,
and the rest is source text. The target parses the text, deletes the edit range
and re-parses incrementally.

```sh
cargo install cargo-fuzz
cd fuzz
# cargo-fuzz instruments Rust code; the C grammars need the flags as well.
export CC=clang CXX=clang++
export CFLAGS="-fsanitize=address,fuzzer-no-link -g -DNDEBUG" CXXFLAGS="$CFLAGS"
cargo +nightly-2026-10-01 fuzz run parse -- -max_total_time=600 -timeout=20
TOKENSAVE_FUZZ_GRAMMAR=bash cargo +nightly-2026-10-01 fuzz run parse   # one grammar
```

Seed the corpus with real code to start from valid syntax:
`mkdir -p corpus/parse && cp ../tests/fixtures/sample.* corpus/parse/`. The
first three bytes of each seed become the grammar selector and the edit
position, and libFuzzer mutates them like the rest. Crashing inputs are saved under `fuzz/artifacts/parse/`;
replay one with `cargo fuzz run parse <file>`.

## Reporting a finding

Open a separate issue for each confirmed crash. Give the grammar key, the
grammar crate and version from `Cargo.lock`, the platform, the sanitizer
report, and the input (attach the `.bin` file or the fuzz artifact). If the
bug is in an upstream grammar, link the upstream report as well.
