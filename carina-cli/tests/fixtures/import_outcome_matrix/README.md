# Import outcome fixtures

Each directory contains a complete multi-file fixture and its `expected.json`:
fixture metadata, 14 native output columns, and bootstrap/loaded direct signatures.
Imported fixture directories use `module/` so diagnostic replay includes them;
`target/` is reserved for build output and is pruned from replay discovery.
Keys are sorted and JSON is pretty-printed. Edit an expected `value` directly and
review its diff. The test has no expectation update mode.

`main.json` stores every native column measured from genuine main
`66063dd7f96561c274807d6943b5441783ee3d71` with the corrected overlay protocol.
The test rejects unknown classification keys and requires every `main` or
`main-4485` expected value to equal that stored main value. Direct signatures
are measured independently by `signatures()`; there is no copied signature column.

Each column has one classification: `main` (exact main value), `main-4485`
(exact main value containing the pre-existing LSP-only absent-attribute finding),
or `changed:<decision-key>`. The closed decision-key list is:

| Key | Decision |
| --- | --- |
| `user-prohibitions` | The user's full-reach decision: the resolver traverses prohibited directories and failing bridges and issues one complete set per root use. CLI validate, lint, and caller LSP publish every child's prohibitions. Read failures beside a prohibition or below a prohibited ancestor accompany the prohibitions in category-first order. JSON and LSP use the same non-null root use/call anchor. Ordinary findings retain main's values; #4485 stays separate. |
| `4398-empty-block` | #4398 rejects an authored empty exports block, which main accepted because its export-parameter list was empty. Separate single-use controls distinguish block presence from repeated-use multiplicity. |
| `4398-placement` | #4398: root evidence owns placement errors and their evidence locations. Arguments, attributes, and require establish module evidence, including empty blocks. |
| `4398-importability` | #4398: an importable directory has .crn files and no root evidence; argument/output entry counts do not determine importability. Root evidence suppresses imported body checks. |
| `A` | Admit an empty interface; keep the reviewed bare module hover and extra-argument finding. Whole changed cells may also expose the unchanged LSP-only #4485 rule. |
| `C` | Report an empty directory as the typed no-configuration-files error; an uncalled import has no use anchor. |

Unreadable siblings are created at runtime: byte 0xff on disk with U+FFFD for
the editor, or mode 000 with known source text. The faithful disk-error epoch
leaves them unopened; the all-open epoch supplies that text. Real CLI processes
and the framed in-process production LSP server measure every column. Editor
overlays reach disk before diagnostics are requested. Nested negative calls also
republish the root caller and await its diagnostics. Overlays are restored, and
retained failing directories regain readable permissions. Mode-000 fixtures are
skipped with a clear message on root or non-Unix runners.

Remeasure genuine main and HEAD with [the measurement tool](../../../../scripts/measure-import-outcomes.py)
using saved binary pairs and an explicit output directory outside the repository.
Pass `--refresh-main-baselines` to regenerate `main.json` from the measured main
cells after a fixture path change. The saved binary hashes and baseline revision
must match; candidate output is never used for these baselines.
The stored baselines cover 200 fixtures. Empty-interface body expectations derive
from genuine main's equivalent declared-argument modules. Added shapes cover two
uses of one prohibited directory, two prohibited children, body-failing and
unreadable bridges, prohibited parents with prohibited or unreadable children,
and an uncalled no-root prohibition with a body error. Each meaningful shape has
called and uncalled controls, with direct and nested routes.

The permanent test compares prohibition counts, order, messages, and both range
endpoints between JSON and LSP. For actual partial reads beside prohibitions,
it also compares the captured IO messages, counts, and ranges. Opening the unreadable sibling does not repair its disk input. Both editor
epochs retain main's handling of document buffers and imported disk reads. Native text, provenance receipts, classification audits,
and PR review ledgers belong in the caller-selected external evidence directory.

The four deterministic matrix shards collect every differing cell before failing. Added controls isolate unrelated unreadable cousins in both import orders, clean descendants of prohibited parents, resolver-only body failures behind main's boundary gate, and empty blocks. Argument/reference overlays preserve and call both aliases in duplicate-use fixtures.

`associated_io` declares that the fixture's unreadable inputs are beside a prohibition or below a prohibited ancestor. These rows require cross-transport IO count, text, and range parity. Ordinary IO keeps the independently measured main value in every column, including main's different CLI/LSP reach; every native cell is still compared exactly. A clean bridge's unreadable child or cousin does not become associated merely because another child is prohibited.

Round 35 adds `empty_exports_require` and `sibling_exports_require`, each direct/nested and called/uncalled. Their ordinary constraints and consumer gates come from freshly measured genuine main; only the recorded prohibitions and retired module-not-found findings change.
