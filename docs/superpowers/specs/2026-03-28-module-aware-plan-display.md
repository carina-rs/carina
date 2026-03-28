# Module-Aware Plan Display

## Background

The YAPC Fukuoka 2025 talk "Why modularization of infrastructure code is difficult" identifies key differences between infrastructure and application code:

- **Details are the concern** — abstracting away details hurts rather than helps
- **White-box usage** — users need to understand module internals
- **Value definition and usage locations diverge** — makes actual configuration hard to grasp
- **Internal structure visibility is the priority** — over abstraction and layering

Carina already has `ModularPlan` infrastructure in `carina-core/src/plan.rs` (`ModuleSource`, `group_by_module()`, `display_by_module()`) and module metadata on resources (`_module`, `_module_instance`), but the CLI plan display does not use them.

## Goal

Improve plan output so that when modules are used, the output clearly shows:

1. Which module each resource belongs to (module boundaries)
2. All attributes of every resource, including those inside modules (no hiding)
3. Which values came from module arguments (value traceability)

## Design

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

**Nested modules:** When a module calls another module (e.g., `web_infra` calls `network`), the plan shows the outermost module boundary. Inner module resources appear with their full dot-path prefix (e.g., `web.net.vpc`). Nested module headers are not displayed to avoid deep visual nesting — the dot-path prefix provides sufficient traceability.

### 2. Full Attribute Visibility

No change to current behavior. All resource attributes are displayed regardless of whether the resource is inside a module. This is intentional — infrastructure code's concern is the details, and hiding them behind module boundaries would be counterproductive.

### 3. Value Traceability

Show which resource attribute values originated from module arguments.

```
  module: network (instance: net)

    + awscc.ec2.vpc net.vpc
        cidr_block: "10.0.0.0/16" (← arg: cidr_block)

    + awscc.ec2.subnet net.subnet
        vpc_id: net.vpc.vpc_id
        cidr_block: "10.0.1.0/24"
        availability_zone: "ap-northeast-1a" (← arg: az)
```

**Implementation:**

- During `expand_module_call()` in `module_resolver.rs`, when substituting argument values into resource attributes, build a mapping: `HashMap<(resource_name, attribute_name), argument_name>`
- For interpolated values (e.g., `"vpc-${env_name}"`), record that the attribute uses the argument (partial origin)
- Store this mapping in `ModularPlan` (new field: `argument_origins`)
- `format_plan()` consults the mapping to append `(← arg: X)` annotations

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

- Module boundary grouping in plan output
- Value traceability annotations for module arguments
- Snapshot tests for module-aware plan display
- Fixture `.crn` files that use modules

### Out of scope

- Changes to `Value` type
- Lint/warnings for module design quality (cohesion, nesting depth)
- Changes to `carina module info` command
- Compact mode module display (can be added later)
- TUI mode module display (can be added later)

## Key Files

- `carina-core/src/plan.rs` — `ModularPlan`, `ModuleSource`
- `carina-core/src/module_resolver.rs` — `expand_module_call()`, argument substitution
- `carina-cli/src/display.rs` — `format_plan()`
- `carina-cli/tests/fixtures/plan_display/` — new fixture with modules
- `carina-cli/src/plan_snapshot_tests.rs` — new snapshot tests
