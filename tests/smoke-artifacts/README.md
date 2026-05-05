# Smoke run artefacts

`tests/smoke/run.sh` writes per-run subdirectories here:

```
tests/smoke-artifacts/
└── 2026-05-05T12-00-00Z/
    ├── 01-folder-suggestions.png
    ├── 02-no-runner-banner.png
    ├── 03a-create-folder-prompt.png
    ├── 03b-after-create.png
    ├── 04-runner-unavailable.png
    ├── notes.md             # human-filled UX rough-edge notes
    ├── html-report/         # Playwright HTML report
    └── test-results/        # per-test traces (only on failure)
```

Everything except `.gitignore` and this README is gitignored — see the
sibling `.gitignore` for the rule. The directory itself is checked in
so the smoke runner can write into it without first running `mkdir -p`
inside a clean clone.

See `docs/adrs/0005-e2e-tests-must-be-containerized.md` for why the
smoke harness runs entirely inside Docker Compose.
