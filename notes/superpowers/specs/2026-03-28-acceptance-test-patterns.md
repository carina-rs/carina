# Acceptance Test Patterns: Consolidation and Coverage

Date: 2026-03-28

## Context

After rapid feature development (for expressions, if/else, modules, state blocks, built-in functions), the acceptance test suite has grown to 94 tests across two providers. The test framework has accumulated duplication and inconsistencies that need cleanup.

## Current State

- 54 awscc single-step tests + 6 multi-step suites (37 custom test scripts)
- 21 aws single-step tests + 3 multi-step suites
- `_helpers.sh` duplicated in 5 directories with identical content
- Two test patterns: auto-discovered (CRUD cycle) and custom tests/ (value verification)

## Decisions

### 1. Consolidate _helpers.sh to a single shared location

**Before**: 5 identical copies at:
```
builtin_functions/tests/_helpers.sh
for_expression/tests/_helpers.sh
if_expression/tests/_helpers.sh
module/tests/_helpers.sh
state_blocks/tests/_helpers.sh
```

**After**: Single file at `shared/_helpers.sh`, loaded via source:
```bash
# Each test script's first line after shebang
source "$(dirname "$0")/../../shared/_helpers.sh"
```

The shared helpers provide: `run_step`, `assert_state_value`, `assert_state_resource_count`, `finish_test`, and the `cleanup` trap.

### 2. Maintain two test patterns (no change)

- **Auto-discovered tests**: `run-tests.sh` finds `.crn` files, runs apply -> plan-verify -> destroy. Validates "creates, is idempotent, and destroys cleanly." No value assertions.
- **Custom test scripts**: `tests/*.sh` in suites that need value verification (for expressions, modules, state blocks, etc.). Used when "works" is not sufficient and specific attribute values must be confirmed.

No new patterns (e.g., `.assertions.sh` files or drift detection tests) will be added.

### 3. Add missing test coverage

#### 3.1 sts_caller_identity (data source read test)

AWS provider has an example at `carina-provider-aws/examples/sts_caller_identity/main.crn` but no acceptance test. Add a test that verifies `read aws.sts.caller_identity {}` returns the expected account info.

Location: `carina-provider-aws/acceptance-tests/sts_caller_identity/`

This tests the `read` (data source) path, which is currently untested in acceptance tests.

#### 3.2 Module attributes_access

`module/attributes_access.crn` exists but is not tested by the module test suite. It tests accessing module output attributes (`net.vpc_id`) to create dependent resources. Add a test script.

Location: `carina-provider-awscc/acceptance-tests/module/tests/attributes_access.sh`

#### 3.3 vpc_full (complex multi-resource)

`carina-provider-awscc/examples/vpc_full/main.crn` demonstrates a complete VPC setup with multiple interdependent resources. Add it as an acceptance test to verify complex dependency graphs work end-to-end.

Location: `carina-provider-awscc/acceptance-tests/vpc_full/`

Note: This may be slow due to NAT Gateway creation. Mark with `.slow` if needed.

### 4. Standardize run.sh template

Multi-step suites with custom test scripts should follow a consistent pattern:

```bash
#!/bin/bash
# Runs all <suite-name> acceptance tests
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FILTER="${1:-}"

TOTAL_PASSED=0
TOTAL_FAILED=0
TESTS_RUN=0

echo "<suite-name> acceptance tests"
echo "════════════════════════════════════════"

for test_file in "$SCRIPT_DIR"/tests/*.sh; do
    [ "$(basename "$test_file")" = "_helpers.sh" ] && continue
    test_name="$(basename "$test_file" .sh)"
    if [ -n "$FILTER" ] && ! echo "$test_name" | grep -q "$FILTER"; then
        continue
    fi
    echo ""
    TESTS_RUN=$((TESTS_RUN + 1))
    if bash "$test_file"; then
        TOTAL_PASSED=$((TOTAL_PASSED + 1))
    else
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
    fi
done

echo ""
echo "════════════════════════════════════════"
echo "Tests run: $TESTS_RUN, $TOTAL_PASSED passed, $TOTAL_FAILED failed"
echo "════════════════════════════════════════"

[ "$TOTAL_FAILED" -gt 0 ] && exit 1
```

Existing run.sh files already follow this pattern. Document it as the standard.

## Implementation Order

| Step | Description | Effort |
|------|-------------|--------|
| 1 | Create `shared/_helpers.sh`, update all test scripts to source it, delete copies | Small |
| 2 | Add `module/tests/attributes_access.sh` test | Small |
| 3 | Add `sts_caller_identity` acceptance test | Small |
| 4 | Add `vpc_full` acceptance test | Medium (may need .slow marker) |

Steps 1-3 can be done in one PR. Step 4 is independent and may require separate handling due to test duration.

## Non-Goals

- CI integration for acceptance tests (too slow)
- Drift detection tests (unit tests cover the differ logic)
- Value assertions for auto-discovered tests (CRUD+idempotency is sufficient for simple resources)
- New test patterns or frameworks
