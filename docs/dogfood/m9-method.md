# M9 dogfood method

T1–T5 retain the M8 rerun fixture, prompts, oracles, provider configuration,
and one-run-per-cell method. Their results remain the only cells in the direct
M8-to-M9 comparison. T6 is a separate probe cell and is never included in that
comparison.

T6 asks for the exact whole-word rename `item` → `product` in
`src/display.rs`, `src/ledger.rs`, and `src/lib.rs`, with no other manual source
or file changes. A new `Cargo.lock` from the requested test command is ignored
as generated output. Its mechanical oracle compares the complete tree to that
exact rename, rejects incomplete or extra changes, and runs the fixture tests
offline while skipping only the known seeded cutoff failure.
`scripts/dogfood.sh --self-test` proves that the pristine fixture fails the T6
oracle, the checked-in reference passes, and incomplete or behavior-changing
edits fail. It also checks that T6 remains in the run records but is excluded
from T1–T5 comparison totals.

After scoring a live T6 cell, the driver invokes `harness sessions undo --yes`
against that cell's real journal and records the exit code and whether the
workspace matches the pristine fixture (apart from a generated `Cargo.lock`).
Only a successful undo with a restored tree is evidence that the run's trusted
edits were reversible. A `bash` mutation, an unavailable session, a refusal, or
any mismatch is recorded as not proven; no provider outcome is inferred from
the self-test. The real-journal self-test also exercises `multi_edit` counts,
approval attribution, and restoration without a provider call.

Live provider runs remain opt-in and require parent coordination. No M9
provider measurements are established by this method document or by the
offline self-test.
