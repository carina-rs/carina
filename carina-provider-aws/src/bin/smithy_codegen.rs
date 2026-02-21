//! Smithy-based Code Generator for Carina AWS Provider
//!
//! Generates Rust schema code from AWS Smithy JSON AST models,
//! producing output identical to the CloudFormation-based codegen.
//!
//! Usage:
//!   smithy-codegen --model-dir <path> --output-dir <path> [--resource <name>]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use carina_smithy::{ShapeKind, SmithyModel};
use clap::Parser;
use heck::ToSnakeCase;

use carina_provider_aws::resource_defs::{self, ResourceDef};

#[derive(Parser, Debug)]
#[command(name = "smithy-codegen")]
#[command(about = "Generate Carina AWS provider schema code from Smithy models")]
struct Args {
    /// Directory containing Smithy model JSON files
    #[arg(long)]
    model_dir: PathBuf,

    /// Output directory for generated Rust files
    #[arg(long)]
    output_dir: PathBuf,

    /// Generate only the specified resource (e.g., "ec2.vpc")
    #[arg(long)]
    resource: Option<String>,

    /// Output format: rust (default) or markdown (for documentation)
    #[arg(long, default_value = "rust")]
    format: String,
}

/// Information about a detected enum type
#[derive(Debug, Clone)]
struct EnumInfo {
    /// Type name in PascalCase (e.g., "InstanceTenancy")
    type_name: String,
    /// Valid enum values (e.g., ["default", "dedicated", "host"])
    values: Vec<String>,
}

/// Information about an attribute to generate
#[derive(Debug, Clone)]
struct AttrInfo {
    /// Snake_case attribute name (e.g., "cidr_block")
    snake_name: String,
    /// PascalCase provider name (e.g., "CidrBlock")
    provider_name: String,
    /// Rust code for the attribute type
    type_code: String,
    /// Whether the field is required
    is_required: bool,
    /// Whether the field is create-only
    is_create_only: bool,
    /// Whether the field is read-only
    is_read_only: bool,
    /// Description from Smithy docs
    description: Option<String>,
    /// Enum info if this attribute is an enum
    enum_info: Option<EnumInfo>,
}

/// Integer range constraint
#[derive(Debug, Clone, Copy)]
struct IntRange {
    min: i64,
    max: i64,
}

/// Convert a DSL resource name to a Rust module name.
/// e.g., "ec2.vpc" -> "ec2_vpc", "ec2.security_group_ingress" -> "ec2_security_group_ingress"
fn module_name(name: &str) -> String {
    name.replace('.', "_")
}

fn main() -> Result<()> {
    let args = Args::parse();

    std::fs::create_dir_all(&args.output_dir)?;

    // Collect all resource definitions
    let mut all_resources = resource_defs::ec2_resources();
    all_resources.extend(resource_defs::s3_resources());

    // Filter to requested resource if specified
    let resources: Vec<&ResourceDef> = if let Some(ref name) = args.resource {
        all_resources
            .iter()
            .filter(|r| r.name == name.as_str())
            .collect()
    } else {
        all_resources.iter().collect()
    };

    if resources.is_empty() {
        if let Some(ref name) = args.resource {
            anyhow::bail!("Unknown resource: {}", name);
        }
        anyhow::bail!("No resource definitions found");
    }

    // Load Smithy models (keyed by service namespace)
    let mut models: HashMap<&str, SmithyModel> = HashMap::new();
    for res in &resources {
        if models.contains_key(res.service_namespace) {
            continue;
        }
        let model = load_model(&args.model_dir, res.service_namespace)?;
        models.insert(res.service_namespace, model);
    }

    match args.format.as_str() {
        "rust" => {
            // Generate each resource
            let mut generated_modules: Vec<&str> = Vec::new();
            for res in &resources {
                let model = models.get(res.service_namespace).unwrap();
                let code = generate_resource(res, model)?;

                let mod_name = module_name(res.name);
                let output_path = args.output_dir.join(format!("{}.rs", mod_name));
                std::fs::write(&output_path, &code)
                    .with_context(|| format!("Failed to write {}", output_path.display()))?;
                eprintln!("Generated: {}", output_path.display());
                generated_modules.push(res.name);
            }

            // Generate mod.rs
            let mod_rs = generate_mod_rs(&generated_modules);
            let mod_path = args.output_dir.join("mod.rs");
            std::fs::write(&mod_path, &mod_rs)
                .with_context(|| format!("Failed to write {}", mod_path.display()))?;
            eprintln!("Generated: {}", mod_path.display());
        }
        "markdown" | "md" => {
            for res in &resources {
                let model = models.get(res.service_namespace).unwrap();
                let md = generate_markdown_resource(res, model)?;

                let output_path = args
                    .output_dir
                    .join(format!("{}.md", module_name(res.name)));
                std::fs::write(&output_path, &md)
                    .with_context(|| format!("Failed to write {}", output_path.display()))?;
                eprintln!("Generated: {}", output_path.display());
            }
        }
        other => anyhow::bail!("Unknown format: {}. Use 'rust' or 'markdown'.", other),
    }

    Ok(())
}

/// Load a Smithy model from a JSON file in the model directory.
fn load_model(model_dir: &Path, namespace: &str) -> Result<SmithyModel> {
    // Map namespace to file name: "com.amazonaws.ec2" -> "ec2.json"
    let service_name = namespace
        .strip_prefix("com.amazonaws.")
        .unwrap_or(namespace);
    let model_path = model_dir.join(format!("{}.json", service_name));

    let json = std::fs::read_to_string(&model_path)
        .with_context(|| format!("Failed to read model: {}", model_path.display()))?;
    let model = carina_smithy::parse(&json)
        .with_context(|| format!("Failed to parse model: {}", model_path.display()))?;

    Ok(model)
}

/// Generate Rust schema code for a single resource.
fn generate_resource(res: &ResourceDef, model: &SmithyModel) -> Result<String> {
    let ns = res.service_namespace;
    let namespace = format!("aws.{}", res.name);

    // Build exclude set
    let exclude: HashSet<&str> = res.exclude_fields.iter().copied().collect();

    // Build type override map
    let type_overrides: HashMap<&str, &str> = res.type_overrides.iter().copied().collect();

    // Build create-only override set
    let create_only_overrides: HashSet<&str> = res.create_only_overrides.iter().copied().collect();

    // Build required override set
    let required_overrides: HashSet<&str> = res.required_overrides.iter().copied().collect();

    // Build read-only override set
    let read_only_overrides: HashSet<&str> = res.read_only_overrides.iter().copied().collect();

    // Build extra read-only set
    let extra_read_only: HashSet<&str> = res.extra_read_only.iter().copied().collect();

    // Build enum alias map: attr_snake_name -> [(canonical, alias)]
    let mut enum_alias_map: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    for (attr, alias, canonical) in &res.enum_aliases {
        enum_alias_map
            .entry(attr)
            .or_default()
            .push((canonical, alias));
    }

    // Build to_dsl override map
    let to_dsl_overrides: HashMap<&str, &str> = res.to_dsl_overrides.iter().copied().collect();

    // Resolve create input fields
    let create_op_id = format!("{}#{}", ns, res.create_op);
    let create_input = model
        .operation_input(&create_op_id)
        .with_context(|| format!("Cannot find create input for {}", create_op_id))?;

    // Resolve read structure fields (if present)
    let read_structure = if let Some(read_struct_name) = res.read_structure {
        let read_structure_id = format!("{}#{}", ns, read_struct_name);
        Some(
            model
                .get_structure(&read_structure_id)
                .with_context(|| format!("Cannot find read structure {}", read_structure_id))?,
        )
    } else {
        None
    };

    // Resolve update input fields and their structures
    let mut updatable_fields: HashSet<String> = HashSet::new();
    let mut update_inputs: Vec<&carina_smithy::StructureShape> = Vec::new();
    for update_op in &res.update_ops {
        for field in &update_op.fields {
            updatable_fields.insert(field.to_string());
        }
        let update_op_id = format!("{}#{}", ns, update_op.operation);
        if let Some(update_input) = model.operation_input(&update_op_id) {
            update_inputs.push(update_input);
        }
    }

    // Collectors for enums and ranged ints (populated during type resolution)
    let mut all_enums: BTreeMap<String, EnumInfo> = BTreeMap::new();
    let mut all_ranged_ints: BTreeMap<String, IntRange> = BTreeMap::new();

    // Collect writable fields from create input
    let mut writable_fields: BTreeMap<String, &carina_smithy::ShapeRef> = BTreeMap::new();
    for (name, member_ref) in &create_input.members {
        if exclude.contains(name.as_str()) {
            continue;
        }
        if name == res.identifier {
            continue;
        }
        if name == "Tags" {
            continue; // handled separately
        }
        writable_fields.insert(name.clone(), member_ref);
    }

    // For read_ops resources: resolve fields from operation outputs and add them
    // as writable fields (if they match an update op) or read-only.
    let mut read_op_read_only: BTreeMap<String, &carina_smithy::ShapeRef> = BTreeMap::new();
    for read_op in &res.read_ops {
        let op_id = format!("{}#{}", ns, read_op.operation);
        let output = model
            .operation_output(&op_id)
            .with_context(|| format!("Cannot find output for {}", op_id))?;
        for (field_name, rename) in &read_op.fields {
            let effective_name = rename.unwrap_or(field_name);
            if let Some(member_ref) = output.members.get(*field_name) {
                if updatable_fields.contains(effective_name)
                    && !writable_fields.contains_key(effective_name)
                {
                    writable_fields.insert(effective_name.to_string(), member_ref);
                } else if !writable_fields.contains_key(effective_name) {
                    read_op_read_only.insert(effective_name.to_string(), member_ref);
                }
            }
        }
    }

    // Add updatable-only fields from read structure and update op inputs
    if let Some(read_struct) = read_structure {
        // (e.g., EnableDnsHostnames for VPC is in ModifyVpcAttributeRequest but not in Vpc struct)
        for (name, member_ref) in &read_struct.members {
            if exclude.contains(name.as_str()) || name == "Tags" || name == res.identifier {
                continue;
            }
            if writable_fields.contains_key(name) {
                continue;
            }
            if updatable_fields.contains(name.as_str()) {
                writable_fields.insert(name.clone(), member_ref);
            }
        }
    }
    // Also check update operation inputs for fields not found in create input or read structure
    for update_input in &update_inputs {
        for (name, member_ref) in &update_input.members {
            if exclude.contains(name.as_str()) || name == "Tags" || name == res.identifier {
                continue;
            }
            if writable_fields.contains_key(name) {
                continue;
            }
            if updatable_fields.contains(name.as_str()) {
                writable_fields.insert(name.clone(), member_ref);
            }
        }
    }

    // Collect read-only fields from read structure
    let mut read_only_fields: BTreeMap<String, &carina_smithy::ShapeRef> = BTreeMap::new();
    if let Some(read_struct) = read_structure {
        for (name, member_ref) in &read_struct.members {
            if exclude.contains(name.as_str()) {
                continue;
            }
            if name == "Tags" {
                continue;
            }
            // Skip fields already in writable set
            if writable_fields.contains_key(name) {
                continue;
            }
            // Include the identifier and extra read-only fields
            if name == res.identifier || extra_read_only.contains(name.as_str()) {
                read_only_fields.insert(name.clone(), member_ref);
            }
        }
    }
    // Add read-only fields from read_ops
    for (name, member_ref) in read_op_read_only {
        if !writable_fields.contains_key(&name) && !read_only_fields.contains_key(&name) {
            read_only_fields.insert(name, member_ref);
        }
    }

    // Build attribute list
    let mut attrs: Vec<AttrInfo> = Vec::new();

    // Process writable fields
    for (name, member_ref) in &writable_fields {
        let snake_name = name.to_snake_case();
        let is_required = (SmithyModel::is_required(member_ref)
            || required_overrides.contains(name.as_str()))
            && !read_only_overrides.contains(name.as_str());
        let is_read_only = read_only_overrides.contains(name.as_str());
        let is_create_only = if is_read_only {
            false
        } else {
            create_only_overrides.contains(name.as_str())
                || !updatable_fields.contains(name.as_str())
        };
        let description = SmithyModel::documentation(&member_ref.traits).map(|s| s.to_string());

        let (type_code, enum_info) = resolve_type(
            model,
            &member_ref.target,
            name,
            &namespace,
            &type_overrides,
            &enum_alias_map,
            &to_dsl_overrides,
            &mut all_enums,
            &mut all_ranged_ints,
        );

        attrs.push(AttrInfo {
            snake_name,
            provider_name: name.clone(),
            type_code,
            is_required,
            is_create_only,
            is_read_only,
            description,
            enum_info,
        });
    }

    // Process read-only fields
    for (name, member_ref) in &read_only_fields {
        let snake_name = name.to_snake_case();
        let description = SmithyModel::documentation(&member_ref.traits).map(|s| s.to_string());

        let (type_code, enum_info) = resolve_type(
            model,
            &member_ref.target,
            name,
            &namespace,
            &type_overrides,
            &enum_alias_map,
            &to_dsl_overrides,
            &mut all_enums,
            &mut all_ranged_ints,
        );

        attrs.push(AttrInfo {
            snake_name,
            provider_name: name.clone(),
            type_code,
            is_required: false,
            is_create_only: false,
            is_read_only: true,
            description,
            enum_info,
        });
    }

    // Also register top-level attribute enums (enum_info is set but may not have
    // been registered if the attribute was detected via known_enum_overrides in
    // resolve_type before the collector existed)
    for attr in &attrs {
        if let Some(ref ei) = attr.enum_info {
            all_enums
                .entry(attr.provider_name.clone())
                .or_insert_with(|| ei.clone());
        }
    }

    // Determine needed imports
    let has_enums = !all_enums.is_empty();
    let has_ranged_ints = !all_ranged_ints.is_empty();
    let code_str = attrs
        .iter()
        .map(|a| a.type_code.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let needs_types = code_str.contains("types::");
    let needs_tags_type = res.has_tags;
    let needs_struct_field = code_str.contains("StructField::");

    // Build code
    let mut code = String::new();
    let mod_name = module_name(res.name);

    // Header
    let resource_short = res
        .name
        .strip_prefix("ec2.")
        .or_else(|| res.name.strip_prefix("s3."))
        .unwrap_or(res.name);
    let mut schema_imports = vec!["AttributeSchema", "ResourceSchema"];
    schema_imports.insert(1, "AttributeType");
    if needs_struct_field {
        schema_imports.push("StructField");
    }
    if needs_types {
        schema_imports.push("types");
    }
    let schema_imports_str = schema_imports.join(", ");

    code.push_str(&format!(
        "//! {} schema definition for AWS Cloud Control\n\
         //!\n\
         //! Auto-generated from Smithy model: {}\n\
         //!\n\
         //! DO NOT EDIT MANUALLY - regenerate with smithy-codegen\n\n\
         use super::AwsSchemaConfig;\n",
        resource_short, ns
    ));

    if needs_tags_type {
        code.push_str("use super::tags_type;\n");
    }
    if has_enums {
        code.push_str("use super::validate_namespaced_enum;\n");
    }
    if has_enums || has_ranged_ints {
        code.push_str("use carina_core::resource::Value;\n");
    }
    code.push_str(&format!(
        "use carina_core::schema::{{{}}};\n\n",
        schema_imports_str
    ));

    // Generate enum constants and validation functions
    for (prop_name, enum_info) in &all_enums {
        let const_name = format!("VALID_{}", prop_name.to_snake_case().to_uppercase());
        let fn_name = format!("validate_{}", prop_name.to_snake_case());

        // Generate constant
        let mut all_values: Vec<String> = enum_info
            .values
            .iter()
            .map(|v| format!("\"{}\"", v))
            .collect();
        // Add alias values
        let snake = prop_name.to_snake_case();
        if let Some(aliases) = enum_alias_map.get(snake.as_str()) {
            for (_, alias) in aliases {
                all_values.push(format!("\"{}\"", alias));
            }
        }
        let values_str = all_values.join(", ");
        code.push_str(&format!(
            "#[allow(dead_code)]\nconst {}: &[&str] = &[{}];\n\n",
            const_name, values_str
        ));

        // Generate validation function
        code.push_str(&format!(
            "#[allow(dead_code)]\n\
             fn {}(value: &Value) -> Result<(), String> {{\n\
             \x20   validate_namespaced_enum(value, \"{}\", \"{}\", {})\n\
             \x20   .map_err(|reason| {{\n\
             \x20       if let Value::String(s) = value {{\n\
             \x20           format!(\"Invalid {} '{{}}': {{}}\", s, reason)\n\
             \x20       }} else {{\n\
             \x20           reason\n\
             \x20       }}\n\
             \x20   }})\n\
             }}\n\n",
            fn_name, enum_info.type_name, namespace, const_name, enum_info.type_name
        ));
    }

    // Generate range validation functions
    for (prop_name, range) in &all_ranged_ints {
        let fn_name = format!("validate_{}_range", prop_name.to_snake_case());
        code.push_str(&format!(
            "fn {}(value: &Value) -> Result<(), String> {{\n\
             \x20   if let Value::Int(n) = value {{\n\
             \x20       if *n < {} || *n > {} {{\n\
             \x20           Err(format!(\"Value {{}} is out of range {}..={}\", n))\n\
             \x20       }} else {{\n\
             \x20           Ok(())\n\
             \x20       }}\n\
             \x20   }} else {{\n\
             \x20       Err(\"Expected integer\".to_string())\n\
             \x20   }}\n\
             }}\n\n",
            fn_name, range.min, range.max, range.min, range.max
        ));
    }

    // Generate config function
    code.push_str(&format!(
        "/// Returns the schema config for {} (Smithy: {})\n\
         pub fn {}_config() -> AwsSchemaConfig {{\n\
         \x20   AwsSchemaConfig {{\n\
         \x20       aws_type_name: \"{}\",\n\
         \x20       resource_type_name: \"{}\",\n\
         \x20       has_tags: {},\n\
         \x20       schema: ResourceSchema::new(\"{}\")\n",
        res.name,
        ns,
        mod_name,
        cf_type_name(res.name),
        res.name,
        res.has_tags,
        namespace,
    ));

    // Description from read structure (or create input for multi-op resources)
    let desc_traits = if let Some(read_struct) = read_structure {
        Some(&read_struct.traits)
    } else {
        Some(&create_input.traits)
    };
    if let Some(traits) = desc_traits
        && let Some(desc) = SmithyModel::documentation(traits)
    {
        let escaped = escape_description(desc);
        let truncated = truncate_str(&escaped, 200);
        code.push_str(&format!(
            "\x20       .with_description(\"{}\")\n",
            truncated
        ));
    }

    // Inject carina-specific attributes (name, region)
    code.push_str(
        "\x20       .attribute(\n\
         \x20           AttributeSchema::new(\"name\", AttributeType::String)\n\
         \x20               .with_description(\"Resource name\"),\n\
         \x20       )\n\
         \x20       .attribute(\n\
         \x20           AttributeSchema::new(\"region\", super::aws_region())\n\
         \x20               .with_description(\"The AWS region (inherited from provider if not specified)\"),\n\
         \x20       )\n",
    );

    // Generate attributes
    for attr in &attrs {
        let type_code = if let Some(ref ei) = attr.enum_info {
            // Use Custom type for enums
            let validate_fn = format!("validate_{}", attr.provider_name.to_snake_case());
            let to_dsl_code =
                if let Some(override_code) = to_dsl_overrides.get(attr.snake_name.as_str()) {
                    override_code.to_string()
                } else {
                    let has_hyphens = ei.values.iter().any(|v| v.contains('-'));
                    let snake = attr.provider_name.to_snake_case();
                    if let Some(aliases) = enum_alias_map.get(snake.as_str()) {
                        let mut match_arms: Vec<String> = aliases
                            .iter()
                            .map(|(canonical, alias)| {
                                format!("\"{}\" => \"{}\".to_string()", canonical, alias)
                            })
                            .collect();
                        let fallback = if has_hyphens {
                            "_ => s.replace('-', \"_\")"
                        } else {
                            "_ => s.to_string()"
                        };
                        match_arms.push(fallback.to_string());
                        format!("Some(|s: &str| match s {{ {} }})", match_arms.join(", "))
                    } else if has_hyphens {
                        "Some(|s: &str| s.replace('-', \"_\"))".to_string()
                    } else {
                        "None".to_string()
                    }
                };
            format!(
                "AttributeType::Custom {{\n\
                 \x20               name: \"{}\".to_string(),\n\
                 \x20               base: Box::new(AttributeType::String),\n\
                 \x20               validate: {},\n\
                 \x20               namespace: Some(\"{}\".to_string()),\n\
                 \x20               to_dsl: {},\n\
                 \x20           }}",
                ei.type_name, validate_fn, namespace, to_dsl_code
            )
        } else {
            attr.type_code.clone()
        };

        let mut attr_code = format!(
            "\x20       .attribute(\n\
             \x20           AttributeSchema::new(\"{}\", {})",
            attr.snake_name, type_code
        );

        if attr.is_required {
            attr_code.push_str("\n\x20               .required()");
        }
        if attr.is_create_only {
            attr_code.push_str("\n\x20               .create_only()");
        }

        if let Some(ref desc) = attr.description {
            let escaped = escape_description(desc);
            let truncated = truncate_str(&escaped, 150);
            let suffix = if attr.is_read_only {
                " (read-only)"
            } else {
                ""
            };
            attr_code.push_str(&format!(
                "\n\x20               .with_description(\"{}{}\")",
                truncated, suffix
            ));
        } else if attr.is_read_only {
            attr_code.push_str("\n\x20               .with_description(\" (read-only)\")");
        }

        attr_code.push_str(&format!(
            "\n\x20               .with_provider_name(\"{}\"),",
            attr.provider_name
        ));
        attr_code.push_str("\n\x20       )\n");
        code.push_str(&attr_code);
    }

    // Tags attribute
    if res.has_tags {
        code.push_str(
            "\x20       .attribute(\n\
             \x20           AttributeSchema::new(\"tags\", tags_type())\n\
             \x20               .with_description(\"The tags for the resource.\")\n\
             \x20               .with_provider_name(\"Tags\"),\n\
             \x20       )\n",
        );
    }

    // Close schema and config
    code.push_str("\x20   }\n}\n");

    // Generate enum_valid_values()
    code.push_str(
        "\n/// Returns the resource type name and all enum valid values for this module\n\
         pub fn enum_valid_values() -> (&'static str, &'static [(&'static str, &'static [&'static str])]) {\n"
    );
    if all_enums.is_empty() {
        code.push_str(&format!("    (\"{}\", &[])\n", res.name));
    } else {
        let entries: Vec<String> = all_enums
            .keys()
            .map(|prop_name| {
                let attr_name = prop_name.to_snake_case();
                let const_name = format!("VALID_{}", attr_name.to_uppercase());
                format!("        (\"{}\", {}),", attr_name, const_name)
            })
            .collect();
        code.push_str(&format!(
            "    (\"{}\", &[\n{}\n    ])\n",
            res.name,
            entries.join("\n")
        ));
    }
    code.push_str("}\n");

    // Generate enum_alias_reverse()
    code.push_str(
        "\n/// Maps DSL alias values back to canonical AWS values for this module.\n\
         /// e.g., (\"ip_protocol\", \"all\") -> Some(\"-1\")\n\
         pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {\n",
    );

    let mut match_arms: Vec<String> = Vec::new();
    for (attr, alias, canonical) in &res.enum_aliases {
        match_arms.push(format!(
            "        (\"{}\", \"{}\") => Some(\"{}\")",
            attr, alias, canonical
        ));
    }

    if match_arms.is_empty() {
        code.push_str("    let _ = (attr_name, value);\n    None\n");
    } else {
        match_arms.push("        _ => None".to_string());
        code.push_str(&format!(
            "    match (attr_name, value) {{\n{}\n    }}\n",
            match_arms.join(",\n")
        ));
    }
    code.push_str("}\n");

    Ok(code)
}

/// Resolve a Smithy type to a Carina type code string.
/// Returns (type_code, Option<EnumInfo>).
/// Also populates collectors for enums and ranged ints discovered during resolution.
#[allow(clippy::too_many_arguments)]
fn resolve_type(
    model: &SmithyModel,
    target: &str,
    field_name: &str,
    namespace: &str,
    type_overrides: &HashMap<&str, &str>,
    enum_alias_map: &HashMap<&str, Vec<(&str, &str)>>,
    to_dsl_overrides: &HashMap<&str, &str>,
    all_enums: &mut BTreeMap<String, EnumInfo>,
    all_ranged_ints: &mut BTreeMap<String, IntRange>,
) -> (String, Option<EnumInfo>) {
    // Check type overrides first
    if let Some(&override_type) = type_overrides.get(field_name) {
        return (override_type.to_string(), None);
    }

    // Check known enum overrides
    if let Some(values) = known_enum_overrides().get(field_name) {
        let type_name = field_name.to_string();
        let enum_info = EnumInfo {
            type_name,
            values: values.iter().map(|s| s.to_string()).collect(),
        };
        all_enums
            .entry(field_name.to_string())
            .or_insert_with(|| enum_info.clone());
        return ("/* enum */".to_string(), Some(enum_info));
    }

    let kind = model.shape_kind(target);

    match kind {
        Some(ShapeKind::String) => {
            // Check name-based type inference
            if let Some(inferred) = infer_string_type(field_name) {
                return (inferred, None);
            }

            // Check for CIDR patterns
            let lower = field_name.to_lowercase();
            if lower.contains("cidr") {
                if lower.contains("ipv6") {
                    return ("types::ipv6_cidr()".to_string(), None);
                }
                return ("types::ipv4_cidr()".to_string(), None);
            }

            // Check for IP address patterns
            if (lower.contains("ipaddress")
                || lower.ends_with("ip")
                || lower.contains("ipaddresses"))
                && !lower.contains("cidr")
                && !lower.contains("count")
                && !lower.contains("type")
            {
                if lower.contains("ipv6") {
                    return ("types::ipv6_address()".to_string(), None);
                }
                return ("types::ipv4_address()".to_string(), None);
            }

            // IPAM Pool IDs
            if is_ipam_pool_id_property(field_name) {
                return ("super::ipam_pool_id()".to_string(), None);
            }

            // Resource IDs
            if is_aws_resource_id_property(field_name) {
                return (get_resource_id_type(field_name).to_string(), None);
            }

            // ARN patterns
            if lower.ends_with("arn") || lower.ends_with("arns") || lower.contains("_arn") {
                return ("super::arn()".to_string(), None);
            }

            // Availability zone
            if lower == "availabilityzone" {
                return ("super::availability_zone()".to_string(), None);
            }

            ("AttributeType::String".to_string(), None)
        }
        Some(ShapeKind::Boolean) => ("AttributeType::Bool".to_string(), None),
        Some(ShapeKind::Integer) | Some(ShapeKind::Long) => {
            // Check for range traits on the target shape
            let range = get_int_range(model, target, field_name);
            if let Some(r) = range {
                all_ranged_ints.entry(field_name.to_string()).or_insert(r);
                let validate_fn = format!("validate_{}_range", field_name.to_snake_case());
                (
                    format!(
                        "AttributeType::Custom {{\n\
                         \x20               name: \"Int({}..={})\".to_string(),\n\
                         \x20               base: Box::new(AttributeType::Int),\n\
                         \x20               validate: {},\n\
                         \x20               namespace: None,\n\
                         \x20               to_dsl: None,\n\
                         \x20           }}",
                        r.min, r.max, validate_fn
                    ),
                    None,
                )
            } else {
                ("AttributeType::Int".to_string(), None)
            }
        }
        Some(ShapeKind::Float) | Some(ShapeKind::Double) => {
            ("AttributeType::Float".to_string(), None)
        }
        Some(ShapeKind::Enum) => {
            // Get enum values from Smithy model
            if let Some(values) = model.enum_values(target) {
                // Use the field name as type_name for consistency with CF codegen
                // (e.g., "InstanceTenancy" not "Tenancy")
                let type_name = field_name.to_string();
                let string_values: Vec<String> = values.into_iter().map(|(_, v)| v).collect();
                let enum_info = EnumInfo {
                    type_name,
                    values: string_values,
                };
                all_enums
                    .entry(field_name.to_string())
                    .or_insert_with(|| enum_info.clone());
                return ("/* enum */".to_string(), Some(enum_info));
            }
            ("AttributeType::String".to_string(), None)
        }
        Some(ShapeKind::IntEnum) => ("AttributeType::Int".to_string(), None),
        Some(ShapeKind::List) => {
            // Get list member type
            if let Some(carina_smithy::Shape::List(list_shape)) = model.get_shape(target) {
                let (item_type, _) = resolve_type(
                    model,
                    &list_shape.member.target,
                    field_name,
                    namespace,
                    type_overrides,
                    enum_alias_map,
                    to_dsl_overrides,
                    all_enums,
                    all_ranged_ints,
                );
                (
                    format!("AttributeType::List(Box::new({}))", item_type),
                    None,
                )
            } else {
                (
                    "AttributeType::List(Box::new(AttributeType::String))".to_string(),
                    None,
                )
            }
        }
        Some(ShapeKind::Map) => (
            "AttributeType::Map(Box::new(AttributeType::String))".to_string(),
            None,
        ),
        Some(ShapeKind::Structure) => {
            // Check if it's a TagList-like structure
            let shape_name = SmithyModel::shape_name(target);
            if shape_name == "TagList" || shape_name == "Tag" {
                return ("tags_type()".to_string(), None);
            }

            // Unwrap EC2 AttributeBooleanValue wrapper → plain Bool
            if shape_name == "AttributeBooleanValue" {
                return ("AttributeType::Bool".to_string(), None);
            }

            // Generate struct type for nested structures
            if let Some(structure) = model.get_structure(target) {
                let struct_code = generate_struct_type(
                    model,
                    shape_name,
                    structure,
                    namespace,
                    type_overrides,
                    enum_alias_map,
                    to_dsl_overrides,
                    all_enums,
                    all_ranged_ints,
                );
                return (struct_code, None);
            }
            ("AttributeType::String".to_string(), None)
        }
        _ => {
            // Fallback: try name-based heuristics
            if let Some(inferred) = infer_string_type(field_name) {
                (inferred, None)
            } else {
                ("AttributeType::String".to_string(), None)
            }
        }
    }
}

/// Generate Rust code for an AttributeType::Struct.
#[allow(clippy::too_many_arguments)]
fn generate_struct_type(
    model: &SmithyModel,
    struct_name: &str,
    structure: &carina_smithy::StructureShape,
    namespace: &str,
    type_overrides: &HashMap<&str, &str>,
    enum_alias_map: &HashMap<&str, Vec<(&str, &str)>>,
    to_dsl_overrides: &HashMap<&str, &str>,
    all_enums: &mut BTreeMap<String, EnumInfo>,
    all_ranged_ints: &mut BTreeMap<String, IntRange>,
) -> String {
    let mut fields: Vec<String> = Vec::new();
    for (field_name, member_ref) in &structure.members {
        let snake_name = field_name.to_snake_case();
        let is_required = SmithyModel::is_required(member_ref);

        let (field_type, enum_info) = resolve_type(
            model,
            &member_ref.target,
            field_name,
            namespace,
            type_overrides,
            enum_alias_map,
            to_dsl_overrides,
            all_enums,
            all_ranged_ints,
        );

        // If enum detected, use Custom type with validator
        let field_type = if let Some(ei) = enum_info {
            let validate_fn = format!("validate_{}", field_name.to_snake_case());
            let to_dsl_code = if let Some(override_code) = to_dsl_overrides.get(snake_name.as_str())
            {
                override_code.to_string()
            } else {
                let has_hyphens = ei.values.iter().any(|v| v.contains('-'));
                if let Some(aliases) = enum_alias_map.get(snake_name.as_str()) {
                    let mut match_arms: Vec<String> = aliases
                        .iter()
                        .map(|(canonical, alias)| {
                            format!("\"{}\" => \"{}\".to_string()", canonical, alias)
                        })
                        .collect();
                    let fallback = if has_hyphens {
                        "_ => s.replace('-', \"_\")"
                    } else {
                        "_ => s.to_string()"
                    };
                    match_arms.push(fallback.to_string());
                    format!("Some(|s: &str| match s {{ {} }})", match_arms.join(", "))
                } else if has_hyphens {
                    "Some(|s: &str| s.replace('-', \"_\"))".to_string()
                } else {
                    "None".to_string()
                }
            };
            format!(
                "AttributeType::Custom {{\n\
                 \x20               name: \"{}\".to_string(),\n\
                 \x20               base: Box::new(AttributeType::String),\n\
                 \x20               validate: {},\n\
                 \x20               namespace: Some(\"{}\".to_string()),\n\
                 \x20               to_dsl: {},\n\
                 \x20           }}",
                ei.type_name, validate_fn, namespace, to_dsl_code
            )
        } else {
            field_type
        };

        let mut field_code = format!("StructField::new(\"{}\", {})", snake_name, field_type);
        if is_required {
            field_code.push_str(".required()");
        }
        if let Some(desc) = SmithyModel::documentation(&member_ref.traits) {
            let escaped = escape_description(desc);
            let truncated = truncate_str(&escaped, 150);
            field_code.push_str(&format!(".with_description(\"{}\")", truncated));
        }
        field_code.push_str(&format!(".with_provider_name(\"{}\")", field_name));
        fields.push(field_code);
    }

    let fields_str = fields.join(",\n                    ");
    format!(
        "AttributeType::Struct {{\n\
         \x20                   name: \"{}\".to_string(),\n\
         \x20                   fields: vec![\n\
         \x20                   {}\n\
         \x20                   ],\n\
         \x20               }}",
        struct_name, fields_str
    )
}

/// Get integer range for a field from Smithy traits or known overrides.
fn get_int_range(model: &SmithyModel, target: &str, field_name: &str) -> Option<IntRange> {
    // Check Smithy range trait on the target shape
    if let Some(shape) = model.get_shape(target) {
        let traits = match shape {
            carina_smithy::Shape::Integer(t) => &t.traits,
            carina_smithy::Shape::Long(t) => &t.traits,
            _ => {
                // Check known overrides for the field name
                return known_int_range_overrides()
                    .get(field_name)
                    .map(|&(min, max)| IntRange { min, max });
            }
        };
        if let Some(range_val) = traits.get("smithy.api#range") {
            let min = range_val.get("min").and_then(|v| v.as_i64());
            let max = range_val.get("max").and_then(|v| v.as_i64());
            if let (Some(min), Some(max)) = (min, max) {
                return Some(IntRange { min, max });
            }
        }
    }

    // Check known overrides
    known_int_range_overrides()
        .get(field_name)
        .map(|&(min, max)| IntRange { min, max })
}

/// Generate mod.rs that includes all generated modules.
fn generate_mod_rs(dsl_names: &[&str]) -> String {
    let mut code = String::new();

    code.push_str(
        "//! Auto-generated AWS provider resource schemas\n\
         //!\n\
         //! DO NOT EDIT MANUALLY - regenerate with:\n\
         //!   ./carina-provider-aws/scripts/generate-schemas-smithy.sh\n\n\
         use carina_core::schema::ResourceSchema;\n\n\
         // Re-export all types and validators from types so that\n\
         // generated schema files can use `super::` to access them.\n\
         pub use super::types::*;\n\n",
    );

    // Sort by module name for deterministic output
    let mut sorted: Vec<&str> = dsl_names.to_vec();
    sorted.sort_by_key(|n| module_name(n));

    // Module declarations
    for name in &sorted {
        code.push_str(&format!("pub mod {};\n", module_name(name)));
    }

    // configs() function
    code.push_str(
        "\n/// Returns all generated schema configs\n\
         pub fn configs() -> Vec<AwsSchemaConfig> {\n\
         \x20   vec![\n",
    );
    for name in &sorted {
        let mn = module_name(name);
        code.push_str(&format!("\x20       {}::{}_config(),\n", mn, mn));
    }
    code.push_str(
        "\x20   ]\n\
         }\n\n\
         /// Returns all generated schemas (for backward compatibility)\n\
         pub fn schemas() -> Vec<ResourceSchema> {\n\
         \x20   configs().into_iter().map(|c| c.schema).collect()\n\
         }\n\n",
    );

    // get_enum_valid_values()
    code.push_str(
        "/// Get valid enum values for a given resource type and attribute name.\n\
         /// Used during read-back to normalize AWS-returned values to canonical DSL form.\n\
         ///\n\
         /// Auto-generated from schema enum constants.\n\
         #[allow(clippy::type_complexity)]\n\
         pub fn get_enum_valid_values(resource_type: &str, attr_name: &str) -> Option<&'static [&'static str]> {\n\
         \x20   let modules: &[(&str, &[(&str, &[&str])])] = &[\n",
    );
    for name in &sorted {
        code.push_str(&format!(
            "\x20       {}::enum_valid_values(),\n",
            module_name(name)
        ));
    }
    code.push_str(
        "\x20   ];\n\
         \x20   for (rt, attrs) in modules {\n\
         \x20       if *rt == resource_type {\n\
         \x20           for (attr, values) in *attrs {\n\
         \x20               if *attr == attr_name {\n\
         \x20                   return Some(values);\n\
         \x20               }\n\
         \x20           }\n\
         \x20           return None;\n\
         \x20       }\n\
         \x20   }\n\
         \x20   None\n\
         }\n\n",
    );

    // get_enum_alias_reverse()
    code.push_str(
        "/// Maps DSL alias values back to canonical AWS values.\n\
         /// Dispatches to per-module enum_alias_reverse() functions.\n\
         pub fn get_enum_alias_reverse(resource_type: &str, attr_name: &str, value: &str) -> Option<&'static str> {\n",
    );
    for name in &sorted {
        let mn = module_name(name);
        code.push_str(&format!(
            "\x20   if resource_type == \"{}\" {{\n\
             \x20       return {}::enum_alias_reverse(attr_name, value);\n\
             \x20   }}\n",
            name, mn
        ));
    }
    code.push_str("    None\n}\n");

    code
}

// ── Markdown documentation generation ──

/// Generate markdown documentation for a single resource.
fn generate_markdown_resource(res: &ResourceDef, model: &SmithyModel) -> Result<String> {
    let ns = res.service_namespace;
    let namespace = format!("aws.{}", res.name);

    let exclude: HashSet<&str> = res.exclude_fields.iter().copied().collect();
    let type_overrides: HashMap<&str, &str> = res.type_overrides.iter().copied().collect();
    let required_overrides: HashSet<&str> = res.required_overrides.iter().copied().collect();
    let read_only_overrides: HashSet<&str> = res.read_only_overrides.iter().copied().collect();
    let extra_read_only: HashSet<&str> = res.extra_read_only.iter().copied().collect();
    let enum_alias_map: HashMap<&str, Vec<(&str, &str)>> = {
        let mut m: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
        for (attr, alias, canonical) in &res.enum_aliases {
            m.entry(attr).or_default().push((canonical, alias));
        }
        m
    };

    // Resolve create input
    let create_op_id = format!("{}#{}", ns, res.create_op);
    let create_input = model
        .operation_input(&create_op_id)
        .with_context(|| format!("Cannot find create input for {}", create_op_id))?;

    // Resolve read structure
    let read_structure = if let Some(read_struct_name) = res.read_structure {
        let read_structure_id = format!("{}#{}", ns, read_struct_name);
        Some(
            model
                .get_structure(&read_structure_id)
                .with_context(|| format!("Cannot find read structure {}", read_structure_id))?,
        )
    } else {
        None
    };

    // Resolve update fields
    let mut updatable_fields: HashSet<String> = HashSet::new();
    for update_op in &res.update_ops {
        for field in &update_op.fields {
            updatable_fields.insert(field.to_string());
        }
    }

    // Collect writable fields
    let mut writable_fields: BTreeMap<String, &carina_smithy::ShapeRef> = BTreeMap::new();
    for (name, member_ref) in &create_input.members {
        if exclude.contains(name.as_str()) || name == res.identifier || name == "Tags" {
            continue;
        }
        writable_fields.insert(name.clone(), member_ref);
    }

    // Read ops fields
    let mut read_op_read_only: BTreeMap<String, &carina_smithy::ShapeRef> = BTreeMap::new();
    for read_op in &res.read_ops {
        let op_id = format!("{}#{}", ns, read_op.operation);
        let output = model
            .operation_output(&op_id)
            .with_context(|| format!("Cannot find output for {}", op_id))?;
        for (field_name, rename) in &read_op.fields {
            let effective_name = rename.unwrap_or(field_name);
            if let Some(member_ref) = output.members.get(*field_name) {
                if updatable_fields.contains(effective_name)
                    && !writable_fields.contains_key(effective_name)
                {
                    writable_fields.insert(effective_name.to_string(), member_ref);
                } else if !writable_fields.contains_key(effective_name) {
                    read_op_read_only.insert(effective_name.to_string(), member_ref);
                }
            }
        }
    }

    // Add updatable-only fields from read structure
    if let Some(read_struct) = read_structure {
        for (name, member_ref) in &read_struct.members {
            if exclude.contains(name.as_str()) || name == "Tags" || name == res.identifier {
                continue;
            }
            if !writable_fields.contains_key(name) && updatable_fields.contains(name.as_str()) {
                writable_fields.insert(name.clone(), member_ref);
            }
        }
    }

    // Read-only fields
    let mut read_only_fields: BTreeMap<String, &carina_smithy::ShapeRef> = BTreeMap::new();
    if let Some(read_struct) = read_structure {
        for (name, member_ref) in &read_struct.members {
            if exclude.contains(name.as_str())
                || name == "Tags"
                || writable_fields.contains_key(name)
            {
                continue;
            }
            if name == res.identifier || extra_read_only.contains(name.as_str()) {
                read_only_fields.insert(name.clone(), member_ref);
            }
        }
    }
    for (name, member_ref) in read_op_read_only {
        if !writable_fields.contains_key(&name) && !read_only_fields.contains_key(&name) {
            read_only_fields.insert(name, member_ref);
        }
    }

    // Collect enum info for documentation
    let mut all_enums: BTreeMap<String, EnumInfo> = BTreeMap::new();
    // Struct definitions for documentation
    let mut struct_defs: BTreeMap<String, Vec<(String, &carina_smithy::ShapeRef)>> =
        BTreeMap::new();

    // Build attr info for writable fields
    struct MdAttrInfo {
        snake_name: String,
        type_display: String,
        is_required: bool,
        description: Option<String>,
    }

    let mut writable_attrs: Vec<MdAttrInfo> = Vec::new();
    for (name, member_ref) in &writable_fields {
        let snake_name = name.to_snake_case();
        let is_required = (SmithyModel::is_required(member_ref)
            || required_overrides.contains(name.as_str()))
            && !read_only_overrides.contains(name.as_str());
        let description = SmithyModel::documentation(&member_ref.traits).map(|s| s.to_string());
        let type_display = type_display_string_md(
            model,
            &member_ref.target,
            name,
            &namespace,
            &type_overrides,
            &mut all_enums,
            &mut struct_defs,
        );

        writable_attrs.push(MdAttrInfo {
            snake_name,
            type_display,
            is_required,
            description,
        });
    }

    let mut read_only_attrs: Vec<MdAttrInfo> = Vec::new();
    for (name, member_ref) in &read_only_fields {
        let snake_name = name.to_snake_case();
        let description = SmithyModel::documentation(&member_ref.traits).map(|s| s.to_string());
        let type_display = type_display_string_md(
            model,
            &member_ref.target,
            name,
            &namespace,
            &type_overrides,
            &mut all_enums,
            &mut struct_defs,
        );

        read_only_attrs.push(MdAttrInfo {
            snake_name,
            type_display,
            is_required: false,
            description,
        });
    }

    // Build markdown output
    let mut md = String::new();

    // Title
    md.push_str(&format!("# aws.{}\n\n", res.name));
    md.push_str(&format!(
        "CloudFormation Type: `{}`\n\n",
        cf_type_name(res.name)
    ));

    // Description
    let desc_traits = if let Some(read_struct) = read_structure {
        Some(&read_struct.traits)
    } else {
        Some(&create_input.traits)
    };
    if let Some(traits) = desc_traits
        && let Some(desc) = SmithyModel::documentation(traits)
    {
        let cleaned = strip_html_tags(desc).replace('\n', " ").replace("  ", " ");
        md.push_str(&format!("{}\n\n", cleaned.trim()));
    }

    // Argument Reference
    md.push_str("## Argument Reference\n\n");

    for attr in &writable_attrs {
        md.push_str(&format!("### `{}`\n\n", attr.snake_name));
        md.push_str(&format!("- **Type:** {}\n", attr.type_display));
        md.push_str(&format!(
            "- **Required:** {}\n",
            if attr.is_required { "Yes" } else { "No" }
        ));
        md.push('\n');

        if let Some(ref desc) = attr.description {
            let cleaned = strip_html_tags(desc).replace('\n', " ").replace("  ", " ");
            md.push_str(&format!("{}\n\n", cleaned.trim()));
        }
    }

    // Tags
    if res.has_tags {
        md.push_str("### `tags`\n\n");
        md.push_str("- **Type:** Map\n");
        md.push_str("- **Required:** No\n\n");
        md.push_str("The tags for the resource.\n\n");
    }

    // Enum Values section
    if !all_enums.is_empty() {
        md.push_str("## Enum Values\n\n");
        for (prop_name, enum_info) in &all_enums {
            let attr_name = prop_name.to_snake_case();
            let has_hyphens = enum_info.values.iter().any(|v| v.contains('-'));
            let prop_aliases = enum_alias_map.get(attr_name.as_str());

            md.push_str(&format!("### {} ({})\n\n", attr_name, enum_info.type_name));
            md.push_str("| Value | DSL Identifier |\n");
            md.push_str("|-------|----------------|\n");

            for value in &enum_info.values {
                let dsl_value = if let Some(alias_list) = prop_aliases {
                    if let Some((_, alias)) = alias_list.iter().find(|(c, _)| *c == value.as_str())
                    {
                        alias.to_string()
                    } else if has_hyphens {
                        value.replace('-', "_")
                    } else {
                        value.clone()
                    }
                } else if has_hyphens {
                    value.replace('-', "_")
                } else {
                    value.clone()
                };
                let dsl_id = format!("{}.{}.{}", namespace, enum_info.type_name, dsl_value);
                md.push_str(&format!("| `{}` | `{}` |\n", value, dsl_id));
            }
            md.push('\n');

            let first_value = enum_info.values.first().map(|s| s.as_str()).unwrap_or("");
            let first_dsl = if let Some(alias_list) = prop_aliases {
                if let Some((_, alias)) = alias_list.iter().find(|(c, _)| *c == first_value) {
                    alias.to_string()
                } else if has_hyphens {
                    first_value.replace('-', "_")
                } else {
                    first_value.to_string()
                }
            } else if has_hyphens {
                first_value.replace('-', "_")
            } else {
                first_value.to_string()
            };
            md.push_str(&format!(
                "Shorthand formats: `{}` or `{}.{}`\n\n",
                first_dsl, enum_info.type_name, first_dsl,
            ));
        }
    }

    // Struct Definitions section
    if !struct_defs.is_empty() {
        md.push_str("## Struct Definitions\n\n");
        for (struct_name, fields) in &struct_defs {
            md.push_str(&format!("### {}\n\n", struct_name));
            md.push_str("| Field | Type | Required | Description |\n");
            md.push_str("|-------|------|----------|-------------|\n");
            for (field_name, member_ref) in fields {
                let snake_name = field_name.to_snake_case();
                let is_required = SmithyModel::is_required(member_ref);
                let field_type_display = type_display_string_md(
                    model,
                    &member_ref.target,
                    field_name,
                    &namespace,
                    &type_overrides,
                    &mut all_enums,
                    &mut BTreeMap::new(),
                );
                let desc = SmithyModel::documentation(&member_ref.traits)
                    .map(|s| {
                        let cleaned = strip_html_tags(s).replace('\n', " ").replace("  ", " ");
                        let trimmed = cleaned.trim().to_string();
                        if trimmed.len() > 100 {
                            // Find a safe UTF-8 boundary
                            let boundary = trimmed
                                .char_indices()
                                .take_while(|&(i, _)| i <= 100)
                                .last()
                                .map(|(i, _)| i)
                                .unwrap_or(0);
                            format!("{}...", &trimmed[..boundary])
                        } else {
                            trimmed
                        }
                    })
                    .unwrap_or_default();
                md.push_str(&format!(
                    "| `{}` | {} | {} | {} |\n",
                    snake_name,
                    field_type_display,
                    if is_required { "Yes" } else { "No" },
                    desc
                ));
            }
            md.push('\n');
        }
    }

    // Attribute Reference (read-only)
    if !read_only_attrs.is_empty() {
        md.push_str("## Attribute Reference\n\n");
        for attr in &read_only_attrs {
            md.push_str(&format!("### `{}`\n\n", attr.snake_name));
            md.push_str(&format!("- **Type:** {}\n\n", attr.type_display));
        }
    }

    Ok(md)
}

/// Determine the display string for a type in markdown docs.
#[allow(clippy::only_used_in_recursion)]
fn type_display_string_md<'a>(
    model: &'a SmithyModel,
    target: &str,
    field_name: &str,
    namespace: &str,
    type_overrides: &HashMap<&str, &str>,
    all_enums: &mut BTreeMap<String, EnumInfo>,
    struct_defs: &mut BTreeMap<String, Vec<(String, &'a carina_smithy::ShapeRef)>>,
) -> String {
    // Check type overrides
    if let Some(&override_type) = type_overrides.get(field_name) {
        return type_code_to_display(override_type);
    }

    // Check known enum overrides
    if let Some(values) = known_enum_overrides().get(field_name) {
        let type_name = field_name.to_string();
        let enum_info = EnumInfo {
            type_name: type_name.clone(),
            values: values.iter().map(|s| s.to_string()).collect(),
        };
        all_enums
            .entry(field_name.to_string())
            .or_insert_with(|| enum_info);
        return format!(
            "[Enum ({})](#{}-{})",
            type_name,
            field_name.to_snake_case(),
            type_name.to_lowercase()
        );
    }

    let kind = model.shape_kind(target);

    match kind {
        Some(ShapeKind::String) => {
            if let Some(inferred) = infer_string_type(field_name) {
                return type_code_to_display(&inferred);
            }
            let lower = field_name.to_lowercase();
            if lower.contains("cidr") {
                return if lower.contains("ipv6") {
                    "Ipv6Cidr".to_string()
                } else {
                    "Ipv4Cidr".to_string()
                };
            }
            if (lower.contains("ipaddress")
                || lower.ends_with("ip")
                || lower.contains("ipaddresses"))
                && !lower.contains("cidr")
                && !lower.contains("count")
                && !lower.contains("type")
            {
                return if lower.contains("ipv6") {
                    "Ipv6Address".to_string()
                } else {
                    "Ipv4Address".to_string()
                };
            }
            if is_ipam_pool_id_property(field_name) {
                return "IpamPoolId".to_string();
            }
            if is_aws_resource_id_property(field_name) {
                return resource_id_display(field_name);
            }
            if lower.ends_with("arn") || lower.ends_with("arns") || lower.contains("_arn") {
                return "Arn".to_string();
            }
            if lower == "availabilityzone" {
                return "AvailabilityZone".to_string();
            }
            "String".to_string()
        }
        Some(ShapeKind::Boolean) => "Bool".to_string(),
        Some(ShapeKind::Integer) | Some(ShapeKind::Long) => {
            let range = get_int_range(model, target, field_name);
            if let Some(r) = range {
                format!("Int({}..={})", r.min, r.max)
            } else {
                "Int".to_string()
            }
        }
        Some(ShapeKind::Float) | Some(ShapeKind::Double) => "Float".to_string(),
        Some(ShapeKind::Enum) => {
            if let Some(values) = model.enum_values(target) {
                let type_name = field_name.to_string();
                let string_values: Vec<String> = values.into_iter().map(|(_, v)| v).collect();
                let enum_info = EnumInfo {
                    type_name: type_name.clone(),
                    values: string_values,
                };
                all_enums
                    .entry(field_name.to_string())
                    .or_insert_with(|| enum_info);
                format!(
                    "[Enum ({})](#{}-{})",
                    type_name,
                    field_name.to_snake_case(),
                    type_name.to_lowercase()
                )
            } else {
                "String".to_string()
            }
        }
        Some(ShapeKind::IntEnum) => "Int".to_string(),
        Some(ShapeKind::List) => {
            if let Some(carina_smithy::Shape::List(list_shape)) = model.get_shape(target) {
                let item_display = type_display_string_md(
                    model,
                    &list_shape.member.target,
                    field_name,
                    namespace,
                    type_overrides,
                    all_enums,
                    struct_defs,
                );
                format!("`List<{}>`", item_display)
            } else {
                "`List<String>`".to_string()
            }
        }
        Some(ShapeKind::Map) => "Map".to_string(),
        Some(ShapeKind::Structure) => {
            let shape_name = SmithyModel::shape_name(target);
            if shape_name == "TagList" || shape_name == "Tag" {
                return "Map".to_string();
            }
            if shape_name == "AttributeBooleanValue" {
                return "Bool".to_string();
            }
            if let Some(structure) = model.get_structure(target) {
                // Register struct definition for docs
                let fields: Vec<(String, &carina_smithy::ShapeRef)> = structure
                    .members
                    .iter()
                    .map(|(n, r)| (n.clone(), r))
                    .collect();
                struct_defs.entry(shape_name.to_string()).or_insert(fields);
                format!("[Struct({})](#{})", shape_name, shape_name.to_lowercase())
            } else {
                "String".to_string()
            }
        }
        _ => {
            if let Some(inferred) = infer_string_type(field_name) {
                type_code_to_display(&inferred)
            } else {
                "String".to_string()
            }
        }
    }
}

/// Convert a Rust type code string to a human-readable display name.
fn type_code_to_display(type_code: &str) -> String {
    match type_code {
        "AttributeType::String" => "String".to_string(),
        "AttributeType::Bool" => "Bool".to_string(),
        "AttributeType::Int" => "Int".to_string(),
        s if s.contains("ipv4_cidr") => "Ipv4Cidr".to_string(),
        s if s.contains("ipv6_cidr") => "Ipv6Cidr".to_string(),
        s if s.contains("ipv4_address") => "Ipv4Address".to_string(),
        s if s.contains("ipv6_address") => "Ipv6Address".to_string(),
        s if s.contains("iam_role_arn") => "IamRoleArn".to_string(),
        s if s.contains("iam_policy_arn") => "IamPolicyArn".to_string(),
        s if s.contains("kms_key_arn") => "KmsKeyArn".to_string(),
        s if s.contains("kms_key_id") => "KmsKeyId".to_string(),
        s if s.contains("vpc_id") => "VpcId".to_string(),
        s if s.contains("subnet_id") => "SubnetId".to_string(),
        s if s.contains("security_group_id") => "SecurityGroupId".to_string(),
        s if s.contains("ipam_pool_id") => "IpamPoolId".to_string(),
        s if s.contains("arn()") => "Arn".to_string(),
        s if s.contains("aws_resource_id") => "AwsResourceId".to_string(),
        s if s.contains("availability_zone") => "AvailabilityZone".to_string(),
        _ => type_code
            .trim_start_matches("super::")
            .trim_end_matches("()")
            .to_string(),
    }
}

/// Get the human-readable display name for a resource ID type.
fn resource_id_display(prop_name: &str) -> String {
    match classify_resource_id(prop_name) {
        ResourceIdKind::VpcId => "VpcId".to_string(),
        ResourceIdKind::SubnetId => "SubnetId".to_string(),
        ResourceIdKind::SecurityGroupId => "SecurityGroupId".to_string(),
        ResourceIdKind::EgressOnlyInternetGatewayId => "EgressOnlyInternetGatewayId".to_string(),
        ResourceIdKind::InternetGatewayId => "InternetGatewayId".to_string(),
        ResourceIdKind::RouteTableId => "RouteTableId".to_string(),
        ResourceIdKind::NatGatewayId => "NatGatewayId".to_string(),
        ResourceIdKind::VpcPeeringConnectionId => "VpcPeeringConnectionId".to_string(),
        ResourceIdKind::TransitGatewayId => "TransitGatewayId".to_string(),
        ResourceIdKind::VpnGatewayId => "VpnGatewayId".to_string(),
        ResourceIdKind::VpcEndpointId => "VpcEndpointId".to_string(),
        ResourceIdKind::Generic => "AwsResourceId".to_string(),
    }
}

// ── Type inference helpers (ported from codegen.rs) ──

fn known_string_type_overrides() -> &'static HashMap<&'static str, &'static str> {
    static OVERRIDES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("DefaultSecurityGroup", "super::security_group_id()");
        m.insert("DefaultNetworkAcl", "super::aws_resource_id()");
        m.insert("DeliverCrossAccountRole", "super::iam_role_arn()");
        m.insert("DeliverLogsPermissionArn", "super::iam_role_arn()");
        m.insert("PeerRoleArn", "super::iam_role_arn()");
        m.insert("PermissionsBoundary", "super::iam_policy_arn()");
        m.insert("ManagedPolicyArns", "super::iam_policy_arn()");
        m.insert("KmsKeyId", "super::kms_key_arn()");
        m.insert("KMSMasterKeyID", "super::kms_key_id()");
        m.insert("ReplicaKmsKeyID", "super::kms_key_id()");
        m.insert("KmsKeyArn", "super::kms_key_arn()");
        m
    });
    &OVERRIDES
}

fn known_enum_overrides() -> &'static HashMap<&'static str, Vec<&'static str>> {
    static OVERRIDES: LazyLock<HashMap<&'static str, Vec<&'static str>>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("IpProtocol", vec!["tcp", "udp", "icmp", "icmpv6", "-1"]);
        m.insert("HostnameType", vec!["ip-name", "resource-name"]);
        m
    });
    &OVERRIDES
}

fn known_int_range_overrides() -> &'static HashMap<&'static str, (i64, i64)> {
    static OVERRIDES: LazyLock<HashMap<&'static str, (i64, i64)>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("Ipv4NetmaskLength", (0, 32));
        m.insert("Ipv6NetmaskLength", (0, 128));
        m.insert("FromPort", (-1, 65535));
        m.insert("ToPort", (-1, 65535));
        m
    });
    &OVERRIDES
}

fn infer_string_type(prop_name: &str) -> Option<String> {
    // Check known string type overrides
    if let Some(&override_type) = known_string_type_overrides().get(prop_name) {
        return Some(override_type.to_string());
    }
    // Check ARN pattern
    let prop_lower = prop_name.to_lowercase();
    if prop_lower.ends_with("arn") || prop_lower.ends_with("arns") || prop_lower.contains("_arn") {
        return Some("super::arn()".to_string());
    }
    // Check resource ID pattern
    if is_aws_resource_id_property(prop_name) {
        return Some(get_resource_id_type(prop_name).to_string());
    }
    None
}

fn is_aws_resource_id_property(prop_name: &str) -> bool {
    let lower = prop_name.to_lowercase();
    let resource_id_suffixes = [
        "vpcid",
        "subnetid",
        "groupid",
        "gatewayid",
        "routetableid",
        "allocationid",
        "networkinterfaceid",
        "instanceid",
        "endpointid",
        "connectionid",
        "prefixlistid",
        "eniid",
    ];
    if lower.contains("owner") || lower.contains("availabilityzone") || lower == "resourceid" {
        return false;
    }
    let singular = if lower.ends_with("ids") {
        &lower[..lower.len() - 1]
    } else {
        &lower
    };
    resource_id_suffixes
        .iter()
        .any(|suffix| lower.ends_with(suffix) || singular.ends_with(suffix))
}

fn is_ipam_pool_id_property(prop_name: &str) -> bool {
    let lower = prop_name.to_lowercase();
    if lower.contains("owner") || lower.contains("availabilityzone") || lower == "resourceid" {
        return false;
    }
    lower.ends_with("poolid")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceIdKind {
    VpcId,
    SubnetId,
    SecurityGroupId,
    EgressOnlyInternetGatewayId,
    InternetGatewayId,
    RouteTableId,
    NatGatewayId,
    VpcPeeringConnectionId,
    TransitGatewayId,
    VpnGatewayId,
    VpcEndpointId,
    Generic,
}

fn classify_resource_id(prop_name: &str) -> ResourceIdKind {
    let lower = prop_name.to_lowercase();
    if lower.ends_with("vpcid") || lower == "vpcid" {
        return ResourceIdKind::VpcId;
    }
    if lower.ends_with("subnetid") || lower == "subnetid" {
        return ResourceIdKind::SubnetId;
    }
    if (lower.contains("securitygroup") || lower.contains("groupid")) && lower.ends_with("id") {
        return ResourceIdKind::SecurityGroupId;
    }
    if lower.contains("egressonlyinternetgateway") && lower.ends_with("id") {
        return ResourceIdKind::EgressOnlyInternetGatewayId;
    }
    if lower.contains("internetgateway") && lower.ends_with("id") {
        return ResourceIdKind::InternetGatewayId;
    }
    if lower.contains("routetable") && lower.ends_with("id") {
        return ResourceIdKind::RouteTableId;
    }
    if lower.contains("natgateway") && lower.ends_with("id") {
        return ResourceIdKind::NatGatewayId;
    }
    if lower.contains("peeringconnection") && lower.ends_with("id") {
        return ResourceIdKind::VpcPeeringConnectionId;
    }
    if lower.contains("transitgateway") && lower.ends_with("id") {
        return ResourceIdKind::TransitGatewayId;
    }
    if lower.contains("vpngateway") && lower.ends_with("id") {
        return ResourceIdKind::VpnGatewayId;
    }
    if lower.contains("vpcendpoint") && lower.ends_with("id") {
        return ResourceIdKind::VpcEndpointId;
    }
    ResourceIdKind::Generic
}

fn get_resource_id_type(prop_name: &str) -> &'static str {
    match classify_resource_id(prop_name) {
        ResourceIdKind::VpcId => "super::vpc_id()",
        ResourceIdKind::SubnetId => "super::subnet_id()",
        ResourceIdKind::SecurityGroupId => "super::security_group_id()",
        ResourceIdKind::EgressOnlyInternetGatewayId => "super::egress_only_internet_gateway_id()",
        ResourceIdKind::InternetGatewayId => "super::internet_gateway_id()",
        ResourceIdKind::RouteTableId => "super::route_table_id()",
        ResourceIdKind::NatGatewayId => "super::nat_gateway_id()",
        ResourceIdKind::VpcPeeringConnectionId => "super::vpc_peering_connection_id()",
        ResourceIdKind::TransitGatewayId => "super::transit_gateway_id()",
        ResourceIdKind::VpnGatewayId => "super::vpn_gateway_id()",
        ResourceIdKind::VpcEndpointId => "super::vpc_endpoint_id()",
        ResourceIdKind::Generic => "super::aws_resource_id()",
    }
}

/// Map resource name to CloudFormation type name for backward compatibility.
fn cf_type_name(resource_name: &str) -> &'static str {
    match resource_name {
        "ec2.vpc" => "AWS::EC2::VPC",
        "ec2.subnet" => "AWS::EC2::Subnet",
        "ec2.internet_gateway" => "AWS::EC2::InternetGateway",
        "ec2.route_table" => "AWS::EC2::RouteTable",
        "ec2.route" => "AWS::EC2::Route",
        "ec2.security_group" => "AWS::EC2::SecurityGroup",
        "ec2.security_group_ingress" => "AWS::EC2::SecurityGroupIngress",
        "ec2.security_group_egress" => "AWS::EC2::SecurityGroupEgress",
        "s3.bucket" => "AWS::S3::Bucket",
        _ => "UNKNOWN",
    }
}

fn strip_html_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result
}

fn escape_description(desc: &str) -> String {
    let stripped = strip_html_tags(desc);
    let collapsed = stripped.replace('"', "\\\"").replace(['\n', '\t'], " ");
    // Collapse all runs of multiple whitespace into a single space
    let mut result = String::with_capacity(collapsed.len());
    let mut prev_space = false;
    for c in collapsed.chars() {
        if c == ' ' {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(c);
            prev_space = false;
        }
    }
    result.trim().to_string()
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        // Find a safe UTF-8 boundary at or before max_len
        let boundary = s
            .char_indices()
            .take_while(|&(i, _)| i <= max_len)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        format!("{}...", &s[..boundary])
    } else {
        s.to_string()
    }
}
