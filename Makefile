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
	cd $(FIXTURES)/compact && $(CARINA) plan --refresh=false --compact main.crn

plan-map-diff:
	cd $(FIXTURES)/map_key_diff && $(CARINA) plan --refresh=false main.crn

plan-enum-display:
	cd $(FIXTURES)/enum_display && $(CARINA) plan --refresh=false main.crn

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

.PHONY: plan-all-create plan-no-changes plan-mixed plan-delete plan-compact \
        plan-map-diff plan-enum-display plan-map-diff-tui \
        plan-all-create-tui plan-mixed-tui plan-delete-tui plan-fixtures
