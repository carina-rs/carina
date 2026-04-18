PLAN_FIXTURE = cargo run --manifest-path $(CURDIR)/Cargo.toml -p carina-cli --example plan-fixture --quiet --

plan-all-create:
	$(PLAN_FIXTURE) all_create
plan-no-changes:
	$(PLAN_FIXTURE) no_changes
plan-mixed:
	$(PLAN_FIXTURE) mixed_operations
plan-delete:
	$(PLAN_FIXTURE) delete_orphan
plan-state-blocks:
	$(PLAN_FIXTURE) state_blocks
plan-compact:
	$(PLAN_FIXTURE) compact --detail none
plan-map-diff:
	$(PLAN_FIXTURE) map_key_diff
plan-nested-map-diff:
	$(PLAN_FIXTURE) nested_map_diff
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
	@echo "=== compact ==="
	@$(MAKE) plan-compact
	@echo ""
	@echo "=== map_key_diff ==="
	@$(MAKE) plan-map-diff
	@echo ""
	@echo "=== nested_map_diff ==="
	@$(MAKE) plan-nested-map-diff
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
	@$(MAKE) plan-deferred-for

plan-upstream-state:
	$(PLAN_FIXTURE) upstream_state
plan-deferred-for:
	$(PLAN_FIXTURE) deferred_for
plan-exports:
	$(PLAN_FIXTURE) exports
plan-exports-multifile:
	$(PLAN_FIXTURE) exports_multifile
.PHONY: plan-all-create plan-no-changes plan-no-changes-enum plan-mixed plan-delete plan-compact \
        plan-map-diff plan-enum-display plan-destroy-full plan-destroy-orphans plan-read-only-attrs \
        plan-default-values plan-explicit plan-default-tags \
        plan-state-blocks plan-secret-values plan-moved-with-changes plan-moved-prev-keys plan-moved-pure \
        plan-upstream-state plan-deferred-for plan-exports plan-exports-multifile \
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
