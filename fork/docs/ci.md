# Fork CI

Two goals: **our changes don't break upstream**, and **upstream changes don't break our changes**. Upstream's checks are
reused untouched (so they improve with every upstream merge), and the fork only adds what upstream can't know about.

## What runs when

| Workflow                                                       | When                         | Checks                                                                                                                                                         |
| -------------------------------------------------------------- | ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ci.yml` (upstream, enabled, never edited)                     | Every PR, every release      | `deno fmt` / `deno lint` / `tsc` / `cargo fmt` / `clippy -D warnings` / `cargo test` over the **whole** repo, fork code included (it's compiled into the app). |
| [`fork-ci.yml`](../../.github/workflows/fork-ci.yml)           | Every PR, every release      | Fork rules + fork tests, below.                                                                                                                                |
| [`fork-release.yml`](../../.github/workflows/fork-release.yml) | Merge into `master`          | Runs both of the above, then builds ([releasing.md](releasing.md)).                                                                                            |
| [`fork-sync.yml`](../../.github/workflows/fork-sync.yml)       | 1st of every month / by hand | Opens a `sync/upstream-*` PR, on which both CIs run against the merged result. Re-applies hooks on conflicted hook files.                                      |

## Fork checks ([`fork/scripts/fork-check.ts`](../scripts/fork-check.ts))

| Check       | Fails when                                                                                                                                                                                           |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `hooks`     | A hook listed in [FORK.md](../../FORK.md#hooks-into-upstream-files) is gone from its upstream file (first code span of the _Hook_ column, matched verbatim). Catches upstream merges dropping hooks. |
| `guard`     | A PR changes an upstream file listed neither under _Hooks into upstream files_ nor _Upstream patches_ in FORK.md. Enforces the fork rules and keeps FORK.md accurate. Skipped on sync PRs.           |
| `preflight` | Release only: public key missing or invalid, update endpoint not this repo.                                                                                                                          |
| `key-match` | Release only: the installer's signature isn't from the key installed copies trust.                                                                                                                   |

Run them locally with `node fork/scripts/fork-check.ts hooks` / `guard origin/master` (Node 22.18+).

## Fork tests

- **TypeScript**: `fork/**/*.test.ts`, Node's built-in runner (`node:test`, `node:assert`), no dependencies:
  `node --test "fork/**/*.test.ts"`. Put tests next to the code they test.
- **Rust**: `#[cfg(test)] mod tests` inside `fork/background/` files. They run in upstream's `cargo test` (the fork
  module is part of the main crate), or alone with `cargo test fork::`.

## Upstream sync

`fork-sync.yml` needs the **`FORK_SYNC_TOKEN`** secret: GitHub's default token can't push the upstream workflow changes
a merge usually brings, and PRs it opens don't trigger CI.

Create it at GitHub → Settings → Developer settings → **Fine-grained tokens**: repository access _only_
`winb1g/Seelen-UI-tweaks`; permissions **Contents**, **Pull requests** and **Workflows**: read and write. Save it as the
repository secret `FORK_SYNC_TOKEN`. Renew it when it expires (the sync run fails with a clear error).

When the sync PR is open:

1. Wait for **Upstream CI** and **Fork CI**. A failing `hooks` check means upstream moved code a hook lived in: re-apply
   the hook and update its FORK.md row.
2. Disable any **new upstream workflow** (FORK.md > Upstream workflows).
3. Merge with **Create a merge commit**, never squash or rebase: that keeps upstream's history, so the next merge only
   brings new commits.

When upstream edits the lines next to a fork hook, the merge conflicts on that file. The run then calls
[`reapply-hooks.ts`](../scripts/reapply-hooks.ts): it takes upstream's version of each conflicted hook file and puts the
hooks back with that file's recipe, and the PR body lists the files it fixed. It leaves files listed under _Upstream
patches_, files deleted upstream, files with no recipe and recipes whose anchor upstream moved. If any conflict remains,
the run fails and its summary lists the files, why, and the commands to resolve them locally.
