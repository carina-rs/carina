FIXTURES = carina-cli/tests/fixtures/plan_display
CARINA = cargo run --manifest-path $(CURDIR)/Cargo.toml --bin carina --

plan-all-create:
	cd $(FIXTURES)/all_create && $(CARINA) plan --refresh=false main.crn

plan-no-changes:
	cd $(FIXTURES)/no_changes && $(CARINA) plan --refresh=false main.crn

plan-mixed:
	cd $(FIXTURES)/mixed_operations && $(CARINA) plan --refresh=false main.crn

plan-delete:
	cd $(FIXTURES)/delete_orphan && $(CARINA) plan --refresh=false main.crn

plan-compact:
	cd $(FIXTURES)/compact && $(CARINA) plan --refresh=false --detail none main.crn

plan-map-diff:
	cd $(FIXTURES)/map_key_diff && $(CARINA) plan --refresh=false main.crn

plan-enum-display:
	cd $(FIXTURES)/enum_display && $(CARINA) plan --refresh=false main.crn

plan-no-changes-enum:
	cd $(FIXTURES)/no_changes_enum && $(CARINA) plan --refresh=false main.crn

plan-destroy-full:
	cd $(FIXTURES)/destroy_full && $(CARINA) destroy --refresh=false --lock=false main.crn

plan-destroy-orphans:
	cd $(FIXTURES)/destroy_orphans && $(CARINA) destroy --refresh=false --lock=false main.crn

plan-read-only-attrs:
	cd $(FIXTURES)/read_only_attrs && $(CARINA) plan --refresh=false main.crn

plan-default-values:
	cd $(FIXTURES)/default_values && $(CARINA) plan --refresh=false main.crn

plan-explicit:
	cd $(FIXTURES)/explicit && $(CARINA) plan --refresh=false --detail explicit main.crn

plan-default-tags:
	cd $(FIXTURES)/default_tags && $(CARINA) plan --refresh=false main.crn

plan-map-diff-tui:
	cd $(FIXTURES)/map_key_diff && $(CARINA) plan --refresh=false --tui main.crn

plan-all-create-tui:
	cd $(FIXTURES)/all_create && $(CARINA) plan --refresh=false --tui main.crn

plan-mixed-tui:
	cd $(FIXTURES)/mixed_operations && $(CARINA) plan --refresh=false --tui main.crn

plan-delete-tui:
	cd $(FIXTURES)/delete_orphan && $(CARINA) plan --refresh=false --tui main.crn

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

.PHONY: plan-all-create plan-no-changes plan-no-changes-enum plan-mixed plan-delete plan-compact \
        plan-map-diff plan-enum-display plan-destroy-full plan-destroy-orphans plan-read-only-attrs \
        plan-default-values plan-explicit plan-default-tags \
        plan-map-diff-tui plan-all-create-tui plan-mixed-tui plan-delete-tui plan-fixtures
