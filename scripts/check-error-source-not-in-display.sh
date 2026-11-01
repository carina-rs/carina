#!/usr/bin/env bash
# Enforce Carina's terminal-error rule for thiserror declarations.
#
# A non-transparent error must render every diagnostic detail in Display and
# must not expose an embedded error through Error::source(). Otherwise ordinary
# Display loses the source text, or a chain renderer duplicates it. Therefore
# every #[source], #[from], or implicit field named `source` is rejected unless
# the declaration is #[error(transparent)]. The diagnostic says whether the
# format embeds the field (duplication) or omits it (information loss).
#
# This is deliberately a small syntactic scanner, not a Rust parser. It handles
# the declaration shapes used in this repository, including multiline error
# attributes, named and tuple fields, same-line field attributes, implicit
# sources, positional `.field`/`.N` arguments, and tuple indices >= 1. It does
# not expand macros, evaluate cfgs, or inspect hand-written Display/Error impls.
# Context-only errors may opt into caller-owned chain rendering with the
# inline marker `error-chain: caller-renders-sources` on #[error]. Such errors
# must omit their source from Display; duplication is still rejected.
# Keep the awk POSIX-compatible: CI uses mawk and developers use macOS awk.
set -euo pipefail

output_file=$(mktemp)
trap 'rm -f "$output_file"' EXIT

if [ "$#" -gt 0 ]; then
  files=("$@")
else
  files=()
  while IFS= read -r file; do
    files+=("$file")
  done < <(find carina-* -type f -name '*.rs' -not -path '*/target/*' | sort)
fi

for file in "${files[@]}"; do
  awk -v file="$file" '
    function reset_item() {
      error_attr = ""
      error_line = 0
      in_error_attr = 0
      body = ""
      body_started = 0
      saw_delimiter = 0
      paren_depth = 0
      brace_depth = 0
    }

    function attr_mentions_named(field, pattern) {
      pattern = "\\{" field "([}:])"
      if (error_attr ~ pattern) {
        return 1
      }
      pattern = "\\." field "([^A-Za-z0-9_]|$)"
      return error_attr ~ pattern
    }

    function attr_mentions_tuple(position, pattern) {
      pattern = "\\{" position "([}:])"
      if (error_attr ~ pattern) {
        return 1
      }
      pattern = "\\." position "([^0-9]|$)"
      return error_attr ~ pattern
    }

    function report_source(field, embedded, kind) {
      # Explicit context-only errors retain typed sources for a caller-owned
      # chain renderer. The opt-in never permits embedding that source too.
      if (!embedded && error_attr ~ /error-chain: caller-renders-sources/) {
        return
      }
      if (embedded) {
        kind = "duplicates source in Display"
      } else {
        kind = "omits source from Display"
      }
      printf "%s:%d: %s (%s): %s\n", file, error_line, kind, field, error_attr
    }

    function named_field_after(text, start, rest, declaration) {
      rest = substr(text, start)
      # Do not mistake a Rust type path such as io::Error for a named field.
      gsub(/::/, "@@", rest)
      if (match(rest, /(pub([[:space:]]*\([^)]*\))?[[:space:]]+)?[A-Za-z_][A-Za-z0-9_]*[[:space:]]*:/)) {
        declaration = substr(rest, RSTART, RLENGTH)
        sub(/^pub([[:space:]]*\([^)]*\))?[[:space:]]+/, "", declaration)
        sub(/[[:space:]]*:.*/, "", declaration)
        return declaration
      }
      return ""
    }

    function tuple_index_before(text, stop, open, i, ch, p, b, s, a, position) {
      open = index(text, "(")
      if (open == 0 || open >= stop) {
        return -1
      }
      p = b = s = a = position = 0
      for (i = open + 1; i < stop; i++) {
        ch = substr(text, i, 1)
        if (ch == "(") p++
        else if (ch == ")" && p > 0) p--
        else if (ch == "{") b++
        else if (ch == "}" && b > 0) b--
        else if (ch == "[") s++
        else if (ch == "]" && s > 0) s--
        else if (ch == "<") a++
        else if (ch == ">" && a > 0) a--
        else if (ch == "," && p == 0 && b == 0 && s == 0 && a == 0) position++
      }
      return position
    }

    function analyze_item(transparent, scan, offset, token, field, position, embedded, iterations, match_start, match_length) {
      if (error_line == 0 || body == "") {
        return
      }
      transparent = (error_attr ~ /^#\[error\([[:space:]]*transparent[[:space:]]*\)\]/)
      if (transparent) {
        return
      }

      # Explicit #[source] and #[from] fields.
      scan = body
      offset = 0
      iterations = 0
      while (match(scan, /#\[(source|from)\]/)) {
        if (++iterations > 100) {
          printf "%s:%d: checker could not advance past a source attribute: %s\n", file, error_line, error_attr
          return
        }
        # RSTART/RLENGTH are global in POSIX awk; preserve them before helper
        # functions perform their own matches.
        match_start = RSTART
        match_length = RLENGTH
        token = offset + match_start
        field = named_field_after(body, token + match_length)
        if (field != "") {
          embedded = attr_mentions_named(field)
          report_source(field, embedded)
        } else {
          position = tuple_index_before(body, token)
          if (position < 0) {
            position = 0
          }
          embedded = attr_mentions_tuple(position)
          report_source(position, embedded)
        }
        offset += match_start + match_length - 1
        scan = substr(body, offset + 1)
      }

      # thiserror implicitly treats a field literally named `source` as a
      # source. Avoid reporting it twice when it also has an explicit marker.
      if (body ~ /(^|[({,;[:space:]])source[[:space:]]*:/ &&
          body !~ /#\[(source|from)\][[:space:]]*(pub([[:space:]]*\([^)]*\))?[[:space:]]+)?source[[:space:]]*:/) {
        report_source("source", attr_mentions_named("source"))
      }
    }

    function update_depth(text, i, ch) {
      for (i = 1; i <= length(text); i++) {
        ch = substr(text, i, 1)
        if (ch == "(") { paren_depth++; saw_delimiter = 1 }
        else if (ch == ")") paren_depth--
        else if (ch == "{") { brace_depth++; saw_delimiter = 1 }
        else if (ch == "}") brace_depth--
      }
    }

    BEGIN { reset_item() }

    {
      line = $0
      trimmed = line
      sub(/^[[:space:]]*/, "", trimmed)

      if (trimmed ~ /^#\[error\(/) {
        analyze_item()
        reset_item()
        error_line = FNR
        error_attr = trimmed
        in_error_attr = (trimmed !~ /\)\][[:space:]]*(\/\/.*)?$/)
        next
      }

      if (in_error_attr) {
        error_attr = error_attr " " trimmed
        if (trimmed ~ /\)\][[:space:]]*(\/\/.*)?$/) {
          in_error_attr = 0
        }
        next
      }

      if (error_line == 0) {
        next
      }

      if (!body_started) {
        # Attributes between #[error] and the declaration belong to the item,
        # but cannot themselves contain its source field.
        if (trimmed == "" || trimmed ~ /^\/\// || trimmed ~ /^#\[/) {
          next
        }
        body_started = 1
      }

      body = body " " trimmed
      update_depth(trimmed)

      if ((saw_delimiter && paren_depth <= 0 && brace_depth <= 0) ||
          (!saw_delimiter && trimmed ~ /[,;][[:space:]]*(\/\/.*)?$/)) {
        analyze_item()
        reset_item()
      }
    }

    END { analyze_item() }
  ' "$file" >> "$output_file"
done

if [ -s "$output_file" ]; then
  echo "thiserror declarations violate the terminal-error rule:" >&2
  sed 's/^/  /' "$output_file" >&2
  echo >&2
  echo "Embed the error in Display once, remove #[source]/#[from], and add a manual From impl if needed." >&2
  echo "Use #[error(transparent)] only when the wrapper intentionally has no distinct diagnostic." >&2
  exit 1
fi

echo "OK: non-transparent thiserror declarations are terminal diagnostics."
