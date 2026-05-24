PLAN_FIXTURE = cargo run --manifest-path $(CURDIR)/Cargo.toml -p carina-cli --example plan-fixture --quiet --

plan-all-create:
	$(PLAN_FIXTURE) all_create
plan-no-changes:
	$(PLAN_FIXTURE) no_changes
plan-mixed:
	$(PLAN_FIXTURE) mixed_operations
plan-delete:
	$(PLAN_FIXTURE) delete_orphan
plan-delete-list-of-maps:
	$(PLAN_FIXTURE) delete_orphan_list_of_maps
plan-state-blocks:
	$(PLAN_FIXTURE) state_blocks
plan-compact:
	$(PLAN_FIXTURE) compact --detail none
plan-map-diff:
	$(PLAN_FIXTURE) map_key_diff
plan-map-added-from-none:
	$(PLAN_FIXTURE) map_added_from_none
plan-map-attribute-removed:
	$(PLAN_FIXTURE) map_attribute_removed
plan-nested-map-diff:
	$(PLAN_FIXTURE) nested_map_diff
plan-list-diff-added-struct:
	$(PLAN_FIXTURE) list_diff_added_struct
plan-list-diff-removed-struct:
	$(PLAN_FIXTURE) list_diff_removed_struct
plan-list-diff-modified-with-unchanged:
	$(PLAN_FIXTURE) list_diff_modified_with_unchanged
plan-list-diff-modified-with-unchanged-nested:
	$(PLAN_FIXTURE) list_diff_modified_with_unchanged_nested
plan-list-diff-paired-all-unchanged-dropped:
	$(PLAN_FIXTURE) list_diff_paired_all_unchanged_dropped
plan-list-diff-string-list-grew:
	$(PLAN_FIXTURE) list_diff_string_list_grew
plan-map-field-string-list-grew:
	$(PLAN_FIXTURE) map_field_string_list_grew
plan-nested-list-of-maps-all-dropped:
	$(PLAN_FIXTURE) nested_list_of_maps_all_dropped
plan-enum-display:
	$(PLAN_FIXTURE) enum_display
plan-no-changes-enum:
	$(PLAN_FIXTURE) no_changes_enum
plan-destroy-full:
	$(PLAN_FIXTURE) destroy_full --destroy
plan-destroy-orphans:
	$(PLAN_FIXTURE) destroy_orphans --destroy
plan-read-only-attrs:
	$(PLAN_FIXTURE) read_only_attrs
plan-default-values:
	$(PLAN_FIXTURE) default_values
plan-explicit:
	$(PLAN_FIXTURE) explicit --detail explicit
plan-default-tags:
	$(PLAN_FIXTURE) default_tags
plan-depends-on:
	$(PLAN_FIXTURE) depends_on
plan-wait-cert:
	$(PLAN_FIXTURE) wait_cert
plan-secret-values:
	$(PLAN_FIXTURE) secret_values
plan-moved-with-changes:
	$(PLAN_FIXTURE) moved_with_changes
plan-moved-prev-keys:
	$(PLAN_FIXTURE) moved_prev_keys
plan-moved-pure:
	$(PLAN_FIXTURE) moved_pure
plan-map-diff-tui:
	$(PLAN_FIXTURE) map_key_diff --tui
plan-all-create-tui:
	$(PLAN_FIXTURE) all_create --tui
plan-mixed-tui:
	$(PLAN_FIXTURE) mixed_operations --tui
plan-delete-tui:
	$(PLAN_FIXTURE) delete_orphan --tui
plan-moved-with-changes-tui:
	$(PLAN_FIXTURE) moved_with_changes --tui
plan-moved-pure-tui:
	$(PLAN_FIXTURE) moved_pure --tui
plan-fixtures:
	@echo "=== all_create ==="
	@$(MAKE) plan-all-create
	@echo ""
	@echo "=== no_changes ==="
	@$(MAKE) plan-no-changes
	@echo ""
	@echo "=== mixed_operations ==="
	@$(MAKE) plan-mixed
	@echo ""
	@echo "=== delete_orphan ==="
	@$(MAKE) plan-delete
	@echo ""
	@echo "=== delete_orphan_list_of_maps ==="
	@$(MAKE) plan-delete-list-of-maps
	@echo ""
	@echo "=== compact ==="
	@$(MAKE) plan-compact
	@echo ""
	@echo "=== map_key_diff ==="
	@$(MAKE) plan-map-diff
	@echo "---"
	@$(MAKE) plan-map-added-from-none
	@echo "---"
	@$(MAKE) plan-map-attribute-removed
	@echo ""
	@echo "=== nested_map_diff ==="
	@$(MAKE) plan-nested-map-diff
	@echo "---"
	@$(MAKE) plan-list-diff-added-struct
	@echo "---"
	@$(MAKE) plan-list-diff-removed-struct
	@echo "---"
	@$(MAKE) plan-list-diff-modified-with-unchanged
	@echo "---"
	@$(MAKE) plan-list-diff-modified-with-unchanged-nested
	@echo "---"
	@$(MAKE) plan-list-diff-paired-all-unchanged-dropped
	@echo "---"
	@$(MAKE) plan-list-diff-string-list-grew
	@echo "---"
	@$(MAKE) plan-map-field-string-list-grew
	@echo "---"
	@$(MAKE) plan-nested-list-of-maps-all-dropped
	@echo ""
	@echo "=== enum_display ==="
	@$(MAKE) plan-enum-display
	@echo ""
	@echo "=== no_changes_enum ==="
	@$(MAKE) plan-no-changes-enum
	@echo ""
	@echo "=== destroy_full ==="
	@$(MAKE) plan-destroy-full
	@echo ""
	@echo "=== destroy_orphans ==="
	@$(MAKE) plan-destroy-orphans
	@echo ""
	@echo "=== read_only_attrs ==="
	@$(MAKE) plan-read-only-attrs
	@echo ""
	@echo "=== default_values ==="
	@$(MAKE) plan-default-values
	@echo ""
	@echo "=== explicit ==="
	@$(MAKE) plan-explicit
	@echo ""
	@echo "=== default_tags ==="
	@$(MAKE) plan-default-tags
	@echo ""
	@echo "=== state_blocks ==="
	@$(MAKE) plan-state-blocks
	@echo ""
	@echo "=== secret_values ==="
	@$(MAKE) plan-secret-values
	@echo ""
	@echo "=== moved_with_changes ==="
	@$(MAKE) plan-moved-with-changes
	@echo ""
	@echo "=== moved_prev_keys ==="
	@$(MAKE) plan-moved-prev-keys
	@echo ""
	@echo "=== moved_pure ==="
	@$(MAKE) plan-moved-pure
	@echo ""
	@echo "=== upstream_state ==="
	@$(MAKE) plan-upstream-state
	@echo "---"
	@$(MAKE) plan-upstream-state-unresolved
	@echo "---"
	@$(MAKE) plan-upstream-state-empty-exports
	@echo "---"
	@$(MAKE) plan-upstream-state-map-subscript
	@echo "---"
	@$(MAKE) plan-upstream-state-map-dot-notation
	@echo "---"
	@$(MAKE) plan-deferred-for
	@echo ""
	@echo "=== policy_pretty ==="
	@$(MAKE) plan-policy-pretty
	@echo "---"
	@$(MAKE) plan-policy-pretty-nested
	@echo "---"
	@$(MAKE) plan-policy-pretty-dynamic-key-list
	@echo "---"
	@$(MAKE) plan-pretty-long-string-list
	@echo "---"
	@$(MAKE) plan-pretty-short-string-list
	@echo ""
	@echo "=== provider_prefix ==="
	@$(MAKE) plan-provider-prefix
	@echo ""
	@echo "=== module_anonymous_resource ==="
	@$(MAKE) plan-module-anonymous-resource
	@echo ""
	@echo "=== multi_instance_create ==="
	@$(MAKE) plan-multi-instance-create
	@echo ""
	@echo "=== multi_instance_module ==="
	@$(MAKE) plan-multi-instance-module

plan-upstream-state:
	$(PLAN_FIXTURE) upstream_state
plan-upstream-state-unresolved:
	$(PLAN_FIXTURE) upstream_state_unresolved
plan-upstream-state-empty-exports:
	$(PLAN_FIXTURE) upstream_state_empty_exports
plan-upstream-state-map-subscript:
	$(PLAN_FIXTURE) upstream_state_map_subscript
plan-upstream-state-map-dot-notation:
	$(PLAN_FIXTURE) upstream_state_map_dot_notation
plan-deferred-for:
	$(PLAN_FIXTURE) deferred_for
plan-exports:
	$(PLAN_FIXTURE) exports
plan-exports-multifile:
	$(PLAN_FIXTURE) exports_multifile
plan-policy-pretty:
	$(PLAN_FIXTURE) policy_pretty
plan-policy-pretty-nested:
	$(PLAN_FIXTURE) policy_pretty_nested
plan-policy-pretty-dynamic-key-list:
	$(PLAN_FIXTURE) policy_pretty_dynamic_key_list
plan-pretty-long-string-list:
	$(PLAN_FIXTURE) pretty_long_string_list
plan-pretty-short-string-list:
	$(PLAN_FIXTURE) pretty_short_string_list
plan-provider-prefix:
	$(PLAN_FIXTURE) provider_prefix
plan-module-anonymous-resource:
	$(PLAN_FIXTURE) module_anonymous_resource
plan-server-default-struct-leaf:
	$(PLAN_FIXTURE) server_default_struct_leaf
plan-multi-instance-create:
	$(PLAN_FIXTURE) multi_instance_create
plan-multi-instance-module:
	$(PLAN_FIXTURE) multi_instance_module
.PHONY: plan-all-create plan-no-changes plan-no-changes-enum plan-mixed plan-delete plan-delete-list-of-maps plan-compact \
        plan-map-diff plan-list-diff-added-struct plan-list-diff-removed-struct \
        plan-list-diff-modified-with-unchanged \
        plan-list-diff-modified-with-unchanged-nested \
        plan-enum-display plan-destroy-full plan-destroy-orphans plan-read-only-attrs \
        plan-default-values plan-explicit plan-default-tags \
        plan-state-blocks plan-secret-values plan-moved-with-changes plan-moved-prev-keys plan-moved-pure \
        plan-upstream-state plan-upstream-state-unresolved plan-upstream-state-empty-exports \
        plan-upstream-state-map-subscript plan-upstream-state-map-dot-notation \
        plan-deferred-for plan-exports plan-exports-multifile plan-policy-pretty \
        plan-policy-pretty-nested plan-policy-pretty-dynamic-key-list \
        plan-pretty-long-string-list plan-pretty-short-string-list plan-provider-prefix \
        plan-module-anonymous-resource plan-server-default-struct-leaf \
        plan-multi-instance-create plan-multi-instance-module \
        plan-map-diff-tui plan-all-create-tui plan-mixed-tui plan-delete-tui \
        plan-moved-with-changes-tui plan-moved-pure-tui plan-fixtures

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------
#
# `cargo install --force` has a rebuild-avoidance quirk where it can reuse a
# previously-installed artifact when only a dependency crate changed, leaving
# ~/.cargo/bin with a stale binary even though `cargo install` exits 0.
#
# Always rebuild the release binaries explicitly and copy them into
# $(INSTALL_DIR). Safe to re-run; cheap when nothing changed.
#
# Uses `cargo metadata` to resolve the effective target directory, so this
# works with both the default `target/` and a custom `target-dir` set in
# .cargo/config.toml.

INSTALL_DIR ?= $(HOME)/.cargo/bin
CARGO_TARGET_DIR := $(shell cargo metadata --format-version 1 --no-deps | \
                            sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')

install: install-cli install-lsp
install-cli:
	cargo build -p carina-cli --release
	install -m 755 "$(CARGO_TARGET_DIR)/release/carina" "$(INSTALL_DIR)/carina"
	@echo "Installed $(INSTALL_DIR)/carina"
install-lsp:
	cargo build -p carina-lsp --release
	install -m 755 "$(CARGO_TARGET_DIR)/release/carina-lsp" "$(INSTALL_DIR)/carina-lsp"
	@echo "Installed $(INSTALL_DIR)/carina-lsp"

.PHONY: install install-cli install-lsp

# ---------------------------------------------------------------------------
# VS Code extension packaging
# ---------------------------------------------------------------------------
#
# `make vscode-package` builds a `.vsix` for the VS Code extension. Install
# the result with: `code --install-extension editors/vscode/carina-X.Y.Z.vsix`.
# Do NOT install by copying the source directory into ~/.vscode/extensions/ —
# that path skips `npm install` and produces the silent activation failure
# described in #2420.

vscode-package:
	cd editors/vscode && npm run package

.PHONY: vscode-package
