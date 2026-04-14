FIXTURES = carina-cli/tests/fixtures/plan_display
CARINA = cargo run --manifest-path $(CURDIR)/Cargo.toml --bin carina --

plan-all-create:
	cd $(FIXTURES)/all_create && $(CARINA) plan --refresh=false
plan-no-changes:
	cd $(FIXTURES)/no_changes && $(CARINA) plan --refresh=false
plan-mixed:
	cd $(FIXTURES)/mixed_operations && $(CARINA) plan --refresh=false
plan-delete:
	cd $(FIXTURES)/delete_orphan && $(CARINA) plan --refresh=false
plan-state-blocks:
	cd $(FIXTURES)/state_blocks && $(CARINA) plan --refresh=false
plan-compact:
	cd $(FIXTURES)/compact && $(CARINA) plan --refresh=false --detail none
plan-map-diff:
	cd $(FIXTURES)/map_key_diff && $(CARINA) plan --refresh=false
plan-nested-map-diff:
	cd $(FIXTURES)/nested_map_diff && $(CARINA) plan --refresh=false
plan-enum-display:
	cd $(FIXTURES)/enum_display && $(CARINA) plan --refresh=false
plan-no-changes-enum:
	cd $(FIXTURES)/no_changes_enum && $(CARINA) plan --refresh=false
plan-destroy-full:
	cd $(FIXTURES)/destroy_full && $(CARINA) destroy --refresh=false --lock=false
plan-destroy-orphans:
	cd $(FIXTURES)/destroy_orphans && $(CARINA) destroy --refresh=false --lock=false
plan-read-only-attrs:
	cd $(FIXTURES)/read_only_attrs && $(CARINA) plan --refresh=false
plan-default-values:
	cd $(FIXTURES)/default_values && $(CARINA) plan --refresh=false
plan-explicit:
	cd $(FIXTURES)/explicit && $(CARINA) plan --refresh=false --detail explicit
plan-default-tags:
	cd $(FIXTURES)/default_tags && $(CARINA) plan --refresh=false
plan-secret-values:
	cd $(FIXTURES)/secret_values && $(CARINA) plan --refresh=false
plan-moved-with-changes:
	cd $(FIXTURES)/moved_with_changes && $(CARINA) plan --refresh=false
plan-moved-prev-keys:
	cd $(FIXTURES)/moved_prev_keys && $(CARINA) plan --refresh=false
plan-moved-pure:
	cd $(FIXTURES)/moved_pure && $(CARINA) plan --refresh=false
plan-map-diff-tui:
	cd $(FIXTURES)/map_key_diff && $(CARINA) plan --refresh=false --tui
plan-all-create-tui:
	cd $(FIXTURES)/all_create && $(CARINA) plan --refresh=false --tui
plan-mixed-tui:
	cd $(FIXTURES)/mixed_operations && $(CARINA) plan --refresh=false --tui
plan-delete-tui:
	cd $(FIXTURES)/delete_orphan && $(CARINA) plan --refresh=false --tui
plan-moved-with-changes-tui:
	cd $(FIXTURES)/moved_with_changes && $(CARINA) plan --refresh=false --tui
plan-moved-pure-tui:
	cd $(FIXTURES)/moved_pure && $(CARINA) plan --refresh=false --tui
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
	@echo "=== remote_state ==="
	@$(MAKE) plan-remote-state

plan-remote-state:
	cd $(FIXTURES)/remote_state && $(CARINA) plan --refresh=false
plan-exports:
	cd $(FIXTURES)/exports && $(CARINA) plan --refresh=false
.PHONY: plan-all-create plan-no-changes plan-no-changes-enum plan-mixed plan-delete plan-compact \
        plan-map-diff plan-enum-display plan-destroy-full plan-destroy-orphans plan-read-only-attrs \
        plan-default-values plan-explicit plan-default-tags \
        plan-state-blocks plan-secret-values plan-moved-with-changes plan-moved-prev-keys plan-moved-pure \
        plan-remote-state plan-exports \
        plan-map-diff-tui plan-all-create-tui plan-mixed-tui plan-delete-tui \
        plan-moved-with-changes-tui plan-moved-pure-tui plan-fixtures
