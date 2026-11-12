use super::super::enum_diagnostic_retains_symbolic;
use carina_core::validation::SymbolicFindingOrigin;

#[test]
fn rich_enum_diagnostic_continues_only_with_recovered_references() {
    let findings = [
        (SymbolicFindingOrigin::Ordinary, "ordinary symbolic error"),
        (SymbolicFindingOrigin::AuthoredHook, "authored hook error"),
        (
            SymbolicFindingOrigin::RecoveredReference,
            "source reference mismatch",
        ),
    ];
    let emitted: Vec<_> = findings
        .iter()
        .filter(|(origin, _)| enum_diagnostic_retains_symbolic(true, *origin))
        .map(|(_, message)| *message)
        .collect();
    assert_eq!(emitted, ["source reference mismatch"]);
    assert!(
        findings
            .iter()
            .all(|(origin, _)| enum_diagnostic_retains_symbolic(false, *origin))
    );
}
