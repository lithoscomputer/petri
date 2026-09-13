# Provenance: vendored Fabro bundles

The directories beside this file are the Fabro workflow bundles of the black
box battery, copied from their source repositories at the revisions
`../bundles.lock.json` records. Everything here is test data. Nothing in it
is linked into Petri.

The files were vendored on 2026-09-08 with `git show <revision>:<path>` from
local checkouts of each source. Every file's SHA-256 matched the lock before
it was committed. `scripts/corpus-fetch-fabro-bundles.sh` verifies the tree
against the lock on every run, and the `petri-fabro-acceptance` test
`reference_version::staged_bundles_match_the_lock_file_or_a_recorded_migration`
checks the digests too.

## Layout

```
bundles/
  PROVENANCE.md               this file
  LICENSES/                   the source repositories' license files
  <id>/<path>                 one bundle, its files at their paths in the source repository
  .sources/                   fetch cache for a source that is not vendored (gitignored; empty today)
```

`<path>` is the file's path in the source repository (the lock's `repo_root`
is `.` for every bundle). File modes follow the lock: one file is executable
(`security-review/.fabro/workflows/security-review/scripts/render_report.py`),
the rest are `0644`. The lock records no symbolic links.

## Bundles

| Bundle | Source repository | Revision | Committed | Working tree at lock time | Files | Bundle hash | Status |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `code-review` | `lithoscomputer/code-review` | `0c81ffb4f68039ca56842ab3328fde03b9714350` | 2026-08-28T20:00:53-04:00 | clean | 80 | `c43a461a05b2962cedd95b165430e3a21942ce8517a8e9d26024ed9ff4f22e8a` | required |
| `security-review` | `lithoscomputer/security-review` | `c14279e9cdac4f7553b5e010718ada1a76e5c1f8` | 2026-08-28T13:33:18-04:00 | clean | 33 | `4b17685a5612fb37e5cdd53c9526793342a562caf5afce36f7d895c4eebba555` | required |
| `fix-ci` | `veniceai/factory` | `1b50f791ac4811aad62788c2a4282a75d1e92422` | 2026-09-05T03:31:21-04:00 | bundle files were read from the working tree and match the committed revision; the repository had unrelated uncommitted changes | 9 | `9c64c9ccff8d1c60b708102b7f5bcb23a9c4e6daaea80887bbc7ae972536174b` | excluded |
| `implement-issue` | `fabro-sh/fabro` | `05ebd0fd1beec214b558f4b478e36bd08b507dc7` | 2026-09-13T08:42:18-06:00 | see lock | 6 | `fec86b59733ac4612cb3cd1e405444a7fa34f3002e8453b0136e2db19fb9d04e` | required |
| `interview` | `fabro-sh/fabro` | `05ebd0fd1beec214b558f4b478e36bd08b507dc7` | 2026-09-13T08:42:18-06:00 | see lock | 3 | `6c52182c0e4e906ba042a7cbcd35c73a0b37ff94d58e33db6572e1e27c749972` | required |

The bundle hash is the lock's `bundle_hash_rule` over the file list. The
status column is the lock's disposition (`../CONTRACT.md`, "Required bundles").

Why the bundles are vendored: `lithoscomputer/code-review` is private and
`veniceai/factory` is internal, so a fetch in hosted CI would have needed a
read-only deploy key per repository. The owner chose (2026-09-08) to copy
the files into this repository instead. The three public sources are vendored
too, so every bundle takes the same path and CI has no bundle network step.

## Licenses

| Source repository | License file at the revision | Copied to |
| --- | --- | --- |
| `fabro-sh/fabro` | `LICENSE.md` (MIT, Qlty Software Inc.) | `LICENSES/fabro-sh-fabro.LICENSE.md` (SHA-256 `9d8408208299ecc4a14d8ec82f7a263a47e0672360d8025b4394c45ed8e1cdbc`) |
| `lithoscomputer/code-review` | none at the repository root; `code-review/.fabro/workflows/code-review/rules/builtin/LICENSE` (Apache-2.0) and `NOTICE.md` cover the rule packs ported from Alibaba OpenCodeReview and are part of the bundle | in the bundle |
| `lithoscomputer/security-review` | none at the revision | not applicable |
| `veniceai/factory` | none at the revision | not applicable |

## Updating a bundle

1. Move the source's `revision` in `../bundles.lock.json` and re-record every
   file's `size`, `sha256`, `mode`, and the `bundle_hash`.
2. Replace the files under `<id>/` from the source at that revision.
3. Run `scripts/corpus-fetch-fabro-bundles.sh`; it must report every bundle
   `ok`.
4. Update this file's table and, when the scenario obligations change,
   `../CONTRACT.md`.
