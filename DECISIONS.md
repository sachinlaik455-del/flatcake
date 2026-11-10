# DECISIONS.md — architectural decisions and rationale

| # | Decision | Rationale | Alternatives rejected | Source |
|---|---|---|---|---|
| D1 | Fidelity **Level B** (CPU prototype, real local SSD I/O); Level A only for algorithm unit tests; Level C design-only | Directive Part 2; hardware (16 GB laptop, no CUDA/GDS assumed) makes C impossible now; B keeps real I/O credibility | Jumping to C (hardware unavailable); pure simulator (would mislabel synthetic numbers as measurements) | Directive Part 2, Part 16 |
| D2 | Paper-reported numbers never presented as ours; qualitative mechanism comparison only | Different hardware, different trace; numeric comparison would be fabrication | Scaling paper numbers to laptop | Directive Part 23 |
| D3 | Synthetic workloads only; production trace NOT downloaded | License/size unverified; 16 GB machine; directive forbids inventing trace data | Downloading claude-code-weka-0621 (OQ-18) | Directive Part 11, Part 21 |
| D4 | Stack selection deferred to ENVIRONMENT_AUDIT.md evidence (Rust vs C++ for core; Python for analysis) — decided by toolchain audit, not preference | Directive Part 3 requires audit before selection | — | Part 3 |
| D5 | τ=4096 tokens, L_agent=65536 tokens as defaults | Grounded in paper's footprint math (7.5 KiB/token/rank FP8): 4096-token reply ≈ 30 MiB/rank (small, DRAM-worthy); 65536-token context ≈ 0.5 GiB/rank ≈ Claude Code median | Arbitrary other values | OQ-1, OQ-2 |
| D6 | HRRN priority recomputed at dispatch decisions (lazy), not continuously | Eq. 1 semantics + avoids background resort; documented as reference interpretation | Continuous reorder (unspecified in paper) | OQ-4 |
| D7 | Scope→GPU File: contiguous file chain when session exceeds one pre-allocated file | Paper implies 1 file/scope but gives no size bound for multi-GiB sessions; chaining preserves contiguity per file and is reversible | Unbounded single files (defeats pre-allocation) | OQ-11 |
| D8 | Max 2 concurrent implementation agents; component agents (FC-GVA/FC-MAS/FC-CIO/Testing/Benchmark) with strict file ownership | Directive Part 18 | Full parallel fan-out | Part 18 |
| D9 | EGM plugin personas used at gates: architecture review (M2), audit support (M12) | User instruction "use egm plugins"; fits persona design | Running EGM on every commit (waste) | User directive |
