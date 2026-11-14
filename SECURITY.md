# Security Policy

## Reporting a vulnerability

If you discover a security vulnerability in FlatCake, please report it responsibly:

1. Use [GitHub's private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-for-reporting-and-fixing-security-vulnerabilities) (Security tab → Report a vulnerability).
2. Include: affected component, trigger condition, reproduction steps, and observed impact.
3. Do not open a public issue for security vulnerabilities.

You will receive a response within 5 business days. Confirmed vulnerabilities will be patched and disclosed in a security advisory after a fix is available.

## Scope

This covers the FlatCake KV-cache engine (Rust source in `src/`), its CLI (`src/bin/`), and its build system. The Python plotting scripts (`python/`) are development tools outside the security boundary.

## Known security posture

- Zero external dependencies (supply-chain attack surface: none).
- All I/O uses `FILE_FLAG_NO_BUFFERING` on scope files — unbuffered, 4 KiB-aligned.
- The platform service binds to 127.0.0.1 only with token authentication.
- KV tensor contents never cross the control plane.
- See `docs/final-security-audit.md` for the full audit and `docs/LIMITATIONS.md` for residual risks.
