# github-release-readiness.md — FlatCake release preparation checklist

Date: 2026-11-14. Verdict: **READY WITH BLOCKERS** (license decision pending; P1 platform feature complete but public release authorization not yet given).

## Checklist

| Item | Status | Evidence |
|---|---|---|
| Repository hygiene | **PASS** | Clean tree, 0 untracked files, 0 debug markers (`grep -rn "TODO\|FIXME\|XXX\|HACK" src/` = 0), .gitignore covers all build/test/data artifacts |
| .gitattributes | **NOT TESTED** | File not present; add `* text=auto` for cross-platform line-ending normalization |
| No credentials/tokens/personal paths | **PASS** | git history scanned (grep -p --all for api_key/secret/password/token patterns — 2 false positives in audit prose only); no .env files; no machine-specific paths in tracked files |
| No accidental binaries | **PASS** | No .exe/.dll/.bin in tracked files; target dirs gitignored |
| README | **PASS** | Accurate: purpose, architecture, build, test, demo, benchmarks, layout, status — all verified against actual implementation; no fabricated claims |
| Architecture diagram | **PASS** | ASCII diagram in docs/platform-foundation.md + docs/ARCHITECTURE.md |
| Quickstart | **PASS** | 4-command build+test+demo verified by clean-clone test |
| Feature overview | **PASS** | README + docs/ARCHITECTURE.md cover all implemented capabilities |
| Benchmark methodology | **PASS** | docs/BENCHMARK_METHODOLOGY.md + docs/RESULTS.md with raw JSON |
| Limitations documented | **PASS** | docs/LIMITATIONS.md + docs/final-security-audit.md |
| CI workflow | **PASS** | .github/workflows/ci.yml — Windows runner, build+test+clippy, least-privilege permissions |
| Cross-platform CI | **NOT TESTED** | Linux/macOS runners not validated (NO_BUFFERING is Windows-specific; Linux port requires O_DIRECT adaptation per FUTURE_GDS_INTEGRATION.md) |
| SECURITY.md | **PASS** | Vulnerability reporting guidance; no invented contacts |
| CONTRIBUTING.md | **PASS** | Contribution guidance referencing the test suite |
| LICENSE | **BLOCKED** | Owner decision pending — no license file applied |
| Package identity | **PASS** | Cargo.toml: name=fckv, version=0.1.0, edition=2021, zero deps |
| Dependency audit | **PASS** | Zero external crates (Cargo.toml has no [dependencies]); supply-chain risk: none |
| Git history secrets | **PASS** | Full history scanned; 2 matches were audit prose, not credentials |
| P1–P4 guarantees intact | **PASS** | 129 tests, 0 failures (see docs/final-validation-report.md) |
| Security audit findings remediated | **PASS** | SEC-1 fixed + regression-pinned; SEC-2 disabled; D-A4/D-A5/D-A2/D-A3 fixed; F1-F8 fixed (see docs/final-security-audit.md findings.json) |
| Benchmarks reproducible | **PASS** | benchmarks/run_sweep.sh, run_p3.sh, run_p4.sh — all use committed scripts + configs |
| Docs cross-references | **PASS** | All 12 retained docs verified to exist; no dead references in README |

## Release blockers

1. **LICENSE**: the project owner must select and apply a license (MIT, Apache-2.0, BSD, etc.) before public distribution. No license file exists; Cargo.toml has no `license` field. This is a legal prerequisite, not an engineering one.

## Pre-publication steps (after license)

1. Add LICENSE file + `license` field to Cargo.toml.
2. Set GitHub repository description and topics.
3. Push to GitHub.
4. Create a GitHub release with tag `v0.1.0` and release notes from CHANGELOG.md.

## CI workflow

`.github/workflows/ci.yml` runs on `windows-latest` (the validated platform):
- checkout (actions/checkout@v4)
- rustup stable-gnu toolchain
- `cargo build --release` (zero deps — no cache warming needed)
- `cargo test --release` (129 tests)
- `cargo clippy --release -- -D warnings`
- `./target/release/fckv.exe selftest`

Least privilege: `permissions: contents: read`. No secrets required.
