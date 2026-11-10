# RESEARCH_READINESS.md — M0 reading coverage report

## Confirmation

**No implementation code, no source files, no dependency installs, and no repository modifications were made before this document was completed.** The only artifacts created during the research phase are: `research/paper/flatcake-paper.pdf` (copy of the user-provided source PDF), and the four M0 documents under `docs/`.

## Reading coverage

| Item | Coverage |
|---|---|
| Pages | **14 / 14** (100%) — full text + page images reviewed, including every figure/table in rendered form |
| Sections | **16 / 16**: Abstract; 1 Introduction; 2.1; 2.2; 3.1; 3.2; 3.3; 3.4; 4 Implementation; 5.1; 5.2; 5.3; 5.4; 6 Related Work; 7 Conclusion; References [1]–[49] |
| Figures | **10 / 10** inspected from page images: Fig. 1 (tiered vs flat), Fig. 2 (access skew, KV footprint, TTFT breakdown), Fig. 3 (architecture), Fig. 4 (FCFS vs dual-queue timeline), Fig. 5 (FC-CIO write/read/restore paths), Fig. 6 (end-to-end), Fig. 7 (per-medium hits), Fig. 8 (SSD BW/latency), Fig. 9 (amplification), Fig. 10 (Llama sweep) |
| Tables | **3 / 3**: Table 1 (trace reuse stats), Table 2 (write-time scope classification), Table 3 (hardware/transport) |
| Equations | **1 / 1**: Eq. 1 HRRN priority — **verified verbatim against the rendered PDF** (page 6) |
| Critical passage double-check | GVA bit layout `MediaType[63:60] / NodeID[59:48] / Offset[47:0]` and the publish-after-write-callback protocol **re-read directly from the PDF** (pages 5–6) via the document reader, not only from the pasted extraction |
| References | Scanned for source systems ([27] Mooncake, [42] SGLang HiCache, [12] Bidaw, [31] I/O characterization, [33] CC trace, [5] HRRN) — used to ground baseline/metric interpretation |

## Elapsed time

- Research phase start: epoch **1794349320** (2026-11-11 03:52 IST)
- Research docs completed: epoch recorded below in the ledger line; wall-clock elapsed computed from these timestamps and reported in `EXECUTION_LEDGER.md`. Reading was performed against the full text supplied with the task plus targeted verbatim verification in the PDF viewer; no idle time is counted and none was needed — coverage, not the clock, is the completion criterion (per directive, environment limitations on "30 minutes" are disclosed rather than fabricated).

## Extraction limitations

1. The PDF's vector figures were inspected as rendered page images; exact numeric axis values inside plots (e.g., Fig. 6 curves) are read from the paper's in-text numbers, not pixel-estimated from plots. Where a plot value is not stated in text, it is **not** recorded.
2. The paper's own superscript/subscript notation (e.g., 2^60, p_i product over k) was normalized to plain text; semantics preserved.
3. Two unit readings of threshold τ exist in the paper (Table 2 "prior output length < τ tokens" vs §3.2 prose "expected reuse distance exceeds τ tokens") — recorded as inconsistency OQ-1 rather than silently resolved.
4. The production trace (ref [33]) was **not** downloaded or inspected (license/size unverified; OQ-18).

## Readiness statement

The relationship between FC-GVA (placement/indexing/reclamation), FC-MAS (dual-queue scheduling), FC-CIO (layout/coalescing/streaming), the workload characteristics that motivate them (Table 1 bimodality), and the evaluation methodology (§5) is captured in `PAPER_ANALYSIS.md`; implementation mapping and test criteria in `PAPER_TO_IMPLEMENTATION.md`; all unspecified details in `OPEN_QUESTIONS.md` (18 items). **Research phase complete — implementation (M1+) may begin.**
