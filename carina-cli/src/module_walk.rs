// Reachability and the complete prohibition collection are resolver-owned.
pub(crate) use carina_core::module_resolver::ModuleImportTraversal as ModuleWalk;

#[cfg(test)]
use carina_core::{module_resolver, paths::ResolvedPath};
#[cfg(test)]
use std::path::Path;

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn module_walk_preserves_symlink_parent_diagnostic_spelling() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let private = temp.path().join("private");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(private.join("nested")).unwrap();
        std::fs::create_dir_all(private.join("bad/child")).unwrap();
        std::os::unix::fs::symlink(private.join("nested"), project.join("jump")).unwrap();
        std::fs::write(
            private.join("bad/main.crn"),
            "arguments { value: String }\nlet child = use { source = './child' }\nchild { value = value }\n",
        )
        .unwrap();
        std::fs::write(
            private.join("bad/child/main.crn"),
            "arguments { value: String }\n",
        )
        .unwrap();
        let context =
            carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        let parsed = carina_core::parser::parse_file(
            "let bad = use { source = './jump/../bad' }\nbad { value = 'hello' }\n",
            &project.join("main.crn"),
            &context,
        )
        .unwrap();
        let directory_parse =
            carina_core::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed.clone());
        let walk = ModuleWalk::load(
            &directory_parse,
            &ResolvedPath::new(&project).unwrap(),
            &context,
            &carina_core::schema::SchemaRegistry::new(),
        );
        assert_eq!(
            walk.iter()
                .map(|module| module.diagnostic_path())
                .collect::<Vec<_>>(),
            vec![
                Path::new("./jump/../bad"),
                Path::new("./jump/../bad/./child")
            ]
        );
        assert_eq!(
            walk.iter()
                .map(|module| module.diagnostic_label().to_string())
                .collect::<Vec<_>>(),
            vec!["jump/../bad", "jump/../bad/child"]
        );
    }
}

#[cfg(test)]
mod unavailable_tests {
    use super::*;
    #[test]
    fn static_walk_keeps_readable_bodies_while_signatures_skip_unavailable_interfaces() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("arguments.crn"), "arguments {}\n").unwrap();
        let environment = carina_core::EnvironmentSnapshot::empty();
        let context = carina_core::parser::ProviderContext::new(environment.clone());
        let parsed = carina_core::parser::parse_file(
            "let child = use { source = './child' }\nlet call = child {}\n",
            &directory.path().join("main.crn"),
            &context,
        )
        .unwrap();
        let path = ResolvedPath::new(directory.path()).unwrap();
        for (source, expected_modules) in [(b"let broken =\n".as_slice(), 0), (&[0xff], 1)] {
            std::fs::write(child.join("broken.crn"), source).unwrap();
            let directory_parse =
                carina_core::config_loader::ParsedRootDirectory::from_parsed_for_test(
                    parsed.clone(),
                );
            let walk = ModuleWalk::load(
                &directory_parse,
                &path,
                &context,
                &carina_core::schema::SchemaRegistry::new(),
            );
            assert_eq!(walk.iter().count(), expected_modules);
            assert!(walk.prohibitions().boundary_findings().next().is_none());
            assert!(
                module_resolver::load_resolved_module_signatures(&parsed, &path, &context)
                    .is_empty()
            );
        }
    }
}

#[cfg(test)]
mod rejected_import_tests {
    use super::*;
    #[test]
    fn ordinary_import_rejections_belong_to_the_resolver() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("not-a-directory.crn"), "// file\n").unwrap();
        std::fs::create_dir(directory.path().join("empty")).unwrap();
        let environment = carina_core::EnvironmentSnapshot::empty();
        let context = carina_core::parser::ProviderContext::new(environment.clone());
        let parsed = carina_core::parser::parse_file(
            "let file = use { source = './not-a-directory.crn' }\nlet empty = use { source = './empty' }\n",
            &directory.path().join("main.crn"), &context,
        ).unwrap();
        let directory_parse =
            carina_core::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed.clone());
        let walk = ModuleWalk::load(
            &directory_parse,
            &ResolvedPath::new(directory.path()).unwrap(),
            &context,
            &carina_core::schema::SchemaRegistry::new(),
        );
        assert!(walk.prohibitions().boundary_findings().next().is_none());
        assert!(crate::wiring::validate_module_boundaries(walk.prohibitions()).is_empty());
    }

    #[test]
    fn rejected_import_still_reports_child_prohibitions() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("child");
        std::fs::create_dir_all(child.join("nested")).unwrap();
        std::fs::write(child.join("arguments.crn"), "arguments {}\n").unwrap();
        std::fs::write(child.join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(
            child.join("main.crn"),
            "let nested = use { source = './nested' }\n",
        )
        .unwrap();
        std::fs::write(
            child.join("nested/backend.crn"),
            "backend local { path = 'state.json' }\n",
        )
        .unwrap();
        let environment = carina_core::EnvironmentSnapshot::empty();
        let context = carina_core::parser::ProviderContext::new(environment.clone());
        let parsed = carina_core::parser::parse_file(
            "let child = use { source = './child' }\n",
            &directory.path().join("main.crn"),
            &context,
        )
        .unwrap();
        let directory_parse =
            carina_core::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed.clone());
        let walk = ModuleWalk::load(
            &directory_parse,
            &ResolvedPath::new(directory.path()).unwrap(),
            &context,
            &carina_core::schema::SchemaRegistry::new(),
        );
        assert_eq!(walk.iter().count(), 0);
        assert_eq!(walk.prohibitions().boundary_findings().count(), 2);
        assert!(
            walk.prohibitions()
                .boundary_findings()
                .all(|finding| finding.prohibition_path().is_some())
        );
    }
}
