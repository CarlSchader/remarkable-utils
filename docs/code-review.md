# Repository review findings

Reviewed revision: `f990c6b` (`feat: phase 3 (2 and 3 needs testing)`).

The original findings below are preserved as historical context. References
identify source lines at the reviewed revision, not the current implementation.
P1 denotes high-priority correctness or data-safety issues; P2 denotes other
actionable correctness or reliability issues. Current disposition is recorded
in the table below.

## Resolution status

**15 fixes are merged. Finding 16 remains a draft pending human macOS
validation.** Each fix received regression coverage and code review. Merged
combinations were tested against the then-current `main`, not just in isolated
branches; conflicts and failing integration tests were returned for repair.

| Finding | Fix PR | Status |
|---|---|---|
| 1. One-way deletion direction | [#4](https://github.com/CarlSchader/remarkable-utils/pull/4) | Merged |
| 2. Local-move conflict baseline | [#5](https://github.com/CarlSchader/remarkable-utils/pull/5) | Merged |
| 3. Rebind conflict baseline | [#11](https://github.com/CarlSchader/remarkable-utils/pull/11) | Merged |
| 4. Fingerprint copy sources | [#13](https://github.com/CarlSchader/remarkable-utils/pull/13) | Merged |
| 5. Excluded documents versus deletions | [#7](https://github.com/CarlSchader/remarkable-utils/pull/7) | Merged |
| 6. Symlink write destinations | [#12](https://github.com/CarlSchader/remarkable-utils/pull/12) | Merged |
| 7. Generated download filenames | [#9](https://github.com/CarlSchader/remarkable-utils/pull/9) | Merged |
| 8. Atomic remote replacements | [#15](https://github.com/CarlSchader/remarkable-utils/pull/15) | Merged |
| 9. Atomic local downloads | [#1](https://github.com/CarlSchader/remarkable-utils/pull/1) | Merged |
| 10. Source stability during transfers | [#2](https://github.com/CarlSchader/remarkable-utils/pull/2) | Merged |
| 11. Authoritative mutation/sync listings | [#8](https://github.com/CarlSchader/remarkable-utils/pull/8) | Merged |
| 12. Ambiguous literal glob references | [#3](https://github.com/CarlSchader/remarkable-utils/pull/3) | Merged |
| 13. Side-effect-free dry-run roots | [#14](https://github.com/CarlSchader/remarkable-utils/pull/14) | Merged |
| 14. Stable filesystem archive roles | [#10](https://github.com/CarlSchader/remarkable-utils/pull/10) | Merged |
| 15. Incomplete incremental listings | [#6](https://github.com/CarlSchader/remarkable-utils/pull/6) | Merged |
| 16. Concurrent diagnostic draining | [#16](https://github.com/CarlSchader/remarkable-utils/pull/16) | Draft: native macOS validation required |

The merged fix set at `d117b70` passed 199 automated tests, strict Clippy,
and formatting checks on Linux. The combined tree of that revision and
PR #16 head `c82617f` passed 200 tests and the same checks on Linux, but
**PR #16 is not merged**. No real-tablet tests were performed. These results
are not claims of macOS runtime validation or race-free device snapshots.

### Human validation required for finding 16

Review found that the original streaming-test helper used a pipe API excluded
on Apple targets. The draft replaces it with portable APIs without disabling
macOS coverage. Linux validation and portability review pass, but this
environment lacks the Apple target standard library and a native macOS runtime.

On a native macOS checkout, run the following against the current PR head and
record the results in [PR #16](https://github.com/CarlSchader/remarkable-utils/pull/16):

```sh
gh pr checkout 16
CARGO_BUILD_JOBS=2 nix develop --command cargo test -p libremarkable-utils --all-features ssh::streaming_tests::transfers_drain_and_reap -- --exact --nocapture
CARGO_BUILD_JOBS=2 nix develop --command cargo test --workspace --all-features
CARGO_BUILD_JOBS=2 nix develop --command cargo clippy --workspace --all-features --all-targets -- -D warnings
nix fmt -- --fail-on-change
```

Keep the PR draft until those checks pass and the final revision is reviewed.
No physical tablet is needed for these tests. The PR documents the remaining
non-Unix EOF dependency and the distinction between bounded post-exit draining
and timeouts for a still-running remote process.

## Scope and validation

The review focused on sync planning and execution, document operations,
SSH transport, path resolution, and CLI behavior. Reproductions used
synthetic files, pure planners, mock filesystem endpoints, and mock SSH
executables. No real devices were contacted.

Baseline checks passed:

- `cargo test --workspace`: 110 tests passed.
- `cargo clippy --all-features --all-targets -- -D warnings`: passed in the
  Nix development environment.
- `nix fmt -- --fail-on-change`: passed with no changes.

These passing checks do not cover the failures below. The principal testing
gap is multi-run executor and CLI coverage, especially failure handling and
interactions between individually valid actions.

## P1: data safety and correctness

### 1. One-way `--delete` can delete the source

**References:** `libremarkable-utils/src/sync.rs:2912–2945`.

The deletion branches of `decide_pair` ignore whether the affected side is
writable in the selected mode. This affects filesystem↔filesystem and
tablet↔tablet sync.

**Reproduction:** Sync A→B, delete B's copy, then run `sync --delete A B`.
The remaining source copy on A is deleted. Verified with local CLI execution
and the tablet-pair planner.

**Fix and regression test:** Gate all deletion outcomes by direction,
including conflict-policy branches. Test destination-side deletions in both
one-way modes and verify that the source is never deleted.

### 2. `MoveLocal` hides unresolved local edits on the next run

**References:** `libremarkable-utils/src/sync.rs:1462–1478`, `2630–2651`,
`2777–2788`; cached-hash reuse at `199–205`.

When a local document is edited and its tablet counterpart is renamed and
edited, the plan can contain `MoveLocal` followed by a conflict. The executor
records the current local size/mtime alongside the old agreed hash. The next
run reuses that hash and considers the unresolved local edit unchanged.

**Evidence:** Actual filesystem/state execution followed by a second planner
pass produced `Download` over the local edit under the default `skip` policy.

**Fix and regression test:** Re-key the archive without advancing the local
comparison baseline. Run the move/conflict scenario twice and ensure the
conflict persists until explicitly resolved.

### 3. `Rebind` turns a skipped conflict into a later overwrite

**References:** `libremarkable-utils/src/sync.rs:1593–1612`, `2726–2736`.

If a mapped tablet UUID disappears and a same-name replacement has different
content, while the local file also changed, the planner emits `Rebind` and a
conflict. Executing the rebind advances the remote timestamp before the
replacement's contents have been reconciled.

**Evidence:** After executing the state-only rebind, the next two-way plan
emitted `UpdateRemote`, overwriting the replacement under default `skip`.

**Fix and regression test:** Preserve an unresolved remote-change marker or
comparison baseline until reconciliation succeeds. Test two consecutive runs
after replacement plus a concurrent local edit.

### 4. Copy-by-fingerprint sources can be overwritten earlier in the plan

**References:** `libremarkable-utils/src/sync.rs:1653–1666`, `1715`,
`2654–2719`.

Copy sources come from initial snapshots, but preceding transfers can change
them before the copy executes.

**Reproduction:** Tablet `a.pdf` contains H1; local `a.pdf` changes to H2;
new local `z.pdf` contains H1. The planner emits `UpdateRemote(a)` before
`CopyRemote(z, from=a)`. The copy consequently reads H2 while recording H1 as
the agreed hash. The pull-side equivalent emits `Download(a)` followed by
`CopyLocal(a → z)`. Both invalid action sequences were reproduced.

**Fix and regression test:** Exclude sources mutated by the plan, stage their
original contents, or dependency-order the actions. Verify resulting bytes
and recorded hashes in both directions, not only action presence.

### 5. Excluded ambiguous documents are mistaken for deletions

**References:** `libremarkable-utils/src/sync.rs:317–330`, `1592–1593`,
`1997–2020`, `3284–3300`.

Duplicate sibling names exclude both documents from the remote snapshot.
Mapped UUIDs then appear absent, allowing deletion propagation despite the
original documents still existing. Duplicate folder names affect subtrees.

**Evidence:** After mapping `a.pdf`, adding a second tablet document named
`a` caused a pull with `--delete` to plan `DeleteLocal(a.pdf)` plus warning
notes. The same absence interpretation exists in tablet-pair planning.

**Fix and regression test:** Distinguish excluded/unknown entries from
confirmed absence. Block deletion for affected UUIDs and subtrees; test
duplicate document and folder names after an initial successful sync.

### 6. Ignored destination symlinks allow writes outside the sync root

**References:** `libremarkable-utils/src/sync.rs:148–153`, `547–552`,
`672–683`, `3054–3056`.

Snapshots omit symlinks, but writes follow them. An omitted destination is
therefore treated as available even when it redirects to unrelated data.
Symlinked parent directories present the same risk.

**Evidence:** A first local sync into a destination whose `doc.pdf` was a
symlink overwrote the outside target with no conflict or overwrite policy.

**Fix and regression test:** Reject symlink collisions and unsafe parent
components at write time. Test both leaf and parent symlinks, ensuring outside
files remain untouched.

### 7. Generated download names escape the output directory

**References:** `libremarkable-utils/src/client.rs:611–632`, `963–982`.

`download_filename` incorporates remote `visibleName` directly, and
`resolve_destination` joins it without validating components. Traversal
escapes the output directory; absolute names replace the directory entirely.

**Evidence:** A mock document named `../victim` downloaded into `out/`
replaced an existing sibling `victim.pdf`.

**Fix and regression test:** Require generated names and extensions to form
one safe filename component. Reject traversal and absolute names; test both
payload and bundle downloads.

### 8. Interrupted remote updates destroy existing metadata or payloads

**References:** `libremarkable-utils/src/ssh.rs:235–260`;
`libremarkable-utils/src/client.rs:432–441`, `876–891`.

Remote writes use `cat > destination`, truncating the original before the
transfer succeeds. An interrupted rename can leave malformed metadata, and
payload updates can similarly destroy the previous document bytes.

**Evidence:** A simulated failure after three metadata bytes made `rename`
return an error while leaving the original metadata replaced by `"{\n "`.
Subsequent listing omits invalid metadata.

**Fix and regression test:** Stage complete writes in unique sibling files
and atomically replace originals only after verified success. Inject failed
and truncated transfers and verify original bytes remain intact.

### 9. Failed downloads erase existing local backups

**References:** `libremarkable-utils/src/ssh.rs:277–288`.

`download_remote_file` creates/truncates the final destination before checking
SSH completion. A missing remote source is sufficient to erase a good backup;
interruption leaves a partial replacement.

**Evidence:** Downloading a nonexistent mock remote file returned an error
and emptied an existing local backup.

**Fix and regression test:** Stream to a sibling temporary file, verify
successful completion, then atomically replace the destination. Test missing
sources and mid-stream failures against an existing backup.

### 10. Concurrent source edits are archived without being transferred

**References:** `libremarkable-utils/src/sync.rs:3023–3045`, `3054–3061`;
change detection at `2971–2981`.

Filesystem sync reads source bytes, writes the destination, then independently
stats both sides. An intervening source edit gets its new stamp recorded
alongside the destination's older content. That edit is no longer detected.

**Evidence:** A deterministic mock-endpoint reproduction returned old source
bytes, then changed the source before archive recording. The next plan was
empty despite different source and destination contents and sizes.

**Fix and regression test:** Record only a verified transferred source
version. If the source changes during transfer, leave it pending. Test edits
between read and archive recording, including a subsequent destination edit.

### 11. Stale cached names can misdirect destructive operations

**References:** `libremarkable-utils/src/client.rs:109–116`, `700–704`,
`876–891`, `1159–1160`.

Cache validity uses second-resolution mtime plus size. Mutations do not
invalidate cached entries, so a same-length rename within the same second
can leave the old name active for destructive reference resolution.

**Evidence:** With caching enabled, renaming `AAAA` to `BBBB` left the next
listing showing `AAAA`; deleting `AAAA` successfully deleted the renamed
object. The documented fastcheck limitation does not make mutation-side
cache invalidation unnecessary.

**Fix and regression test:** Invalidate or update entries after writes, and
use authoritative metadata for destructive resolution. Test a same-size,
same-second rename followed by listing and deletion using the old name.

### 12. Ambiguous literal glob names broaden destructive selections

**References:** `libremarkable-utils/src/xochitl.rs:413–425`.

`expand_refs` falls back to glob expansion on any exact-resolution error,
including `AmbiguousPath`. This violates exact-name precedence and ambiguity
rejection when a literal name contains glob characters.

**Evidence:** With two documents literally named `vol-*` and another named
`vol-important`, exact resolution rejected ambiguity, but expansion returned
all three UUIDs. Delete and move operations consume this expansion.

**Fix and regression test:** Only genuine not-found errors should trigger
pattern fallback. Test duplicate literal metacharacter names and verify
unrelated matching items are never selected.

## P2: other correctness and reliability issues

### 13. `--dry-run` creates destination roots

**References:** `rmu/src/main.rs:532–557`, `643–681`, `792–813`.

Filesystem roots and tablet folders are created before the dry-run check.
This contradicts the documented promise to change nothing.

**Evidence:** Local CLI execution created a missing destination directory.
A mock SSH execution captured tablet folder metadata, content, and pagedata
writes while `--dry-run` reported nothing to do.

**Fix and regression test:** Represent missing writable roots in the plan
without creating them. Test every endpoint pairing with missing roots and
assert no write operations occur during dry-run.

### 14. Reversing filesystem endpoints misinterprets the archive

**References:** `rmu/src/main.rs:770–779`.

Archive keys are order-independent, but A/B assignment follows argument order
for local↔local and SSH↔SSH. Reversing arguments compares each endpoint against
the other endpoint's archived stamps.

**Evidence:** After a successful local A→B sync, an unchanged two-way dry-run
in the original order was empty; reversing arguments reported both sides
changed.

**Fix and regression test:** Assign stable archive roles independently of
argument order, or explicitly translate stored roles. Test reversed arguments
with unequal per-side timestamps after a transfer.

### 15. Incremental stat failures become successful incomplete listings

**References:** `libremarkable-utils/src/client.rs:109–116`, `166–174`.

The stat script suppresses diagnostics and unconditionally exits zero to
accommodate unmatched globs. Genuine stat failures therefore look like absent
files, and an incomplete index is persisted instead of triggering fallback.
Such listings can bypass conflict checks or feed incorrect deletion plans.

**Evidence:** A mock stat reporting an I/O error made `list_items()` return
`Ok([])` despite valid document files being present.

**Fix and regression test:** Filter unmatched glob literals explicitly and
propagate failures for actual files. Reject incomplete listings for mutation
planning; test whole-listing and partial stat failures.

### 16. Undrained subprocess diagnostics can deadlock transfers

**References:** `libremarkable-utils/src/ssh.rs:193–204`, `218–228`,
`253–262`, `277–288`.

Streaming methods pipe stderr but drain it only after synchronous transfer
I/O completes. If SSH or the remote process fills that pipe first, both
processes can block indefinitely. Stdin-streaming methods also pipe stdout.

**Evidence:** A mock SSH process writing 1 MiB to stderr before stdout caused
`run_checked_bytes` to hang until the reproduction terminated it.

**Fix and regression test:** Drain diagnostics concurrently with transfer
I/O, including stdout where appropriate. Test output larger than pipe
capacity with a bounded completion timeout.
