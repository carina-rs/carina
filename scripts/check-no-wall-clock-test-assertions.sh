#!/usr/bin/env bash
# Reject stopwatch comparisons and unannotated real-time assertion timeouts in
# Rust tests. Use work/attempt counts, configured durations, or a paused clock.
#
# Escape hatch: // wall-clock-ok: <reason>
# Applies to the immediately following statement/condition only. Retained real
# process/socket hang guards must explain why they need a real clock.
# A missing colon or empty reason is an error, even before harmless code.
#
# This is a syntactic guard, not a Rust type checker: it follows local clock and
# elapsed-duration aliases, but does not expand macros or trace across calls.
# Keep awk POSIX-compatible (macOS awk and CI mawk).
set -euo pipefail
export LC_ALL=C

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
output=$(mktemp)
trap 'rm -f "$output"' EXIT

if [ "$#" -gt 0 ]; then
  files=("$@")
else
  files=()
  while IFS= read -r file; do
    files+=("$file")
  done < <(find "$repo_root" \
    -type d \( -name vendor -o -name target -o -name .git -o -name .worktrees -o -name node_modules -o -path '*/scripts/tests' \) -prune -o \
    -type f -name '*.rs' -print | sort)
fi

for file in "${files[@]}"; do
  awk -v file="$file" '
    # Erase Rust comments and strings before examining tokens or brace scopes.
    function code_only(line, result, i, ch, tail, j, terminator) {
      result = ""
      for (i = 1; i <= length(line); i++) {
        ch = substr(line, i, 1)
        tail = substr(line, i)
        if (block_comment) {
          if (substr(tail, 1, 2) == "/*") { block_comment++; i++ }
          else if (substr(tail, 1, 2) == "*/") { block_comment--; i++ }
        } else if (raw_close != "") {
          if (substr(tail, 1, length(raw_close)) == raw_close) {
            i += length(raw_close) - 1; raw_close = ""
          }
        } else if (in_string) {
          if (ch == "\\") i++
          else if (ch == "\"") in_string = 0
        } else if (substr(tail, 1, 2) == "//") {
          if (tail ~ /^\/\/[[:space:]]*wall-clock-ok([^A-Za-z0-9_-]|$)/) {
            if (tail ~ /^\/\/[[:space:]]*wall-clock-ok:[[:space:]]*[^[:space:]]/) allowed = 1
            else {
              printf "%s:%d: invalid wall-clock-ok marker: a colon and nonempty reason are required\n", file, FNR
              allowed = 0
            }
          }
          break
        } else if (substr(tail, 1, 2) == "/*") {
          block_comment++; i++
        } else if (tail ~ /^r#*"/) {
          match(tail, /^r#*"/)
          terminator = substr(tail, 2, RLENGTH - 2)
          raw_close = "\"" terminator
          i += RLENGTH - 1
          result = result " STRING "
        } else if (ch == "\"") {
          in_string = 1; result = result " STRING "
        } else if (ch == quote && substr(line, i + 2, 1) == quote) {
          i += 2; result = result " CHAR "
        } else if (ch == quote && substr(line, i + 1, 1) == "\\" && substr(line, i + 3, 1) == quote) {
          i += 3; result = result " CHAR "
        } else result = result ch
      }
      return result
    }

    # A clock-bearing initializer can return a container (a socket task or
    # credentials with an expiry). Unrelated methods/functions on that value
    # do not return elapsed time. Preserve direct values, clock methods, and
    # generic wrappers instead of tainting every result computed from them.
    function clock_value_used(expr, name, scan, chunk, start, before, after, method, j, balance, callee, ch) {
      scan = expr
      while (match(scan, "(^|[^A-Za-z0-9_])" name "([^A-Za-z0-9_]|$)")) {
        chunk = substr(scan, RSTART, RLENGTH)
        start = RSTART + (substr(chunk, 1, 1) ~ /[A-Za-z0-9_]/ ? 0 : 1)
        before = substr(scan, 1, start - 1)
        after = substr(scan, start + length(name))
        scan = after
        if (after ~ /^[[:space:]]*\.[[:space:]]*[A-Za-z_][A-Za-z0-9_]*[[:space:]]*\(/) {
          method = after
          sub(/^[[:space:]]*\.[[:space:]]*/, "", method)
          sub(/[^A-Za-z0-9_].*$/, "", method)
          if (method == "join" && clock_container[name]) continue
          if (clock_container[name] && method !~ /^(elapsed|duration_since|saturating_duration_since|as_secs|as_secs_f32|as_secs_f64|as_millis|as_micros|as_nanos|saturating_add|saturating_sub|checked_add|checked_sub|clone|to_owned|unwrap|expect|unwrap_or|unwrap_or_else|into_inner|join)$/) continue
        }
        balance = 0
        callee = ""
        for (j = length(before); j > 0; j--) {
          ch = substr(before, j, 1)
          if (ch == ")") balance++
          else if (ch == "(") {
            if (balance > 0) balance--
            else {
              callee = substr(before, 1, j - 1)
              sub(/[[:space:]]*$/, "", callee)
              if (match(callee, /[A-Za-z_][A-Za-z0-9_:!]*$/)) callee = substr(callee, RSTART, RLENGTH)
              else callee = ""
              break
            }
          }
        }
        if (!clock_container[name]) return 1
        if (callee == "" || callee ~ /^(assert(_eq|_ne)?!|debug_assert(_eq|_ne)?!|Some|Ok|Err|Box::new|std::future::ready|Duration::from_[A-Za-z_]+|std::time::Duration::from_[A-Za-z_]+)$/) return 1
      }
      return 0
    }

    # 2 = real time, 1 = Tokio logical time, 0 = no clock dependency.
    function clock_kind(expr, kind, name, scan, receiver, token) {
      kind = 0
      if (expr ~ /tokio::time::Instant::now[[:space:]]*\(/) kind = 1
      scan = expr
      gsub(/tokio::time::Instant::now/, "LOGICAL_NOW", scan)
      if (scan ~ /std::time::Instant::now[[:space:]]*\(/) kind = 2
      gsub(/std::time::Instant::now/, "REAL_NOW", scan)
      if (scan ~ /Instant::now[[:space:]]*\(/ && kind < 2) kind = (logical_instant ? 1 : 2)
      for (name in clock_alias) {
        if (scan ~ (name "::now[[:space:]]*\\(" ) && clock_alias[name] > kind) kind = clock_alias[name]
      }
      if (expr ~ /(^|[^A-Za-z0-9_])(Utc|Local|SystemTime)::now[[:space:]]*\(/) kind = 2
      if (kind == 2) real_clock_seen = 1
      for (name in clocks) {
        if (clock_value_used(expr, name) && clocks[name] > kind) kind = clocks[name]
      }
      # A returned elapsed field can be destructured without calling elapsed()
      # in the test. An explicitly constructed duration is tracked as constant.
      scan = expr
      while (match(scan, /[A-Za-z_][A-Za-z0-9_]*/)) {
        token = substr(scan, RSTART, RLENGTH)
        scan = substr(scan, RSTART + RLENGTH)
        if (kind == 0 && !constant[token] && scan !~ /^[[:space:]]*:/ &&
            (token ~ /(^|_)elapsed($|_)/ || (real_clock_seen && token ~ /(^|_)(time|instant|deadline)($|_)/))) kind = (paused ? 1 : 2)
      }
      if (expr ~ /\.(elapsed|saturating_duration_since|duration_since)[[:space:]]*\(/ && kind == 0) kind = 2
      return kind
    }

    # Space-separated binding names for declarations, tuple patterns, and
    # plain reassignment. A tuple conservatively inherits all RHS dependencies.
    function binding_names(expr, lhs, count, parts, i, name, names) {
      lhs = expr
      if (lhs ~ /^[[:space:]]*let[[:space:]]+/) {
        sub(/^[[:space:]]*let[[:space:]]+(mut[[:space:]]+)?/, "", lhs)
        if (lhs ~ /^\(/) {
          if (lhs !~ /\)[[:space:]]*(:[^=]+)?=/) return ""
          sub(/^\(/, "", lhs)
          sub(/\)[[:space:]]*(:[^=]+)?=.*/, "", lhs)
          count = split(lhs, parts, ",")
          names = ""
          for (i = 1; i <= count; i++) {
            name = parts[i]
            sub(/^[[:space:]]*(mut[[:space:]]+)?/, "", name)
            sub(/[[:space:]]*$/, "", name)
            if (name ~ /^[A-Za-z_][A-Za-z0-9_]*$/ && name != "_") names = names " " name
          }
          sub(/^[[:space:]]+/, "", names)
          return names
        }
        if (lhs !~ /^[A-Za-z_][A-Za-z0-9_]*[[:space:]]*(:[^=]+)?=/) return ""
      } else {
        sub(/^[[:space:]]+/, "", lhs)
        if (lhs !~ /^[A-Za-z_][A-Za-z0-9_]*[[:space:]]*=[^=]/) return ""
      }
      sub(/[^A-Za-z0-9_].*$/, "", lhs)
      return (lhs == "_" ? "" : lhs)
    }

    function analyze(expr, line, kind, assignment, name, rhs, guard, comparison, dependency, selected, scope, sleep_header, failure, arm, blocking_guard, bindings, names, count, binding_index) {
      if (!in_function || expr ~ /^[[:space:]]*$/) return
      if (expr ~ /tokio::time::pause[[:space:]]*\(/ || expr ~ /\.start_paused[[:space:]]*\([[:space:]]*true/) paused = 1
      if (expr ~ /select[[:space:]]*!/) select_allow[depth + 1] = allowed
      selected = 0
      for (scope in select_allow) if (scope + 0 <= depth && scope + 0 > selected) selected = scope + 0
      sleep_header = (selected && expr ~ /(^|[^A-Za-z0-9_])sleep(_until)?[[:space:]]*\(/ && expr ~ /=>/)
      failure = (expr ~ /(panic|assert(_eq|_ne)?|unreachable|bail|fail)[[:space:]]*!/ || expr ~ /(^|[^A-Za-z0-9_])Err[[:space:]]*\(/)
      arm = 0
      for (scope in sleep_arm) if (scope + 0 <= depth && scope + 0 > arm) arm = scope + 0
      if (!paused && failure && ((sleep_header && !allowed && !select_allow[selected]) || (arm && !allowed && !sleep_arm[arm]))) {
        printf "%s:%d: wall-clock test assertion: select sleep arm can fail\n", file, line
      }
      if (sleep_header && expr ~ /=>[[:space:]]*$/) sleep_arm[depth + 1] = (allowed || select_allow[selected])
      bindings = binding_names(expr)
      dependency = expr
      if (bindings != "") sub(/^[^=]*=[[:space:]]*/, "", dependency)
      kind = clock_kind(dependency)
      last_clock_kind = kind
      # A struct initializer contains timestamps without being a timestamp
      # itself. Preserve this distinction when its last field carries a clock.
      if ((depth in pending_binding) && expr ~ /^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*[[:space:]]*:/) pending_container[depth] = 1
      # Blocks split the lexical statement at each brace. Remember the clock
      # dependency for every enclosing initializer until its matching close.
      for (scope in pending_binding) {
        if (scope + 0 <= depth && kind > pending_clock[scope]) pending_clock[scope] = kind
      }
      comparison = (expr ~ /[<>!=]=|[<>]/ || expr ~ /assert(_eq|_ne)?[[:space:]]*!/)
      # Type parameters in a declaration are not clock comparisons.
      if (expr ~ /^[[:space:]]*let[[:space:]]/ && expr !~ /assert.*!/ && expr !~ /[<>]=|[[:space:]][<>][[:space:]]/) comparison = 0
      guard = (expr ~ /((tokio::)?time::timeout(_at)?|(^|[^A-Za-z0-9_:])timeout)[[:space:]]*\(/)
      # Blocking OS waits remain real even when Tokio time is paused.
      blocking_guard = (expr ~ /(\.recv_timeout|([.]|::)wait_timeout(_while)?)[[:space:]]*\(/)
      if (!allowed && (blocking_guard || (kind == 2 && comparison) || (kind == 1 && comparison && !paused) || (guard && !paused))) {
        gsub(/[[:space:]]+/, " ", expr)
        printf "%s:%d: wall-clock test assertion: %s\n", file, line, expr
      }
      if (bindings != "") {
        rhs = dependency
        count = split(bindings, names, " ")
        for (binding_index = 1; binding_index <= count; binding_index++) {
          name = names[binding_index]
          constant[name] = (rhs ~ /^((std::time::)?Duration::|Cell::new|[0-9])/)
          clock_container[name] = 0
          if (kind && !constant[name]) clocks[name] = kind
          else delete clocks[name]
        }
      }
    }

    function flush() {
      analyze(statement, statement_line)
      statement = ""; statement_line = 0; allowed = 0
    }

    BEGIN {
      quote = sprintf("%c", 39)
      test_file = (file ~ /(^|\/)tests\// || file ~ /(^|\/)(tests|[^\/]+_tests|[^\/]+_test)\.rs$/)
    }

    {
      line = code_only($0)
      if (line ~ /use .*(Instant|Utc|Local|SystemTime)[[:space:]]+as[[:space:]]+/) {
        alias = line; sub(/^.*(Instant|Utc|Local|SystemTime)[[:space:]]+as[[:space:]]+/, "", alias)
        sub(/[^A-Za-z0-9_].*$/, "", alias)
        clock_alias[alias] = (line ~ /tokio::time/ ? 1 : 2)
      }
      if (line ~ /use .*time::.*Instant/ && line !~ /Instant[[:space:]]+as/) logical_instant = (line ~ /tokio::time/)
      if (line ~ /#[[:space:]]*\[cfg\((any\(|all\()?test([,)])/) cfg_test = 1
      # A cfg(test) field does not make the following impl a test scope.
      if (line ~ /^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?[A-Za-z_][A-Za-z0-9_]*[[:space:]]*:/) cfg_test = 0
      if (line ~ /#[[:space:]]*\[(tokio::)?test([[:space:]]*\(|\])/) test_attribute = 1
      if (line ~ /start_paused[[:space:]]*=[[:space:]]*true/) next_paused = 1
      if (line ~ /(^|[^A-Za-z0-9_])fn[[:space:]]+[A-Za-z_]/ && !in_function) {
        pending_function = 1
        function_is_test = (test_file || test_depth || test_attribute || cfg_test)
      }
      for (i = 1; i <= length(line); i++) {
        ch = substr(line, i, 1)
        if (statement_line == 0 && ch !~ /[[:space:]]/) statement_line = FNR
        if (ch == "(") parens++
        if (ch == ")") parens--
        if (ch == "{") {
          block_binding = (in_function ? binding_names(statement) : "")
          block_dependency = statement
          if (block_binding != "") sub(/^[^=]*=[[:space:]]*/, "", block_dependency)
          block_clock = (in_function ? clock_kind(block_dependency) : 0)
          flush()
          depth++
          if (block_binding != "") {
            pending_binding[depth] = block_binding
            pending_clock[depth] = block_clock
            pending_container[depth] = 0
          }
          if (pending_function) {
            if (function_is_test) {
              in_function = 1; function_depth = depth; paused = next_paused; real_clock_seen = 0
              for (name in clocks) delete clocks[name]
              for (name in constant) delete constant[name]
              for (name in clock_container) delete clock_container[name]
              last_clock_kind = 0
            }
            pending_function = test_attribute = next_paused = cfg_test = 0
          } else if (cfg_test) { test_depth = depth; cfg_test = 0 }
        } else if (ch == "}") {
          flush()
          if (depth in pending_binding) {
            count = split(pending_binding[depth], closing_names, " ")
            for (binding_index = 1; binding_index <= count; binding_index++) {
              name = closing_names[binding_index]
              if (pending_clock[depth] > 0) {
                clocks[name] = pending_clock[depth]
                constant[name] = 0
                clock_container[name] = (pending_container[depth] || last_clock_kind == 0)
              }
            }
            delete pending_binding[depth]
            delete pending_clock[depth]
            delete pending_container[depth]
          }
          delete select_allow[depth]
          delete sleep_arm[depth]
          if (in_function && depth == function_depth) in_function = paused = 0
          if (test_depth == depth) test_depth = 0
          depth--
        } else {
          statement = statement ch
          if (ch == ";") { flush(); cfg_test = 0 }
        }
      }
      statement = statement " "
    }
    END { flush() }
  ' "$file" >>"$output"
done

if [ -s "$output" ]; then
  echo "Tests must assert work, outcomes, or virtual time instead of wall-clock speed:" >&2
  cat "$output" >&2
  echo 'For an unavoidable real-time hang guard, annotate its statement with // wall-clock-ok: <reason>.' >&2
  exit 1
fi

echo "OK: no unannotated wall-clock test assertions."
