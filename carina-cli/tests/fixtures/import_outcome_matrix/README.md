# Import outcome fixtures

Each directory contains a complete multi-file fixture and its `expected.json`:
fixture metadata, 14 native output columns, and bootstrap/loaded direct signatures.
Keys are sorted and JSON is pretty-printed. Edit an expected `value` directly and
review its diff. The test has no expectation update mode.

`main.json` stores every native column measured from genuine main
`980dd4e9dea5cce43552b3d1802175f924c83d12` with the corrected overlay protocol.
The test rejects unknown classification keys and requires every `main` or
`main-4485` expected value to equal that stored main value. Direct signatures
are measured independently by `signatures()`; there is no copied signature column.

Each column has one classification: `main` (exact main value), `main-4485`
(exact main value containing the pre-existing LSP-only absent-attribute finding),
or `changed:<decision-key>`. The closed decision-key list is:

| Key | Decision |
| --- | --- |
| `user-prohibitions` | The user's three decisions: replace misleading Module-not-found findings with every prohibition; give CLI validate and caller LSP the same prohibition set/count; locate validate JSON findings at the actual call. Preserve ordinary main findings, their order, and the separate #4485 gap. |
| `4398-placement` | #4398: root evidence owns placement errors and their evidence locations. Arguments, attributes, and require establish module evidence, including empty blocks. |
| `4398-importability` | #4398: an importable directory has .crn files and no root evidence; argument/output entry counts do not determine importability. Root evidence suppresses imported body checks. |
| `4398-readable-prohibitions` | #4398: readable root evidence is monotone; publish its complete prohibition set at the root use/call even beside an unreadable sibling. Preserve main's ordinary findings and readable-sibling signature/UI policy. |
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
using saved binary pairs and an explicit output directory. Classification rationale
and Round 32 evidence live in `round32/` outside the repository. Empty-interface
body expectations derive from genuine main's equivalent declared-argument
modules. The fixture set includes those controls, rejected parents with children,
ordinary/prohibited import mixes, and readable root evidence beside both kinds
of unreadable sibling.
