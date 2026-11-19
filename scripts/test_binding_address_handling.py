import importlib.util
import pathlib
import unittest

spec = importlib.util.spec_from_file_location("boundary", pathlib.Path(__file__).with_name("check-binding-address-handling.py"))
boundary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(boundary)


class BindingAddressBoundaryTests(unittest.TestCase):
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

    def test_repository_has_no_consumer_spelling_or_raw_minting(self):
        self.assertEqual(boundary.main(), 0)
