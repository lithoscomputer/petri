# Decision records

One TOML file per intentional difference between Petri and the pinned
Fabro, or per tracked departure the differential matrix must name. The
file name is the record's `id`. This directory is the decision index the
comparison (`crates/petri/cli/tests/support/fabro/compare.rs`) loads;
`crates/fabro/acceptance/CONTRACT.md` summarizes it and
`crates/fabro/acceptance/DIFFERENTIAL.md` explains how the comparison uses
it. `crates/fabro/acceptance/tests/reference_version.rs` checks every
record.

## Format

```toml
id = "skipped-stages-in-path"          # equals the file name
title = "A skipped stage is a path record in Petri"
scenarios = ["*"]                      # scenario names or globs the record applies to
bundles = ["*"]                        # bundle ids or globs
fabro = "what the pinned Fabro does"
petri = "what Petri does"
user_visible_effect = "what a workflow author or operator notices"
reason = "why the difference is intentional, or why it is tracked"
acceptance = "what the comparison may accept, and when to retire the record"
known_defects = ["assertion name"]   # optional: independent expectations the pinned Fabro fails

[migration]                            # only when a bundle file changed
old_bundle = "file and sha256 in bundles.lock.json"
new_bundle = "file and sha256 under scenarios/"

[[accepts]]                            # zero or more
kind = "path.skipped_stage"            # a difference kind the comparison emits
field = "path[*]"                      # optional glob on the difference's field
```

Rules:

- A record with no `accepts` documents a difference the projection cannot
  show (events, diagnostics, limits). It accepts nothing.
- `accepts` names exact kinds; a field pattern narrows further. A record
  never accepts a whole family of differences: the comparison's kinds are
  listed in `reference_version.rs` (`KINDS`), and an unknown kind fails.
- A tracked departure (not intentional) says so in `reason` and names the
  retirement condition in `acceptance`; example:
  `interview-run-model-migration`.
- A migration names the old and new bundle file digests; the staged
  bundle's other files stay byte-identical to `bundles.lock.json`.
- A baseline defect of the pinned Fabro is never accepted as Petri
  behaviour. The record lists the failed assertion's exact name under
  `known_defects` and names the scenario in `scenarios` (no glob); the
  differential cell reads that list, reports the failure as a known
  baseline defect, and still requires Petri to pass the assertion; the
  coverage report (`scripts/fabro-coverage-report.py`) treats the Fabro
  record's failed assertion as expected and names the record in its note.
  A failed Fabro assertion no record lists fails the cell. A record may
  also let the comparison name the resulting artifact or request
  difference (the retired `fallback-repeated-tool-effect` record did, while
  the reference at `b648291` repeated a tool effect on failover).
- Neither test execution nor a baseline refresh changes a record. A new
  difference needs a new record in the same change that introduces it.
