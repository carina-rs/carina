# Module-Aware Plan Display

## Background

The YAPC Fukuoka 2025 talk "Why modularization of infrastructure code is difficult" identifies key differences between infrastructure and application code:

- **Details are the concern** — abstracting away details hurts rather than helps
- **White-box usage** — users need to understand module internals
- **Value definition and usage locations diverge** — makes actual configuration hard to grasp. Investigating "which resource uses this CIDR?" requires mentally unwinding the module call chain from usage site back to definition site. Furthermore, the search often starts with "which directory is this even in?", adding a file-system navigation step before the value lookup.
- **Internal structure visibility is the priority** — over abstraction and layering

Carina already has `ModularPlan` infrastructure in `carina-core/src/plan.rs` (`ModuleSource`, `group_by_module()`, `display_by_module()`) and module metadata on resources (`_module`, `_module_instance`), but the CLI plan display does not use them.

## Goal

Improve plan output so that when modules are used, the output clearly shows:

1. Which module each resource belongs to (module boundaries)
2. All attributes of every resource, including those inside modules (no hiding)
3. Which values came from module arguments (value traceability)

## Design

### 0. Plan Summary Header

When modules are used, display a structured summary at the top of plan output before the detailed execution plan. This provides a quick overview of what each module produces, enabling fast review of large plans.

#### Display format

```
Plan Summary:
  root.crn
    ~ awscc.s3.bucket
  modules/network (instance: net, source: modules/network/main.crn)
    + awscc.ec2.vpc, + awscc.ec2.subnet ×3, + awscc.ec2.nat_gateway
  modules/monitoring (instance: mon, source: modules/monitoring/main.crn)
    + awscc.cloudwatch.metric_alarm ×2

  7 to add, 1 to change, 0 to destroy.

─────────────────────────────────────

Execution Plan:
  (detailed plan with module boundary grouping)
```

#### Nested modules

When a module calls another module (e.g., `web_infra` calls `network`), the summary shows the nested structure to preserve white-box visibility of which inner module generates which resources.

```
Plan Summary:
  root.crn
    ~ awscc.s3.bucket
  modules/web_infra (instance: web, source: modules/web_infra/main.crn)
    modules/network (instance: web.net, source: modules/network/main.crn)
      + awscc.ec2.vpc, + awscc.ec2.subnet ×3
    + awscc.ecs.service ×2

  8 to add, 1 to change, 0 to destroy.
```

Each nesting level adds one level of indentation. Resources that belong directly to an outer module (not delegated to an inner module) appear at the outer module's indentation level.

#### Rules

- Group resources by module, with root-level resources listed under the source `.crn` file name
- Each module shows instance name and source file path
- Nested modules are displayed with hierarchical indentation, showing the full module call tree
- Same resource types within a module are collapsed with `×N` suffix
- Each resource is prefixed with its effect symbol (`+`, `~`, `-`, `-/+`, `<=`, `<-`, `x`, `->`)
- The summary total line matches the existing `PlanSummary` format
- When no modules are used (all resources are root-level), still show the summary but without module grouping — just the source file and resource list
- A `--summary` CLI flag shows only the summary section without the detailed execution plan

#### Implementation

- Use `ModularPlan.group_by_module()` to partition effects by module source
- Render the summary section before calling existing `format_plan()` logic
- For `×N` collapsing: group consecutive effects of the same resource type and effect kind within a module
- `--summary` flag short-circuits after rendering the summary, skipping the detailed plan

### 1. Module Boundary Display

Group resources by module in plan output. Resources from a module are visually grouped with a header showing the module name and instance name.

```
Execution Plan:

  module: network (instance: net)

    + awscc.ec2.vpc net.vpc
        cidr_block: "10.0.0.0/16"
          │
          └─ + awscc.ec2.subnet net.subnet
                vpc_id: net.vpc.vpc_id
                availability_zone: "ap-northeast-1a"

  + awscc.ec2.security_group sg
      vpc_id: net.vpc.vpc_id

Plan: 3 to add, 0 to change, 0 to destroy.
```

**Implementation:** Use existing `ModularPlan.group_by_module()` in `format_plan()`. Root resources display without a module header (same as current behavior). Exact visual formatting (borders, indentation) will be refined during implementation.

**Nested modules:** When a module calls another module (e.g., `web_infra` calls `network`), the plan shows nested module boundaries with hierarchical indentation. This preserves white-box visibility of which inner module generates which resources.

```
Execution Plan:

  module: web_infra (instance: web)

    module: network (instance: web.net)

      + awscc.ec2.vpc web.net.vpc
          cidr_block: "10.0.0.0/16"
            │
            └─ + awscc.ec2.subnet web.net.subnet
                  vpc_id: web.net.vpc.vpc_id

    + awscc.ecs.service web.app
        cluster: "main"

  + awscc.ec2.security_group sg
      vpc_id: web.net.vpc.vpc_id
```

### 2. Full Attribute Visibility

No change to current behavior. All resource attributes are displayed regardless of whether the resource is inside a module. This is intentional — infrastructure code's concern is the details, and hiding them behind module boundaries would be counterproductive.

### 3. Value Traceability

#### Motivation

Three use cases motivate value traceability:

- **Forward (value propagation):** "This argument is passed to the module — which resource attribute does it end up in?"
- **Reverse (value lookup):** "I see this value on a running resource — where in the .crn files was it originally defined?" When the actual value and its definition location are separate, finding "the resource that has this specific value" requires mentally unwinding the module call chain.
- **Directory-first search:** "Which directory/file contains this module?" The search often starts with file-system navigation before the value lookup itself.

#### Approach candidates

Six approaches were considered. They are not mutually exclusive — combinations are possible.

##### A. Module header with args + file path

Always show source file path and argument values at the module boundary header.

```
  module: network (instance: net)
    source: modules/network/main.crn
    args: cidr_block = "10.0.0.0/16", az = "ap-northeast-1a"

    + awscc.ec2.vpc net.vpc
        cidr_block: "10.0.0.0/16" (← arg: cidr_block)
```

- Covers directory-first search (source path visible) and value scanning (args listed)
- Always visible — no extra command needed
- Lightweight, self-contained in plan output

##### B. Per-attribute origin chain

Show the full definition→substitution→usage chain on each attribute.

```
    + awscc.ec2.vpc net.vpc
        cidr_block: "10.0.0.0/16"
                    └─ defined at main.crn:12 → network(cidr_block) → modules/network/main.crn:3
```

- Maximum information density per attribute
- Can be verbose, especially with many module arguments
- Requires tracking source locations (file + line) through the parser and module resolver

##### C. `carina plan --trace <value>` filter command

A dedicated CLI option that filters plan output to show only resources/attributes matching a given value, with full origin chain.

```bash
$ carina plan example.crn --trace "10.0.0.0/16"

  "10.0.0.0/16" found in:

    main.crn:12       import "modules/network" { cidr_block = "10.0.0.0/16" }
      ↓ arg: cidr_block
    modules/network/main.crn:3   awscc.ec2.vpc.cidr_block
```

- Default plan output stays clean; detailed tracing is opt-in
- Intuitive grep-like workflow
- Requires building an origin index but only when `--trace` is requested

##### D. Grep-friendly inline comments

Embed file path and origin as trailing comments on each attribute line.

```
  module: network (instance: net)  # modules/network/main.crn

    + awscc.ec2.vpc net.vpc
        cidr_block: "10.0.0.0/16"  # ← arg:cidr_block @ main.crn:12
```

- `grep "10.0.0.0/16"` hits the line and origin is on the same line
- Works with existing Unix toolchain (grep, awk, etc.)
- Risk of lines becoming too long or cluttered

##### E. `--verbose` mode for progressive disclosure

Default plan output uses approach A (header with source + args). `--verbose` adds approach B (full origin chains on each attribute).

```bash
$ carina plan example.crn             # Shows A-level detail
$ carina plan example.crn --verbose   # Shows A + B-level detail
```

- Balances simplicity and depth
- Avoids cluttering default output
- Two rendering paths to maintain

##### F. Module call-site summary at plan footer

Append a summary section after the plan showing all module call sites with their arguments and source locations.

```
Execution Plan:
  ...（normal plan output with (← arg: X) annotations）...

Module call sites:
  net = import "modules/network" (main.crn:12)
    cidr_block = "10.0.0.0/16"
    az         = "ap-northeast-1a"

Plan: 3 to add, 0 to change, 0 to destroy.
```

- Plan body stays clean
- Call-site summary provides a scannable index of all module invocations
- Easy to find "which modules use this value" at a glance

#### Chosen approach

TBD — to be decided after evaluating trade-offs.

#### Implementation (common to all approaches)

Regardless of which approach is chosen, the following data structures support value traceability:

- During `expand_module_call()` in `module_resolver.rs`, when substituting argument values into resource attributes, build a mapping: `HashMap<(resource_name, attribute_name), argument_name>`
- For interpolated values (e.g., `"vpc-${env_name}"`), record that the attribute uses the argument (partial origin)
- Store this mapping in `ModularPlan` (new field: `argument_origins`)
- `format_plan()` consults the mapping to render traceability annotations

**Data structure:**

```rust
// In ModularPlan
pub struct ModularPlan {
    pub plan: Plan,
    pub effect_sources: HashMap<usize, ModuleSource>,
    pub module_graphs: HashMap<String, DependencyGraph>,
    pub argument_origins: HashMap<ArgumentOriginKey, String>,  // NEW
}

pub struct ArgumentOriginKey {
    pub module_instance: String,  // e.g., "net"
    pub resource_name: String,    // e.g., "vpc"
    pub attribute_name: String,   // e.g., "cidr_block"
}
```

## Scope

### In scope

- Plan summary header with module-grouped resource overview
- `--summary` CLI flag for summary-only output
- Module boundary grouping in plan output
- Value traceability annotations for module arguments
- Value traceability in `carina module info` output (see "module info vs plan" below)
- Snapshot tests for module-aware plan display (including summary)
- Fixture `.crn` files that use modules

### Out of scope

- Changes to `Value` type
- Lint/warnings for module design quality (cohesion, nesting depth)
- Compact mode module display (can be added later)
- TUI mode module display (can be added later)

## module info vs plan

`module info` and `plan` serve different purposes along three axes:

| Axis | `module info` | `plan` |
|------|--------------|--------|
| **Static vs Dynamic** | Static — no concrete values. "This module accepts `cidr_block: string`" | Dynamic — concrete values bound. "`cidr_block` is `"10.0.0.0/16"`, flows to `net.vpc.cidr_block`" |
| **Scope** | Single module in isolation | All modules expanded into a whole-system view |
| **Usage timing** | Design time — "how do I use this module?" | Execution time — "what will happen when I apply?" |

Value traceability manifests differently in each:

- **`module info`** shows **argument-to-attribute path definitions** — which arguments flow to which resource attributes, without concrete values. This helps module authors and consumers understand the module's internal wiring.

  ```
  $ carina module info modules/network

  Module: network
    Source: modules/network/main.crn

  Arguments:
    cidr_block: string (required)
      → vpc.cidr_block
    az: string (required)
      → subnet.availability_zone

  Resources:
    awscc.ec2.vpc (vpc)
    awscc.ec2.subnet (subnet)
  ```

- **`plan`** shows **concrete values with origin annotations** — the actual values that were passed and where they ended up. This helps operators verify what will be applied.

  ```
    + awscc.ec2.vpc net.vpc
        cidr_block: "10.0.0.0/16" (← arg: cidr_block)
  ```

## Key Files

- `carina-core/src/plan.rs` — `ModularPlan`, `ModuleSource`
- `carina-core/src/module_resolver.rs` — `expand_module_call()`, argument substitution
- `carina-cli/src/display.rs` — `format_plan()`
- `carina-cli/tests/fixtures/plan_display/` — new fixture with modules
- `carina-cli/src/plan_snapshot_tests.rs` — new snapshot tests
