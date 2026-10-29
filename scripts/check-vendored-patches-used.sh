#!/usr/bin/env bash
# Ensure every vendored crate patch is active and replaces its registry package.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

lockfile=${CARINA_CARGO_LOCK_PATH:-"$repo_root/Cargo.lock"}

if grep -Eq '^[[:space:]]*\[\[patch\.unused\]\][[:space:]]*$' "$lockfile"; then
  echo "ERROR: $lockfile contains a [[patch.unused]] section; update or remove the stale vendored patch." >&2
  exit 1
fi

metadata_file=$(mktemp)
resolved_manifests_file=$(mktemp)
trap 'rm -f "$metadata_file" "$resolved_manifests_file"' EXIT

if ! cargo metadata --format-version 1 --locked >"$metadata_file"; then
  echo "ERROR: cargo metadata failed while checking vendored patches." >&2
  exit 1
fi

jq -r '
  (.resolve.nodes | map(.id)) as $resolved_ids
  | .packages[]
  | select(.id as $id | $resolved_ids | index($id))
  | .manifest_path
' "$metadata_file" >"$resolved_manifests_file"

violations=0

while IFS= read -r vendor_dir; do
  manifest_path="$vendor_dir/Cargo.toml"
  if [[ ! -f "$manifest_path" ]]; then
    echo "ERROR: Vendored directory has no Cargo.toml: $vendor_dir" >&2
    violations=$((violations + 1))
    continue
  fi

  manifest_path="$(cd "$vendor_dir" && pwd -P)/Cargo.toml"
  if ! grep -Fxq "$manifest_path" "$resolved_manifests_file"; then
    echo "ERROR: Vendored package is not in the Cargo resolve: $manifest_path" >&2
    violations=$((violations + 1))
    continue
  fi

  package_info=$(
    jq -r --arg manifest_path "$manifest_path" '
      .packages[]
      | select(.manifest_path == $manifest_path)
      | [.name, .version]
      | @tsv
    ' "$metadata_file"
  )
  IFS=$'\t' read -r package_name package_version <<<"$package_info"

  registry_match=$(
    jq -r \
      --arg name "$package_name" \
      --arg version "$package_version" '
        (.resolve.nodes | map(.id)) as $resolved_ids
        | .packages[]
        | select(.id as $id | $resolved_ids | index($id))
        | select(
            .name == $name
            and .version == $version
            and ((.source // "") | startswith("registry+"))
          )
        | "\(.name) \(.version) from \(.source)"
      ' "$metadata_file"
  )
  if [[ -n "$registry_match" ]]; then
    echo "ERROR: Registry package remains resolved alongside vendored $package_name $package_version:" >&2
    echo "  $registry_match" >&2
    violations=$((violations + 1))
  fi
done < <(find "$repo_root/vendor" -mindepth 1 -maxdepth 1 -type d | sort)

if [[ "$violations" -ne 0 ]]; then
  exit 1
fi

echo "OK: All vendored patches are resolved and replace their registry packages."
