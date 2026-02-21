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

    /// Generate only the specified resource (e.g., "ec2_vpc")
    #[arg(long)]
    resource: Option<String>,
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

fn main() -> Result<()> {
    let args = Args::parse();

    std::fs::create_dir_all(&args.output_dir)?;

    // Collect all resource definitions
    let all_resources = resource_defs::ec2_resources();

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

    // Generate each resource
    let mut generated_modules: Vec<&str> = Vec::new();
    for res in &resources {
        let model = models.get(res.service_namespace).unwrap();
        let code = generate_resource(res, model)?;

        let output_path = args.output_dir.join(format!("{}.rs", res.name));
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

    // Resolve read structure fields
    let read_structure_id = format!("{}#{}", ns, res.read_structure);
    let read_structure = model
        .get_structure(&read_structure_id)
        .with_context(|| format!("Cannot find read structure {}", read_structure_id))?;

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

    // Add updatable-only fields: fields in update ops that are NOT in create input.
    // First check read structure, then check update op inputs.
    // (e.g., EnableDnsHostnames for VPC is in ModifyVpcAttributeRequest but not in Vpc struct)
    for (name, member_ref) in &read_structure.members {
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
    for (name, member_ref) in &read_structure.members {
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

    // Header
    let resource_short = res.name.strip_prefix("ec2_").unwrap_or(res.name);
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
        res.name,
        cf_type_name(res.name),
        res.name,
        res.has_tags,
        namespace,
    ));

    // Description from read structure
    if let Some(desc) = SmithyModel::documentation(&read_structure.traits) {
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
            ("AttributeType::Int".to_string(), None)
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

/// Generate mod.rs that includes all modules plus s3_bucket (kept from CF codegen).
fn generate_mod_rs(modules: &[&str]) -> String {
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

    // Module declarations
    let mut all_modules: Vec<&str> = modules.to_vec();
    // Keep s3_bucket from CF codegen
    if !all_modules.contains(&"s3_bucket") {
        all_modules.push("s3_bucket");
    }
    all_modules.sort();
    for module in &all_modules {
        code.push_str(&format!("pub mod {};\n", module));
    }

    // configs() function
    code.push_str(
        "\n/// Returns all generated schema configs\n\
         pub fn configs() -> Vec<AwsSchemaConfig> {\n\
         \x20   vec![\n",
    );
    for module in &all_modules {
        code.push_str(&format!("\x20       {}::{}_config(),\n", module, module));
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
    for module in &all_modules {
        code.push_str(&format!("\x20       {}::enum_valid_values(),\n", module));
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
    for module in &all_modules {
        code.push_str(&format!(
            "\x20   if resource_type == \"{}\" {{\n\
             \x20       return {}::enum_alias_reverse(attr_name, value);\n\
             \x20   }}\n",
            module, module
        ));
    }
    code.push_str("    None\n}\n");

    code
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
        "ec2_vpc" => "AWS::EC2::VPC",
        "ec2_subnet" => "AWS::EC2::Subnet",
        "ec2_internet_gateway" => "AWS::EC2::InternetGateway",
        "ec2_route_table" => "AWS::EC2::RouteTable",
        "ec2_route" => "AWS::EC2::Route",
        "ec2_security_group" => "AWS::EC2::SecurityGroup",
        "ec2_security_group_ingress" => "AWS::EC2::SecurityGroupIngress",
        "ec2_security_group_egress" => "AWS::EC2::SecurityGroupEgress",
        _ => "UNKNOWN",
    }
}

fn escape_description(desc: &str) -> String {
    desc.replace('"', "\\\"")
        .replace('\n', " ")
        .replace("  ", " ")
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len])
    } else {
        s.to_string()
    }
}
