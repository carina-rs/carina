# Carina DSL Language Design Review

Date: 2026-03-27

## Context

Carina is a functional infrastructure management tool with a custom DSL (.crn files). After rapid feature development (for expressions, modules, state blocks, built-in functions, index/field access), this review examines the language design for consistency, identifies gaps, and establishes a direction for future evolution.

## Design Principles (Confirmed)

1. **Static resource graph**: The set of resources is always fully known at plan time. No runtime-determined resource counts.
2. **Expression-based**: `let`, `for`, and (future) `if` are all expressions that produce values. Consistency over special syntax.
3. **Minimal syntax**: Avoid adding keywords or constructs when existing patterns suffice. Don't change working syntax for theoretical purity.

## Decisions

### 1. For expression evaluation model: parse-time expansion (keep)

**Decision**: Maintain parse-time expansion for `for` expressions.

**Rationale**: Carina's core value is "plan shows the complete resource graph." Runtime expansion would break this — resource counts could change between plan and apply if external state changes. Terraform's `count` + data source pattern is a known anti-pattern that creates unstable plans.

**Improvement**: Add **pure function eager evaluation** at parse time. When a `for` iterable is a pure function call whose arguments are all statically known, evaluate it immediately:

```crn
for x in keys({a = 1, b = 2})         # OK: evaluates to ["a", "b"] at parse time
for x in concat(list_a, list_b)        # OK: if list_a and list_b are statically known
for x in keys(vpc.tags)                # Error: depends on runtime value
```

Tracked in: #1196

### 2. Value type separation: staged Expr/Value split

**Decision**: Gradually separate the current `Value` enum into `Expr` (unevaluated expressions) and `Value` (final runtime values).

**Current problem**: The `Value` enum carries 10 variants mixing three lifecycles:
- Final values: String, Int, Float, Bool, List, Map
- Parse artifact: UnresolvedIdent
- Deferred evaluation: ResourceRef, Interpolation, FunctionCall

This forces every pipeline stage (validation, diff, display, state serialization) to handle impossible states like FunctionCall in state attributes.

**Staged approach**:

1. **Phase 1**: Remove `UnresolvedIdent` — resolve fully within the parser. This variant should never escape the parse function.
2. **Phase 2**: Introduce `Expr` enum containing ResourceRef, Interpolation, FunctionCall. Resource attributes become `HashMap<String, Expr>`. The resolver phase converts `Expr` → `Value`.
3. **Phase 3**: Unify `ResourceRef`'s asymmetric `binding_name` / `attribute_name` / `field_path` into a single `AccessPath(Vec<PathSegment>)` within the `Expr` type.

Each phase is independently shippable and testable.

### 3. Add if/else expression

**Decision**: Add `if`/`else` as an expression (not a statement or resource modifier).

**Syntax**:

```crn
# Resource generation (else optional — produces 0 or 1 resource)
let alarm = if enable_monitoring {
  awscc.cloudwatch.alarm { alarm_name = "cpu-high" }
}

# Value expression (else required — value must always be determined)
let instance_type = if is_production {
  "m5.xlarge"
} else {
  "t3.micro"
}

# With module calls
let monitoring = if enable_monitoring {
  monitoring_stack { vpc_id = vpc.vpc_id }
}
```

**Semantics**:
- `if` is an expression, consistent with `for` and `let`
- **Resource-producing if**: `else` is optional. When the condition is false, no resource is generated (same as `for` over empty list producing 0 resources)
- **Value-producing if**: `else` is required. A compile-time error if omitted, because the value must always be determined
- Condition must be a `Bool` value determinable at parse time: literal booleans, variables bound to boolean values, boolean arguments, or pure function calls returning booleans. ResourceRef values (e.g., `vpc.enable_dns`) are rejected because they depend on runtime state. Same restriction as `for` iterables — static resource graph principle.
- `if` expands at parse time, like `for`. When the condition is true, the body is included; when false, it is omitted from the resource graph.

**Grammar addition**:

```pest
if_expr = { "if" ~ expression ~ "{" ~ if_body ~ "}" ~ else_clause? }
else_clause = { "else" ~ "{" ~ if_body ~ "}" }
if_body = { local_let_binding* ~ (read_resource_expr | resource_expr | module_call | expression) }
```

### 4. Module call syntax: no change

**Decision**: Keep current syntax (`name { args }`).

**Rationale**: Changing syntax (e.g., `name(args)` or `use name { args }`) adds friction for users without meaningful benefit. The `import` statement preceding the call provides sufficient context. HCL uses the same pattern successfully.

### 5. Comma rules: no change

**Decision**: Keep current comma rules (required in lists/function args, optional in maps, none in resource blocks).

**Rationale**: The inconsistency exists but causes no practical problems. `carina fmt` normalizes formatting. Unifying would require breaking existing `.crn` files.

### 6. ResourceRef structure: defer to Value type split

**Decision**: Unify `binding_name` / `attribute_name` / `field_path` into `AccessPath` as part of Phase 3 of the Value type separation.

### 7. Nested modules: future work

**Decision**: Not addressed in this review. Tracked in #1053.

## Implementation Priority

| Priority | Item | Effort | Dependencies |
|----------|------|--------|-------------|
| 1 | Pure function eager evaluation in for iterables | Small | None |
| 2 | if/else expression | Medium | None |
| 3 | Value split Phase 1: remove UnresolvedIdent | Small | None |
| 4 | Value split Phase 2: introduce Expr type | Large | Phase 1 |
| 5 | Value split Phase 3: unify AccessPath | Medium | Phase 2 |

Items 1-3 can be done independently and in parallel. Item 4 is a significant refactoring effort affecting all crates.

## Non-Goals

- Changing module call syntax
- Unifying comma rules
- Runtime-determined resource counts (for/if with runtime conditions)
- Pattern matching / destructuring in for bindings
- Mutable variable reassignment
