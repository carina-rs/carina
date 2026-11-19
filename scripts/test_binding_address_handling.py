import importlib.util
import pathlib
import unittest

spec = importlib.util.spec_from_file_location("boundary", pathlib.Path(__file__).with_name("check-binding-address-handling.py"))
boundary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(boundary)


class BindingAddressBoundaryTests(unittest.TestCase):
    def test_rejects_quoted_key_consumer_spelling(self):
        for expression in [
            'format!("{binding}[\'{key}\']")',
            'write!(output, "{binding}[\'{key}\']")',
        ]:
            with self.subTest(expression=expression):
                self.assertTrue(boundary.violations("carina-core/src/new_consumer.rs", expression))

    def test_external_test_module_does_not_hide_production_code(self):
        for declaration in ["#[cfg(test)]\nmod tests;", "#[cfg(test)] mod tests;"]:
            with self.subTest(declaration=declaration):
                self.assertTrue(boundary.violations(
                    "carina-core/src/new_consumer.rs",
                    declaration + "\nfn lookup() { binding.contains('['); }\n",
                ))

    def test_inline_test_module_examples_are_excluded(self):
        self.assertFalse(boundary.violations(
            "carina-core/src/new_consumer.rs",
            "#[cfg(test)]\nmod tests { fn example() { binding.contains('['); } }",
        ))

    def test_production_after_inline_test_module_is_checked(self):
        self.assertTrue(boundary.violations(
            "carina-core/src/new_consumer.rs",
            '#[cfg(test)]\nmod tests { fn example() { let text = "}"; } }\n'
            "fn production() { binding.contains('['); }\n",
        ))

    def test_rejects_additional_consumer_spelling(self):
        for expression in [
            "binding.split_once('[')", "binding.ends_with(']')",
            'binding.strip_suffix("\']")',
            'format!("{binding}[{index}]")', 'format!("rs[{index}]")',
            'write!(output, "{binding}[{index}]")', 'write!(output, "rs[{index}]")',
            'write!(output, "{}[{}]", binding, index)',
        ]:
            with self.subTest(expression=expression):
                self.assertTrue(boundary.violations("carina-core/src/new_consumer.rs", expression))

    def test_rejects_consumer_spelling_and_raw_minting(self):
        for expression in ["binding.contains('[')", "binding.find('[')", "binding.rfind('[')", 'format!("{binding}[{index}]")', 'format!(\n    "{}[{}]", binding, index)', "binding_with_index(&binding, 0)", "binding_with_map_key(&binding, &key)", "binding.with_index(0)", "crate::utils::map_key_address(binding, key)"]:
            with self.subTest(expression=expression):
                findings = boundary.violations("carina-core/src/new_consumer.rs", expression)
                self.assertTrue(findings)
                self.assertIn("decompose_binding_address", findings[0])

    def test_allows_address_owner_and_non_address_paths(self):
        self.assertFalse(boundary.violations("carina-core/src/resource/binding_address.rs", "binding_with_index(&binding, 0)"))
        self.assertFalse(boundary.violations("carina-core/src/references.rs", 'format!("{path}[{index}]")'))
        self.assertFalse(boundary.violations("carina-core/src/new_consumer.rs", "decompose_binding_address(binding).is_some()"))

    def test_json_vector_diagnostics_do_not_exempt_binding_spelling(self):
        path = "carina-cli/src/commands/plan.rs"
        self.assertFalse(boundary.violations(path, 'format!("Invalid saved plan: {kind}[{index}] contains non-managed dependency")'))
        self.assertTrue(boundary.violations(path, 'format!("{binding}[{index}]")'))
        self.assertTrue(boundary.violations("carina-state/src/state/mod.rs", 'format!("{binding}[{index}]")'))

    def test_module_instance_prefix_split_has_its_own_allow_list(self):
        expression = "name.split_once('.')"
        self.assertTrue(boundary.violations("carina-core/src/new_consumer.rs", expression))
        self.assertFalse(boundary.violations("carina-core/src/module_resolver/expander.rs", "fn split_instance_prefix() { " + expression + " }"))

    def test_prefix_owner_does_not_exempt_other_consumers_in_the_same_file(self):
        self.assertTrue(boundary.violations(
            "carina-core/src/module_resolver/expander.rs",
            "fn loop_membership() { name.split_once('.'); }",
        ))

    def test_repository_has_no_consumer_spelling_or_raw_minting(self):
        self.assertEqual(boundary.main(), 0)
