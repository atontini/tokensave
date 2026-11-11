#!/usr/bin/env bash
# Builds examples/grammar_stress.rs and every tree-sitter grammar with
# AddressSanitizer, then stress-parses a corpus (#958).
#
# Requires Linux x86_64 or aarch64, rustup, clang with its sanitizer runtime
# (Debian/Ubuntu: clang libclang-rt-dev), the OpenSSL headers
# (libssl-dev, for the openssl-sys dependency) and a nightly toolchain:
#   rustup toolchain install "$ASAN_TOOLCHAIN" --profile minimal
#
# Usage: scripts/sanitizers/asan.sh [--asserts] [--features F] [-- HARNESS ARGS...]
#   --asserts      keep C assert() enabled (drops -DNDEBUG); production builds
#                  compile them out, so an assertion failure is a separate class
#   --features F   cargo features (default: full)
# Anything after `--` goes to the harness; the default corpus is tests/fixtures.
# Environment: ASAN_TOOLCHAIN (default below), CC/CXX (default clang/clang++),
# OUT_DIR_ASAN (default target/asan-run) for the log and the last-case files.
set -euo pipefail

cd "$(dirname "$0")/../.."
toolchain="${ASAN_TOOLCHAIN:-nightly-2026-10-01}"
features="full"
ndebug="-DNDEBUG"
harness_args=()
while (($#)); do
  case "$1" in
    --asserts) ndebug=""; shift ;;
    --features) features="$2"; shift 2 ;;
    --) shift; harness_args=("$@"); break ;;
    *) echo "asan.sh: unknown argument $1" >&2; exit 64 ;;
  esac
done
if ((${#harness_args[@]} == 0)); then
  # ASan slows parsing several times; deep-nesting cases are slow even natively.
  harness_args=(--extract --timeout-ms 60000 --slow-ms 5000 tests/fixtures)
fi

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64 | Linux-aarch64) ;;
  *) echo "asan.sh: AddressSanitizer runs here on Linux x86_64/aarch64 only" >&2; exit 64 ;;
esac
target="$(rustc "+$toolchain" -vV | sed -n 's/^host: //p')"
out="${OUT_DIR_ASAN:-target/asan-run}"
mkdir -p "$out/cases"

# Instrument every C/C++ object cc-rs builds: the tree-sitter runtime, the
# bundled grammars and their external scanners, and the vendored grammars in
# build.rs. Setting CFLAGS replaces the -DNDEBUG default from
# .cargo/config.toml, so it is added back here unless --asserts is given.
san_c="-fsanitize=address -fno-omit-frame-pointer -fno-common -g -O1 $ndebug"
export CC="${CC:-clang}" CXX="${CXX:-clang++}"
export CFLAGS="$san_c" CXXFLAGS="$san_c"
# --target keeps build scripts and proc macros uninstrumented.
export RUSTFLAGS="-Zsanitizer=address -Cforce-frame-pointers=yes -Cdebuginfo=1"
export CARGO_TARGET_DIR="target/asan"

# cc-rs also applies CFLAGS to C tools that build scripts compile and run on
# the host (libsql's rlemon parser generator), and those leak by design.
# LeakSanitizer stays on for the harness run below.
export ASAN_OPTIONS="detect_leaks=0"
start=$(date +%s)
cargo "+$toolchain" build --locked --release --example grammar_stress \
  --target "$target" --no-default-features --features "$features"
build_secs=$(( $(date +%s) - start ))

export ASAN_OPTIONS="abort_on_error=1:detect_leaks=1:detect_stack_use_after_return=1:check_initialization_order=1:strict_string_checks=1:symbolize=1:log_path=$out/asan"
start=$(date +%s)
status=0
"target/asan/$target/release/examples/grammar_stress" --case-dir "$out/cases" "${harness_args[@]}" \
  2>&1 | tee "$out/harness.log" || status=${PIPESTATUS[0]}
run_secs=$(( $(date +%s) - start ))

echo "asan.sh: toolchain=$toolchain features=$features build=${build_secs}s run=${run_secs}s status=$status"
if compgen -G "$out/asan.*" >/dev/null; then
  echo "asan.sh: sanitizer report(s):" >&2
  cat "$out"/asan.* >&2
  echo "asan.sh: the crashing input is the newest file in $out/cases" >&2
fi
exit "$status"
