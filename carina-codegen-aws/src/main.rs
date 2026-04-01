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

use carina_codegen_aws::resource_defs::{self, ResourceDef};

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

/// Unified type override for resource-scoped property overrides.
/// Allows overriding string type, enum values, integer range, or integer enum
/// for a specific (resource_type, property_name) pair.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum TypeOverride {
    /// Override to a specific string type (e.g., "super::iam_role_arn()")
    StringType(&'static str),
    /// Override to an enum with specific values
    Enum(Vec<&'static str>),
    /// Override to an integer range (min, max)
    IntRange(i64, i64),
    /// Override to an integer enum with specific allowed values
    IntEnum(Vec<i64>),
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

/// Integer range constraint (supports one-sided ranges)
#[derive(Debug, Clone, Copy)]
struct IntRange {
    min: Option<i64>,
    max: Option<i64>,
}

/// Convert a DSL resource name to a Rust module name.
/// e.g., "ec2.vpc" -> "ec2_vpc", "ec2.security_group_ingress" -> "ec2_security_group_ingress"
fn module_name(name: &str) -> String {
    name.replace('.', "_")
}

/// Split a DSL resource name into (service, resource).
/// e.g., "ec2.vpc" -> ("ec2", "vpc"), "s3.bucket" -> ("s3", "bucket")
fn split_service_resource(name: &str) -> (&str, &str) {
    name.split_once('.').expect("DSL name must contain '.'")
}

fn main() -> Result<()> {
    let args = Args::parse();

    std::fs::create_dir_all(&args.output_dir)?;

    // Collect all resource definitions
    let mut all_resources = resource_defs::ec2_resources();
    all_resources.extend(resource_defs::s3_resources());
    all_resources.extend(resource_defs::sts_resources());

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
            // Generate each resource into service/resource directory structure
            let mut generated_modules: Vec<&str> = Vec::new();
            for res in &resources {
                let model = models.get(res.service_namespace).unwrap();
                let code = generate_resource(res, model)?;

                let (service, resource) = split_service_resource(res.name);
                let service_dir = args.output_dir.join(service);
                std::fs::create_dir_all(&service_dir)?;

                let output_path = service_dir.join(format!("{}.rs", resource));
                std::fs::write(&output_path, &code)
                    .with_context(|| format!("Failed to write {}", output_path.display()))?;
                eprintln!("Generated: {}", output_path.display());
                generated_modules.push(res.name);
            }

            // Generate per-service mod.rs files
            generate_service_mod_files(&args.output_dir, &generated_modules)?;

            // Generate top-level mod.rs
            let mod_rs = generate_mod_rs(&generated_modules);
            let mod_path = args.output_dir.join("mod.rs");
            std::fs::write(&mod_path, &mod_rs)
                .with_context(|| format!("Failed to write {}", mod_path.display()))?;
            eprintln!("Generated: {}", mod_path.display());
        }
        "provider" => {
            let code = generate_provider_code(&all_resources, &models);
            let output_path = args.output_dir.join("provider_generated.rs");
            std::fs::write(&output_path, &code)
                .with_context(|| format!("Failed to write {}", output_path.display()))?;
            eprintln!("Generated: {}", output_path.display());
        }
        "markdown" | "md" => {
            for res in &resources {
                let model = models.get(res.service_namespace).unwrap();
                let md = generate_markdown_resource(res, model)?;

                let (service, resource) = split_service_resource(res.name);
                let service_dir = args.output_dir.join(service);
                std::fs::create_dir_all(&service_dir)?;
                let output_path = service_dir.join(format!("{}.md", resource));
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

    // Data sources have no create_op — skip create input resolution
    let is_data_source = res.create_op.is_empty();

    // Resolve create input fields (skip for data sources)
    let create_input = if !is_data_source {
        let create_op_id = format!("{}#{}", ns, res.create_op);
        Some(
            model
                .operation_input(&create_op_id)
                .with_context(|| format!("Cannot find create input for {}", create_op_id))?,
        )
    } else {
        None
    };

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
        for field in update_op.fields.field_names() {
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

    // Collect writable fields from create input (empty for data sources)
    let mut writable_fields: BTreeMap<String, &carina_smithy::ShapeRef> = BTreeMap::new();
    if let Some(create_input) = &create_input {
        for (name, member_ref) in &create_input.members {
            if exclude.contains(name.as_str()) {
                continue;
            }
            if name == "Tags" {
                continue; // handled separately
            }
            writable_fields.insert(name.clone(), member_ref);
        }
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

    // Add extra writable fields from read structure
    for extra in &res.extra_writable {
        if writable_fields.contains_key(extra.name) {
            continue;
        }
        if let Some(source_field) = extra.read_source
            && let Some(read_struct) = read_structure
            && let Some(member_ref) = read_struct.members.get(source_field)
        {
            writable_fields.insert(extra.name.to_string(), member_ref);
        }
        // Synthetic fields (read_source = None) are handled after main attribute generation
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

    // Build extra_writable description override map
    let extra_writable_descs: HashMap<&str, Option<&str>> = res
        .extra_writable
        .iter()
        .map(|e| (e.name, e.description))
        .collect();
    // Set of field names that are from extra_writable (always create-only)
    let extra_writable_names: HashSet<&str> = res.extra_writable.iter().map(|e| e.name).collect();

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
        } else if extra_writable_names.contains(name.as_str()) {
            true // Extra writable fields are always create-only
        } else {
            create_only_overrides.contains(name.as_str())
                || !updatable_fields.contains(name.as_str())
        };
        // Use ExtraField description override if available, otherwise Smithy docs
        let description = if let Some(Some(desc)) = extra_writable_descs.get(name.as_str()) {
            Some(desc.to_string())
        } else {
            SmithyModel::documentation(&member_ref.traits).map(|s| s.to_string())
        };

        let (type_code, enum_info) = resolve_type(
            &mut TypeResolutionContext {
                model,
                namespace: &namespace,
                type_overrides: &type_overrides,
                enum_alias_map: &enum_alias_map,
                to_dsl_overrides: &to_dsl_overrides,
                all_enums: &mut all_enums,
                all_ranged_ints: &mut all_ranged_ints,
            },
            &member_ref.target,
            name,
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

    // Process synthetic extra writable fields (no read_source)
    for extra in &res.extra_writable {
        if extra.read_source.is_some() {
            continue; // Already handled via writable_fields
        }
        let snake_name = extra.name.to_snake_case();
        let type_code = if let Some(&override_type) = type_overrides.get(extra.name) {
            override_type.to_string()
        } else if let Some(inferred) = infer_string_type(extra.name) {
            inferred
        } else {
            "AttributeType::String".to_string()
        };
        attrs.push(AttrInfo {
            snake_name,
            provider_name: extra.name.to_string(),
            type_code,
            is_required: false,
            is_create_only: true,
            is_read_only: false,
            description: extra.description.map(|s| s.to_string()),
            enum_info: None,
        });
    }

    // Process read-only fields
    for (name, member_ref) in &read_only_fields {
        let snake_name = name.to_snake_case();
        let description = SmithyModel::documentation(&member_ref.traits).map(|s| s.to_string());

        let (type_code, enum_info) = resolve_type(
            &mut TypeResolutionContext {
                model,
                namespace: &namespace,
                type_overrides: &type_overrides,
                enum_alias_map: &enum_alias_map,
                to_dsl_overrides: &to_dsl_overrides,
                all_enums: &mut all_enums,
                all_ranged_ints: &mut all_ranged_ints,
            },
            &member_ref.target,
            name,
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
        .or_else(|| res.name.strip_prefix("sts."))
        .unwrap_or(res.name);
    let mut schema_imports = vec!["AttributeSchema", "ResourceSchema"];
    let needs_attribute_type = attrs.iter().any(|a| {
        matches!(
            a.type_code.as_str(),
            "AttributeType::String"
                | "AttributeType::Bool"
                | "AttributeType::Int"
                | "AttributeType::Float"
                | "AttributeType::Map"
        ) || a.type_code.starts_with("AttributeType::Custom")
    });
    if needs_attribute_type {
        schema_imports.insert(1, "AttributeType");
    }
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
    if has_ranged_ints {
        code.push_str("use carina_core::resource::Value;\n");
    }
    code.push_str(&format!(
        "use carina_core::schema::{{{}}};\n\n",
        schema_imports_str
    ));

    // Generate enum constants.
    for (prop_name, enum_info) in &all_enums {
        let const_name = format!("VALID_{}", prop_name.to_snake_case().to_uppercase());

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
            "const {}: &[&str] = &[{}];\n\n",
            const_name, values_str
        ));
    }

    // Generate range validation functions
    for (prop_name, range) in &all_ranged_ints {
        let fn_name = format!("validate_{}_range", prop_name.to_snake_case());
        let (condition, display) = int_range_condition_and_display(range.min, range.max);
        code.push_str(&format!(
            "fn {}(value: &Value) -> Result<(), String> {{\n\
             \x20   if let Value::Int(n) = value {{\n\
             \x20       if {} {{\n\
             \x20           Err(format!(\"Value {{}} is out of range {}\", n))\n\
             \x20       }} else {{\n\
             \x20           Ok(())\n\
             \x20       }}\n\
             \x20   }} else {{\n\
             \x20       Err(\"Expected integer\".to_string())\n\
             \x20   }}\n\
             }}\n\n",
            fn_name, condition, display
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
        create_input.as_ref().map(|ci| &ci.traits)
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

    // Mark data sources
    if is_data_source {
        code.push_str("\x20       .as_data_source()\n");
    }

    // Generate attributes
    for attr in &attrs {
        let type_code = if let Some(ref ei) = attr.enum_info {
            // Use shared schema enum type for constrained strings.
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
            let values_str = ei
                .values
                .iter()
                .map(|v| format!("\"{}\".to_string()", v))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "AttributeType::StringEnum {{\n\
                 \x20               name: \"{}\".to_string(),\n\
                 \x20               values: vec![{}],\n\
                 \x20               namespace: Some(\"{}\".to_string()),\n\
                 \x20               to_dsl: {},\n\
                 \x20           }}",
                ei.type_name, values_str, namespace, to_dsl_code
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
            "\n\x20               .with_provider_name(\"{}\")",
            attr.provider_name
        ));

        attr_code.push_str(",\n\x20       )\n");
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

/// Shared context for Smithy-to-Carina type resolution.
///
/// Groups the Smithy model, configuration overrides, and mutable collectors
/// that are passed to both `resolve_type` and `generate_struct_type`.
struct TypeResolutionContext<'a> {
    model: &'a SmithyModel,
    namespace: &'a str,
    type_overrides: &'a HashMap<&'a str, &'a str>,
    enum_alias_map: &'a HashMap<&'a str, Vec<(&'a str, &'a str)>>,
    to_dsl_overrides: &'a HashMap<&'a str, &'a str>,
    all_enums: &'a mut BTreeMap<String, EnumInfo>,
    all_ranged_ints: &'a mut BTreeMap<String, IntRange>,
}

/// Resolve a Smithy type to a Carina type code string.
/// Returns (type_code, Option<EnumInfo>).
/// Also populates collectors for enums and ranged ints discovered during resolution.
fn resolve_type(
    ctx: &mut TypeResolutionContext<'_>,
    target: &str,
    field_name: &str,
) -> (String, Option<EnumInfo>) {
    // Check type overrides first
    if let Some(&override_type) = ctx.type_overrides.get(field_name) {
        return (override_type.to_string(), None);
    }

    // Check known enum overrides
    if let Some(values) = known_enum_overrides().get(field_name) {
        let type_name = field_name.to_string();
        let enum_info = EnumInfo {
            type_name,
            values: values.iter().map(|s| s.to_string()).collect(),
        };
        ctx.all_enums
            .entry(field_name.to_string())
            .or_insert_with(|| enum_info.clone());
        return ("/* enum */".to_string(), Some(enum_info));
    }

    let kind = ctx.model.shape_kind(target);

    match kind {
        Some(ShapeKind::String) => {
            // Check name-based type inference (handles CIDR, IP, AZ, ARN, resource IDs, etc.)
            if let Some(inferred) = infer_string_type(field_name) {
                return (inferred, None);
            }

            ("AttributeType::String".to_string(), None)
        }
        Some(ShapeKind::Boolean) => ("AttributeType::Bool".to_string(), None),
        Some(ShapeKind::Integer) | Some(ShapeKind::Long) => {
            // Check for range traits on the target shape
            let range = get_int_range(ctx.model, target, field_name);
            if let Some(r) = range {
                ctx.all_ranged_ints
                    .entry(field_name.to_string())
                    .or_insert(r);
                let validate_fn = format!("validate_{}_range", field_name.to_snake_case());
                let display = range_display_string(r.min, r.max);
                (
                    format!(
                        "AttributeType::Custom {{\n\
                         \x20               name: \"Int({})\".to_string(),\n\
                         \x20               base: Box::new(AttributeType::Int),\n\
                         \x20               validate: {},\n\
                         \x20               namespace: None,\n\
                         \x20               to_dsl: None,\n\
                         \x20           }}",
                        display, validate_fn
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
            if let Some(values) = ctx.model.enum_values(target) {
                // Use the field name as type_name for consistency with CF codegen
                // (e.g., "InstanceTenancy" not "Tenancy")
                let type_name = field_name.to_string();
                let string_values: Vec<String> = values.into_iter().map(|(_, v)| v).collect();
                let enum_info = EnumInfo {
                    type_name,
                    values: string_values,
                };
                ctx.all_enums
                    .entry(field_name.to_string())
                    .or_insert_with(|| enum_info.clone());
                return ("/* enum */".to_string(), Some(enum_info));
            }
            ("AttributeType::String".to_string(), None)
        }
        Some(ShapeKind::IntEnum) => ("AttributeType::Int".to_string(), None),
        Some(ShapeKind::List) => {
            // Get list member type
            if let Some(carina_smithy::Shape::List(list_shape)) = ctx.model.get_shape(target) {
                let (item_type, _) = resolve_type(ctx, &list_shape.member.target, field_name);
                (format!("AttributeType::list({})", item_type), None)
            } else {
                (
                    "AttributeType::list(AttributeType::String)".to_string(),
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
            if let Some(structure) = ctx.model.get_structure(target) {
                let struct_code = generate_struct_type(ctx, shape_name, structure);
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
fn generate_struct_type(
    ctx: &mut TypeResolutionContext<'_>,
    struct_name: &str,
    structure: &carina_smithy::StructureShape,
) -> String {
    let mut fields: Vec<String> = Vec::new();
    for (field_name, member_ref) in &structure.members {
        let snake_name = field_name.to_snake_case();
        let is_required = SmithyModel::is_required(member_ref);

        let (field_type, enum_info) = resolve_type(ctx, &member_ref.target, field_name);

        // If enum detected, use shared schema enum type.
        let field_type = if let Some(ei) = enum_info {
            let to_dsl_code =
                if let Some(override_code) = ctx.to_dsl_overrides.get(snake_name.as_str()) {
                    override_code.to_string()
                } else {
                    let has_hyphens = ei.values.iter().any(|v| v.contains('-'));
                    if let Some(aliases) = ctx.enum_alias_map.get(snake_name.as_str()) {
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
            let values_str = ei
                .values
                .iter()
                .map(|v| format!("\"{}\".to_string()", v))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "AttributeType::StringEnum {{\n\
                 \x20               name: \"{}\".to_string(),\n\
                 \x20               values: vec![{}],\n\
                 \x20               namespace: Some(\"{}\".to_string()),\n\
                 \x20               to_dsl: {},\n\
                 \x20           }}",
                ei.type_name, values_str, ctx.namespace, to_dsl_code
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
                    .map(|&(min, max)| IntRange {
                        min: Some(min),
                        max: Some(max),
                    });
            }
        };
        if let Some(range_val) = traits.get("smithy.api#range") {
            let min = range_val.get("min").and_then(|v| v.as_i64());
            let max = range_val.get("max").and_then(|v| v.as_i64());
            if min.is_some() || max.is_some() {
                return Some(IntRange { min, max });
            }
        }
    }

    // Check known overrides
    known_int_range_overrides()
        .get(field_name)
        .map(|&(min, max)| IntRange {
            min: Some(min),
            max: Some(max),
        })
}

/// Generate per-service mod.rs files that declare resource modules.
fn generate_service_mod_files(output_dir: &std::path::Path, dsl_names: &[&str]) -> Result<()> {
    // Group resources by service
    let mut services: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for name in dsl_names {
        let (service, resource) = split_service_resource(name);
        services.entry(service).or_default().push(resource);
    }

    for (service, resources) in &services {
        let mut code = String::new();
        code.push_str(
            "//! Auto-generated — DO NOT EDIT MANUALLY\n\
             //!\n\
             //! Regenerate with:\n\
             //!   ./carina-provider-aws/scripts/generate-schemas-smithy.sh\n\n\
             // Re-export parent types so resource modules can use `super::` to access them.\n\
             pub use super::*;\n\n",
        );

        let mut sorted_resources: Vec<&&str> = resources.iter().collect();
        sorted_resources.sort();
        for resource in sorted_resources {
            code.push_str(&format!("pub mod {};\n", resource));
        }

        let mod_path = output_dir.join(service).join("mod.rs");
        std::fs::write(&mod_path, &code)
            .with_context(|| format!("Failed to write {}", mod_path.display()))?;
        eprintln!("Generated: {}", mod_path.display());
    }

    Ok(())
}

/// Generate mod.rs that includes all generated modules.
fn generate_mod_rs(dsl_names: &[&str]) -> String {
    let mut code = String::new();

    code.push_str(
        "//! Auto-generated AWS provider resource schemas\n\
         //!\n\
         //! DO NOT EDIT MANUALLY - regenerate with:\n\
         //!   ./carina-provider-aws/scripts/generate-schemas-smithy.sh\n\n\
         // Re-export all types and validators from types so that\n\
         // generated schema files can use `super::` to access them.\n\
         pub use super::types::*;\n\n",
    );

    // Sort by DSL name for deterministic output
    let mut sorted: Vec<&str> = dsl_names.to_vec();
    sorted.sort();

    // Collect unique services (sorted)
    let mut services: Vec<&str> = sorted.iter().map(|n| split_service_resource(n).0).collect();
    services.dedup();

    // Service module declarations
    for service in &services {
        code.push_str(&format!("pub mod {};\n", service));
    }

    // configs() function
    code.push_str(
        "\n/// Returns all generated schema configs\n\
         pub fn configs() -> Vec<AwsSchemaConfig> {\n\
         \x20   vec![\n",
    );
    for name in &sorted {
        let (service, resource) = split_service_resource(name);
        let mn = module_name(name);
        code.push_str(&format!(
            "\x20       {}::{}::{}_config(),\n",
            service, resource, mn
        ));
    }
    code.push_str(
        "\x20   ]\n\
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
        let (service, resource) = split_service_resource(name);
        code.push_str(&format!(
            "\x20       {}::{}::enum_valid_values(),\n",
            service, resource
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
        let (service, resource) = split_service_resource(name);
        code.push_str(&format!(
            "\x20   if resource_type == \"{}\" {{\n\
             \x20       return {}::{}::enum_alias_reverse(attr_name, value);\n\
             \x20   }}\n",
            name, service, resource
        ));
    }
    code.push_str("    None\n}\n");

    code
}

// ── Provider boilerplate generation ──

/// Generate the provider_generated.rs file from ResourceDef metadata and Smithy models.
/// Uses Smithy models to resolve types for read/write helper generation.
fn generate_provider_code(
    all_resources: &[ResourceDef],
    models: &HashMap<&str, SmithyModel>,
) -> String {
    let mut code = String::new();

    // Header
    code.push_str(
        "//! Auto-generated provider boilerplate\n\
         //!\n\
         //! DO NOT EDIT MANUALLY - regenerate with:\n\
         //!   ./carina-provider-aws/scripts/generate-provider.sh\n\n\
         use std::collections::HashMap;\n\n\
         use carina_core::provider::{ProviderError, ProviderResult};\n\
         use carina_core::resource::{Resource, ResourceId, State, Value};\n\
         use carina_core::utils::extract_enum_value;\n\n\
         use crate::AwsProvider;\n\n",
    );

    // Generate methods on AwsProvider
    code.push_str("// ===== Generated Methods on AwsProvider =====\n\n");
    code.push_str("impl AwsProvider {\n");

    // Simple delete methods
    for res in all_resources.iter().filter(|r| r.simple_delete) {
        let method_name = format!("delete_{}", res.name.replace('.', "_"));
        let client_field = client_field_name(res.service_namespace);
        let sdk_method = res.delete_op.to_snake_case();
        let id_setter = res.identifier.to_snake_case();

        // Human-readable resource name for error message
        let display_name = res
            .name
            .split('.')
            .next_back()
            .unwrap_or(res.name)
            .replace('_', " ");

        code.push_str(&format!(
            "\x20   /// Delete {} (generated)\n\
             \x20   pub(crate) async fn {}(\n\
             \x20       &self,\n\
             \x20       id: ResourceId,\n\
             \x20       identifier: &str,\n\
             \x20   ) -> ProviderResult<()> {{\n\
             \x20       self.{}.{}().{}(identifier).send().await.map_err(|e| {{\n\
             \x20           ProviderError::new(\"Failed to delete {}\")\n\
             \x20               .with_cause(e)\n\
             \x20               .for_resource(id.clone())\n\
             \x20       }})?;\n\
             \x20       Ok(())\n\
             \x20   }}\n\n",
            res.name, method_name, client_field, sdk_method, id_setter, display_name,
        ));
    }

    // No-op update methods (with optional tag handling)
    for res in all_resources.iter().filter(|r| r.noop_update) {
        let method_name = format!("update_{}", res.name.replace('.', "_"));
        let read_method = format!("read_{}", res.name.replace('.', "_"));

        if res.has_tags {
            // Tag-enabled noop update: apply tags then read back
            code.push_str(&format!(
                "\x20   /// Update {}: apply tag changes and read back (generated)\n\
                 \x20   pub(crate) async fn {}(\n\
                 \x20       &self,\n\
                 \x20       id: ResourceId,\n\
                 \x20       identifier: &str,\n\
                 \x20       from: &State,\n\
                 \x20       to: Resource,\n\
                 \x20   ) -> ProviderResult<State> {{\n\
                 \x20       self.apply_ec2_tags(&id, identifier, &to.attributes, Some(&from.attributes))\n\
                 \x20           .await?;\n\
                 \x20       self.{}(&id, Some(identifier)).await\n\
                 \x20   }}\n\n",
                res.name, method_name, read_method,
            ));
        } else {
            code.push_str(&format!(
                "\x20   /// Update {} (no-op, just read back current state) (generated)\n\
                 \x20   pub(crate) async fn {}(\n\
                 \x20       &self,\n\
                 \x20       id: ResourceId,\n\
                 \x20       identifier: &str,\n\
                 \x20       _to: Resource,\n\
                 \x20   ) -> ProviderResult<State> {{\n\
                 \x20       self.{}(&id, Some(identifier)).await\n\
                 \x20   }}\n\n",
                res.name, method_name, read_method,
            ));
        }
    }

    // Read helpers for read_ops (non-data-source resources only)
    for res in all_resources
        .iter()
        .filter(|r| !r.read_ops.is_empty() && !r.identifier.is_empty())
    {
        let model = match models.get(res.service_namespace) {
            Some(m) => m,
            None => continue,
        };
        let ns = res.service_namespace;
        let client_field = client_field_name(ns);
        let id_setter = res.identifier.to_snake_case();
        let resource_name = res.name.replace('.', "_");

        for read_op in &res.read_ops {
            let suffix = op_suffix(read_op.operation, res.identifier);
            let method_name = format!("read_{}_{}", resource_name, suffix);
            let sdk_method = read_op.operation.to_snake_case();

            // Resolve output structure
            let op_id = format!("{}#{}", ns, read_op.operation);
            let output = match model.operation_output(&op_id) {
                Some(o) => o,
                None => continue,
            };

            // Build defaults map
            let defaults: HashMap<&str, &str> = read_op.defaults.iter().copied().collect();

            // Method signature
            let op_desc = format!("{} {}", res.name, read_op.operation);
            code.push_str(&format!("\x20   /// Read {} (generated)\n", op_desc));
            code.push_str(&format!("\x20   pub(crate) async fn {}(\n", method_name));
            code.push_str("\x20       &self,\n");
            code.push_str("\x20       id: &ResourceId,\n");
            code.push_str("\x20       identifier: &str,\n");
            code.push_str("\x20       attributes: &mut HashMap<String, Value>,\n");
            code.push_str("\x20   ) -> ProviderResult<()> {\n");
            code.push_str(&format!(
                "\x20       let output = self.{}.{}().{}(identifier).send().await.map_err(|e| {{\n",
                client_field, sdk_method, id_setter
            ));
            code.push_str(&format!(
                "\x20           ProviderError::new(\"Failed to read {}\")\n",
                op_desc
            ));
            code.push_str("\x20               .with_cause(e)\n");
            code.push_str("\x20               .for_resource(id.clone())\n");
            code.push_str("\x20       })?;\n");

            // Extract each field
            for (field_name, rename) in &read_op.fields {
                let effective_name = rename.unwrap_or(field_name);
                let attr_snake = effective_name.to_snake_case();
                let accessor = field_name.to_snake_case();

                // Determine if field is an enum
                let is_enum = if let Some(member_ref) = output.members.get(*field_name) {
                    matches!(model.shape_kind(&member_ref.target), Some(ShapeKind::Enum))
                } else {
                    false
                };

                let value_expr = if is_enum {
                    "v.as_str().to_string()"
                } else {
                    "v.to_string()"
                };

                if let Some(default_value) = defaults.get(effective_name) {
                    code.push_str(&format!(
                        "\x20       let value = output.{}().map(|v| {}).unwrap_or_else(|| \"{}\".to_string());\n",
                        accessor, value_expr, default_value,
                    ));
                    code.push_str(&format!(
                        "\x20       attributes.insert(\"{}\".to_string(), Value::String(value));\n",
                        attr_snake,
                    ));
                } else {
                    code.push_str(&format!(
                        "\x20       if let Some(v) = output.{}() {{\n",
                        accessor,
                    ));
                    code.push_str(&format!(
                        "\x20           attributes.insert(\"{}\".to_string(), Value::String({}));\n",
                        attr_snake, value_expr,
                    ));
                    code.push_str("\x20       }\n");
                }
            }

            code.push_str("\x20       Ok(())\n");
            code.push_str("\x20   }\n\n");
        }
    }

    // Write helpers for update_ops with InsideStruct layout
    for res in all_resources.iter().filter(|r| {
        r.update_ops
            .iter()
            .any(|op| matches!(op.fields, resource_defs::FieldLayout::InsideStruct { .. }))
    }) {
        let model = match models.get(res.service_namespace) {
            Some(m) => m,
            None => continue,
        };
        let ns = res.service_namespace;
        let client_field = client_field_name(ns);
        let id_setter = res.identifier.to_snake_case();
        let resource_name = res.name.replace('.', "_");
        let sdk_crate_name = sdk_crate_name(ns);

        // Build reverse rename map from read_ops: effective_name -> original_smithy_name
        let mut reverse_rename: HashMap<&str, &str> = HashMap::new();
        for read_op in &res.read_ops {
            for (field_name, rename) in &read_op.fields {
                if let Some(renamed) = rename {
                    reverse_rename.insert(renamed, field_name);
                }
            }
        }

        for update_op in &res.update_ops {
            let resource_defs::FieldLayout::InsideStruct {
                name: struct_name,
                fields: update_fields,
            } = &update_op.fields
            else {
                continue;
            };
            let suffix = op_suffix(update_op.operation, res.identifier);
            let method_name = format!("write_{}_{}", resource_name, suffix);
            let sdk_method = update_op.operation.to_snake_case();
            let struct_setter = struct_name.to_snake_case();

            // Resolve the nested struct from the Put input
            let op_id = format!("{}#{}", ns, update_op.operation);
            let input = match model.operation_input(&op_id) {
                Some(i) => i,
                None => continue,
            };
            let struct_ref = match input.members.get(*struct_name) {
                Some(r) => r,
                None => continue,
            };
            let nested_struct = match model.get_structure(&struct_ref.target) {
                Some(s) => s,
                None => continue,
            };
            let struct_type_name = SmithyModel::shape_name(&struct_ref.target);

            // Collect field info and use types
            struct FieldInfo {
                attr_snake: String,
                builder_setter: String,
                enum_type_name: Option<String>,
            }
            let mut fields = Vec::new();
            let mut use_types: Vec<String> = vec![struct_type_name.to_string()];

            for effective_name in update_fields {
                let original_name = reverse_rename
                    .get(*effective_name)
                    .copied()
                    .unwrap_or(effective_name);
                let attr_snake = effective_name.to_snake_case();
                let builder_setter = original_name.to_snake_case();

                // Look up field in nested struct to resolve enum type
                let enum_type_name =
                    if let Some(member_ref) = nested_struct.members.get(original_name) {
                        if matches!(model.shape_kind(&member_ref.target), Some(ShapeKind::Enum)) {
                            let type_name = SmithyModel::shape_name(&member_ref.target).to_string();
                            use_types.push(type_name.clone());
                            Some(type_name)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                fields.push(FieldInfo {
                    attr_snake,
                    builder_setter,
                    enum_type_name,
                });
            }

            let op_desc = sdk_method.replace('_', " ");
            let use_list = use_types.join(", ");

            // Method signature
            code.push_str(&format!(
                "\x20   /// Write {} {} (generated)\n",
                res.name, update_op.operation
            ));
            code.push_str(&format!("\x20   pub(crate) async fn {}(\n", method_name));
            code.push_str("\x20       &self,\n");
            code.push_str("\x20       id: &ResourceId,\n");
            code.push_str("\x20       identifier: &str,\n");
            code.push_str("\x20       attributes: &HashMap<String, Value>,\n");
            code.push_str("\x20   ) -> ProviderResult<()> {\n");
            code.push_str(&format!(
                "\x20       use {}::types::{{{}}};\n",
                sdk_crate_name, use_list
            ));

            // Build nested struct
            code.push_str(&format!(
                "\x20       let mut builder = {}::builder();\n",
                struct_type_name
            ));
            code.push_str("\x20       let mut has_changes = false;\n");

            for field in &fields {
                code.push_str(&format!(
                    "\x20       if let Some(Value::String(val)) = attributes.get(\"{}\") {{\n",
                    field.attr_snake
                ));
                if let Some(ref enum_type) = field.enum_type_name {
                    code.push_str("\x20           let normalized = extract_enum_value(val);\n");
                    code.push_str(&format!(
                        "\x20           builder = builder.{}({}::from(normalized));\n",
                        field.builder_setter, enum_type
                    ));
                } else {
                    code.push_str(&format!(
                        "\x20           builder = builder.{}(val.as_str());\n",
                        field.builder_setter
                    ));
                }
                code.push_str("\x20           has_changes = true;\n");
                code.push_str("\x20       }\n");
            }

            // Send API call
            code.push_str("\x20       if has_changes {\n");
            code.push_str("\x20           let config = builder.build();\n");
            code.push_str(&format!(
                "\x20           self.{}.{}().{}(identifier).{}(config).send().await.map_err(|e| {{\n",
                client_field, sdk_method, id_setter, struct_setter
            ));
            code.push_str(&format!(
                "\x20               ProviderError::new(\"Failed to {}\")\n",
                op_desc
            ));
            code.push_str("\x20                   .with_cause(e)\n");
            code.push_str("\x20                   .for_resource(id.clone())\n");
            code.push_str("\x20           })?;\n");
            code.push_str("\x20       }\n");
            code.push_str("\x20       Ok(())\n");
            code.push_str("\x20   }\n\n");
        }
    }

    // Read attribute extraction methods for resources with read_structure
    for res in all_resources.iter().filter(|r| r.read_structure.is_some()) {
        let model = match models.get(res.service_namespace) {
            Some(m) => m,
            None => continue,
        };
        let ns = res.service_namespace;
        let read_struct_name = res.read_structure.unwrap();
        let read_struct_id = format!("{}#{}", ns, read_struct_name);
        let read_struct = match model.get_structure(&read_struct_id) {
            Some(s) => s,
            None => continue,
        };

        let resource_name = res.name.replace('.', "_");
        let sdk_crate = sdk_crate_name(ns);

        // Build exclude set
        let exclude: HashSet<&str> = res.exclude_fields.iter().copied().collect();

        // Get create input members
        let create_input = if !res.create_op.is_empty() {
            let create_op_id = format!("{}#{}", ns, res.create_op);
            model.operation_input(&create_op_id)
        } else {
            None
        };

        // Compute updatable field names
        let updatable_fields: HashSet<&str> = res
            .update_ops
            .iter()
            .flat_map(|op| op.fields.field_names().iter())
            .copied()
            .collect();

        let extra_read_only: HashSet<&str> = res.extra_read_only.iter().copied().collect();

        // Collect fields to extract: (attr_snake_name, accessor_snake_name, member_ref)
        let mut fields_to_extract: Vec<(String, String, &carina_smithy::ShapeRef)> = Vec::new();

        for (member_name, member_ref) in &read_struct.members {
            if exclude.contains(member_name.as_str()) || member_name == "Tags" {
                continue;
            }

            let is_schema_attr = member_name == res.identifier
                || extra_read_only.contains(member_name.as_str())
                || updatable_fields.contains(member_name.as_str())
                || create_input.is_some_and(|ci| ci.members.contains_key(member_name));

            if !is_schema_attr {
                continue;
            }

            let snake_name = member_name.to_snake_case();
            fields_to_extract.push((snake_name.clone(), snake_name, member_ref));
        }

        // Add extra_writable fields with read_source (different attr name vs accessor)
        for extra in &res.extra_writable {
            if let Some(read_source) = extra.read_source
                && let Some(member_ref) = read_struct.members.get(read_source)
            {
                let attr_name = extra.name.to_snake_case();
                let accessor_name = read_source.to_snake_case();
                // Avoid duplicates (if already extracted under the same accessor)
                if !fields_to_extract.iter().any(|(a, _, _)| a == &attr_name) {
                    fields_to_extract.push((attr_name, accessor_name, member_ref));
                }
            }
        }

        // Sort fields for deterministic output
        fields_to_extract.sort_by(|a, b| a.0.cmp(&b.0));

        // Generate method
        code.push_str(&format!(
            "\x20   /// Extract {} attributes from SDK response type (generated)\n",
            res.name
        ));
        code.push_str(&format!(
            "\x20   pub(crate) fn extract_{}_attributes(\n",
            resource_name
        ));
        code.push_str(&format!(
            "\x20       obj: &{}::types::{},\n",
            sdk_crate, read_struct_name
        ));
        code.push_str("\x20       attributes: &mut HashMap<String, Value>,\n");
        code.push_str("\x20   ) -> Option<String> {\n");

        for (attr_name, accessor_name, member_ref) in &fields_to_extract {
            let kind = model.shape_kind(&member_ref.target);

            match kind {
                Some(ShapeKind::Enum) => {
                    code.push_str(&format!(
                        "\x20       if let Some(v) = obj.{}() {{\n",
                        accessor_name
                    ));
                    code.push_str(&format!(
                        "\x20           attributes.insert(\"{}\".to_string(), Value::String(v.as_str().to_string()));\n",
                        attr_name
                    ));
                    code.push_str("\x20       }\n");
                }
                Some(ShapeKind::Boolean) => {
                    code.push_str(&format!(
                        "\x20       if let Some(v) = obj.{}() {{\n",
                        accessor_name
                    ));
                    code.push_str(&format!(
                        "\x20           attributes.insert(\"{}\".to_string(), Value::Bool(v));\n",
                        attr_name
                    ));
                    code.push_str("\x20       }\n");
                }
                Some(ShapeKind::Integer) | Some(ShapeKind::Long) => {
                    code.push_str(&format!(
                        "\x20       if let Some(v) = obj.{}() {{\n",
                        accessor_name
                    ));
                    code.push_str(&format!(
                        "\x20           attributes.insert(\"{}\".to_string(), Value::Int(v as i64));\n",
                        attr_name
                    ));
                    code.push_str("\x20       }\n");
                }
                Some(ShapeKind::String) => {
                    code.push_str(&format!(
                        "\x20       if let Some(v) = obj.{}() {{\n",
                        accessor_name
                    ));
                    code.push_str(&format!(
                        "\x20           attributes.insert(\"{}\".to_string(), Value::String(v.to_string()));\n",
                        attr_name
                    ));
                    code.push_str("\x20       }\n");
                }
                _ => {
                    // Skip complex types (structures, lists, maps) that need
                    // custom handling in hand-written code
                }
            }
        }

        // Return identifier value (only if identifier exists in read_structure)
        if !res.identifier.is_empty() && read_struct.members.contains_key(res.identifier) {
            let id_snake = res.identifier.to_snake_case();
            code.push_str(&format!(
                "\x20       obj.{}().map(String::from)\n",
                id_snake
            ));
        } else {
            code.push_str("\x20       None\n");
        }

        code.push_str("\x20   }\n\n");
    }

    code.push_str("}\n");

    code
}

/// Derive a short suffix from an operation name by stripping the verb prefix and identifier.
/// e.g., "GetBucketVersioning" with identifier "Bucket" -> "versioning"
fn op_suffix(operation: &str, identifier: &str) -> String {
    let without_verb = operation
        .strip_prefix("Get")
        .or_else(|| operation.strip_prefix("Put"))
        .or_else(|| operation.strip_prefix("Describe"))
        .unwrap_or(operation);
    let without_id = if !identifier.is_empty() {
        without_verb
            .strip_prefix(identifier)
            .unwrap_or(without_verb)
    } else {
        without_verb
    };
    without_id.to_snake_case()
}

/// Get the SDK crate name from a service namespace.
/// e.g., "com.amazonaws.s3" -> "aws_sdk_s3"
fn sdk_crate_name(service_namespace: &str) -> String {
    let service = service_namespace
        .strip_prefix("com.amazonaws.")
        .unwrap_or(service_namespace);
    format!("aws_sdk_{}", service)
}

/// Get the client field name from a service namespace.
/// e.g., "com.amazonaws.ec2" -> "ec2_client", "com.amazonaws.s3" -> "s3_client"
fn client_field_name(service_namespace: &str) -> String {
    let service = service_namespace
        .strip_prefix("com.amazonaws.")
        .unwrap_or(service_namespace);
    format!("{}_client", service)
}

// ── Markdown documentation generation ──

/// Generate markdown documentation for a single resource.
fn generate_markdown_resource(res: &ResourceDef, model: &SmithyModel) -> Result<String> {
    let ns = res.service_namespace;
    let namespace = format!("aws.{}", res.name);

    let is_data_source = res.create_op.is_empty();

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

    // Resolve create input (skip for data sources)
    let create_input = if !is_data_source {
        let create_op_id = format!("{}#{}", ns, res.create_op);
        Some(
            model
                .operation_input(&create_op_id)
                .with_context(|| format!("Cannot find create input for {}", create_op_id))?,
        )
    } else {
        None
    };

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
        for field in update_op.fields.field_names() {
            updatable_fields.insert(field.to_string());
        }
    }

    // Collect writable fields (empty for data sources)
    let mut writable_fields: BTreeMap<String, &carina_smithy::ShapeRef> = BTreeMap::new();
    if let Some(create_input) = &create_input {
        for (name, member_ref) in &create_input.members {
            if exclude.contains(name.as_str()) || name == "Tags" {
                continue;
            }
            writable_fields.insert(name.clone(), member_ref);
        }
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

    // Add extra writable fields from read structure
    for extra in &res.extra_writable {
        if writable_fields.contains_key(extra.name) {
            continue;
        }
        if let Some(source_field) = extra.read_source
            && let Some(read_struct) = read_structure
            && let Some(member_ref) = read_struct.members.get(source_field)
        {
            writable_fields.insert(extra.name.to_string(), member_ref);
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

    // Build extra_writable description override map
    let extra_writable_descs: HashMap<&str, Option<&str>> = res
        .extra_writable
        .iter()
        .map(|e| (e.name, e.description))
        .collect();

    let mut writable_attrs: Vec<MdAttrInfo> = Vec::new();
    for (name, member_ref) in &writable_fields {
        let snake_name = name.to_snake_case();
        let is_required = (SmithyModel::is_required(member_ref)
            || required_overrides.contains(name.as_str()))
            && !read_only_overrides.contains(name.as_str());
        let description = if let Some(Some(desc)) = extra_writable_descs.get(name.as_str()) {
            Some(desc.to_string())
        } else {
            SmithyModel::documentation(&member_ref.traits).map(|s| s.to_string())
        };
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

    // Add synthetic extra writable fields (no read_source) to markdown
    for extra in &res.extra_writable {
        if extra.read_source.is_some() {
            continue;
        }
        let snake_name = extra.name.to_snake_case();
        let type_display = if let Some(&override_type) = type_overrides.get(extra.name) {
            type_code_to_display(override_type)
        } else if is_aws_resource_id_property(extra.name) {
            resource_id_display(extra.name)
        } else {
            "String".to_string()
        };
        writable_attrs.push(MdAttrInfo {
            snake_name,
            type_display,
            is_required: false,
            description: extra.description.map(|s| s.to_string()),
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
        create_input.as_ref().map(|ci| &ci.traits)
    };
    if let Some(traits) = desc_traits
        && let Some(desc) = SmithyModel::documentation(traits)
    {
        let cleaned = collapse_whitespace(&strip_html_tags(desc).replace(['\n', '\t'], " "));
        md.push_str(&format!("{}\n\n", cleaned.trim()));
    }

    // Argument Reference (skip for data sources)
    if !is_data_source {
        md.push_str("## Argument Reference\n\n");
    }

    for attr in &writable_attrs {
        md.push_str(&format!("### `{}`\n\n", attr.snake_name));
        md.push_str(&format!("- **Type:** {}\n", attr.type_display));
        md.push_str(&format!(
            "- **Required:** {}\n",
            if attr.is_required { "Yes" } else { "No" }
        ));
        md.push('\n');

        if let Some(ref desc) = attr.description {
            let cleaned = collapse_whitespace(&strip_html_tags(desc).replace(['\n', '\t'], " "));
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
                        let cleaned =
                            collapse_whitespace(&strip_html_tags(s).replace(['\n', '\t'], " "));
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
            "String".to_string()
        }
        Some(ShapeKind::Boolean) => "Bool".to_string(),
        Some(ShapeKind::Integer) | Some(ShapeKind::Long) => {
            let range = get_int_range(model, target, field_name);
            if let Some(r) = range {
                format!("Int({})", range_display_string(r.min, r.max))
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
        s if s.contains("security_group_rule_id") => "SecurityGroupRuleId".to_string(),
        s if s.contains("security_group_id") => "SecurityGroupId".to_string(),
        s if s.contains("ipam_pool_id") => "IpamPoolId".to_string(),
        s if s.contains("instance_id") => "InstanceId".to_string(),
        s if s.contains("network_interface_id") => "NetworkInterfaceId".to_string(),
        s if s.contains("allocation_id") => "AllocationId".to_string(),
        s if s.contains("prefix_list_id") => "PrefixListId".to_string(),
        s if s.contains("carrier_gateway_id") => "CarrierGatewayId".to_string(),
        s if s.contains("local_gateway_id") => "LocalGatewayId".to_string(),
        s if s.contains("network_acl_id") => "NetworkAclId".to_string(),
        "super::gateway_id()" => "GatewayId".to_string(),
        s if s.contains("arn()") => "Arn".to_string(),
        s if s.contains("aws_account_id") => "AwsAccountId".to_string(),
        s if s.contains("aws_resource_id") => "AwsResourceId".to_string(),
        s if s.contains("availability_zone_id") => "AvailabilityZoneId".to_string(),
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
        ResourceIdKind::InstanceId => "InstanceId".to_string(),
        ResourceIdKind::NetworkInterfaceId => "NetworkInterfaceId".to_string(),
        ResourceIdKind::AllocationId => "AllocationId".to_string(),
        ResourceIdKind::PrefixListId => "PrefixListId".to_string(),
        ResourceIdKind::CarrierGatewayId => "CarrierGatewayId".to_string(),
        ResourceIdKind::LocalGatewayId => "LocalGatewayId".to_string(),
        ResourceIdKind::NetworkAclId => "NetworkAclId".to_string(),
        ResourceIdKind::Generic => "AwsResourceId".to_string(),
    }
}

// ── Type inference helpers (ported from codegen.rs) ──

fn known_string_type_overrides() -> &'static HashMap<&'static str, &'static str> {
    static OVERRIDES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("DefaultSecurityGroup", "super::security_group_id()");
        m.insert("DefaultNetworkAcl", "super::network_acl_id()");
        m.insert("AccountId", "super::aws_account_id()");
        m.insert("GatewayId", "super::gateway_id()");
        m.insert("DeliverCrossAccountRole", "super::iam_role_arn()");
        m.insert("DeliverLogsPermissionArn", "super::iam_role_arn()");
        m.insert("PeerRoleArn", "super::iam_role_arn()");
        m.insert("PermissionsBoundary", "super::iam_policy_arn()");
        m.insert("ManagedPolicyArns", "super::iam_policy_arn()");
        m.insert("KmsKeyId", "super::kms_key_arn()");
        m.insert("KMSMasterKeyID", "super::kms_key_id()");
        m.insert("ReplicaKmsKeyID", "super::kms_key_id()");
        m.insert("KmsKeyArn", "super::kms_key_arn()");
        m.insert("SecurityGroupRuleId", "super::security_group_rule_id()");
        m.insert("Locale", "super::aws_region()");
        m.insert("BucketAccountId", "super::aws_account_id()");
        m.insert("PublicIp", "types::ipv4_address()");
        m.insert("LogDestination", "super::arn()");
        m.insert("GrantFullControl", "super::s3_grantee()");
        m.insert("GrantRead", "super::s3_grantee()");
        m.insert("GrantReadACP", "super::s3_grantee()");
        m.insert("GrantWrite", "super::s3_grantee()");
        m.insert("GrantWriteACP", "super::s3_grantee()");
        m
    });
    &OVERRIDES
}

fn known_enum_overrides() -> &'static HashMap<&'static str, Vec<&'static str>> {
    static OVERRIDES: LazyLock<HashMap<&'static str, Vec<&'static str>>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert(
            "IpProtocol",
            vec!["tcp", "udp", "icmp", "icmpv6", "-1", "all"],
        );
        m.insert("HostnameType", vec!["ip-name", "resource-name"]);
        m
    });
    &OVERRIDES
}

/// Generate condition string and display string for integer range validation.
fn int_range_condition_and_display(min: Option<i64>, max: Option<i64>) -> (String, String) {
    match (min, max) {
        (Some(min), Some(max)) => (
            format!("*n < {} || *n > {}", min, max),
            format!("{}..={}", min, max),
        ),
        (Some(min), None) => (format!("*n < {}", min), format!("{}..", min)),
        (None, Some(max)) => (format!("*n > {}", max), format!("..={}", max)),
        (None, None) => unreachable!("at least one bound must be present"),
    }
}

/// Format a range display string for type names.
fn range_display_string(min: Option<i64>, max: Option<i64>) -> String {
    match (min, max) {
        (Some(min), Some(max)) => format!("{}..={}", min, max),
        (Some(min), None) => format!("{}..", min),
        (None, Some(max)) => format!("..={}", max),
        (None, None) => unreachable!("at least one bound must be present"),
    }
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

/// Unified resource-specific property type overrides.
/// Maps (Smithy resource name, property name) to a TypeOverride.
/// Use this when a property needs resource-specific type treatment that differs
/// from global overrides or pattern-based inference.
#[allow(dead_code)]
fn resource_type_overrides() -> &'static HashMap<(&'static str, &'static str), TypeOverride> {
    static OVERRIDES: LazyLock<HashMap<(&'static str, &'static str), TypeOverride>> =
        LazyLock::new(HashMap::new);
    &OVERRIDES
}

fn infer_string_type(prop_name: &str) -> Option<String> {
    // Check known string type overrides
    if let Some(&override_type) = known_string_type_overrides().get(prop_name) {
        return Some(override_type.to_string());
    }

    // Normalize plural forms for type inference
    let singular_name = if prop_name.ends_with("Ids")
        || prop_name.ends_with("ids")
        || prop_name.ends_with("Arns")
        || prop_name.ends_with("arns")
    {
        &prop_name[..prop_name.len() - 1]
    } else {
        prop_name
    };

    // Check overrides for singular form too (e.g., list items)
    if let Some(&override_type) = known_string_type_overrides().get(singular_name) {
        return Some(override_type.to_string());
    }

    let prop_lower = singular_name.to_lowercase();

    // CIDR types - differentiate IPv4 vs IPv6 based on property name
    if prop_lower.contains("cidr") {
        if prop_lower.contains("ipv6") {
            return Some("types::ipv6_cidr()".to_string());
        }
        return Some("types::ipv4_cidr()".to_string());
    }

    // IP address types (not CIDR) - e.g., PrivateIpAddress, PublicIp
    if (prop_lower.contains("ipaddress")
        || prop_lower.ends_with("ip")
        || prop_lower.contains("ipaddresses"))
        && !prop_lower.contains("count")
        && !prop_lower.contains("type")
    {
        if prop_lower.contains("ipv6") {
            return Some("types::ipv6_address()".to_string());
        }
        return Some("types::ipv4_address()".to_string());
    }

    // Availability zone (but not AvailabilityZoneId which uses AZ ID format like "use1-az1")
    if prop_lower == "availabilityzone" || prop_lower == "availabilityzones" {
        return Some("super::availability_zone()".to_string());
    }

    // Availability zone ID (e.g., "use1-az1", "usw2-az2")
    if prop_lower == "availabilityzoneid" || prop_lower == "availabilityzoneids" {
        return Some("super::availability_zone_id()".to_string());
    }

    // Region types (e.g., PeerRegion, ServiceRegion, RegionName, ResourceRegion)
    if prop_lower.ends_with("region") || prop_lower == "regionname" {
        return Some("super::aws_region()".to_string());
    }

    // Check ARN pattern
    if prop_lower.ends_with("arn") || prop_lower.ends_with("arns") || prop_lower.contains("_arn") {
        return Some("super::arn()".to_string());
    }

    // IPAM Pool IDs
    if is_ipam_pool_id_property(singular_name) {
        return Some("super::ipam_pool_id()".to_string());
    }

    // Check resource ID pattern
    if is_aws_resource_id_property(singular_name) {
        return Some(get_resource_id_type(singular_name).to_string());
    }

    // AWS Account ID (owner IDs and account IDs are 12-digit account IDs)
    if prop_lower.ends_with("ownerid") || prop_lower.ends_with("accountid") {
        return Some("super::aws_account_id()".to_string());
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
    InstanceId,
    NetworkInterfaceId,
    AllocationId,
    PrefixListId,
    CarrierGatewayId,
    LocalGatewayId,
    NetworkAclId,
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
    // Instance IDs (e.g., InstanceId)
    if lower.ends_with("instanceid") {
        return ResourceIdKind::InstanceId;
    }
    // Network Interface IDs (e.g., NetworkInterfaceId, EniId)
    if lower.ends_with("networkinterfaceid") || lower.ends_with("eniid") {
        return ResourceIdKind::NetworkInterfaceId;
    }
    // Allocation IDs (e.g., AllocationId)
    if lower.ends_with("allocationid") {
        return ResourceIdKind::AllocationId;
    }
    // Prefix List IDs (e.g., PrefixListId, DestinationPrefixListId)
    if lower.ends_with("prefixlistid") {
        return ResourceIdKind::PrefixListId;
    }
    // Carrier Gateway IDs (e.g., CarrierGatewayId)
    if lower.contains("carriergateway") && lower.ends_with("id") {
        return ResourceIdKind::CarrierGatewayId;
    }
    // Local Gateway IDs (e.g., LocalGatewayId)
    if lower.contains("localgateway") && lower.ends_with("id") {
        return ResourceIdKind::LocalGatewayId;
    }
    // Network ACL IDs (e.g., NetworkAclId)
    if lower.contains("networkacl") && lower.ends_with("id") {
        return ResourceIdKind::NetworkAclId;
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
        ResourceIdKind::InstanceId => "super::instance_id()",
        ResourceIdKind::NetworkInterfaceId => "super::network_interface_id()",
        ResourceIdKind::AllocationId => "super::allocation_id()",
        ResourceIdKind::PrefixListId => "super::prefix_list_id()",
        ResourceIdKind::CarrierGatewayId => "super::carrier_gateway_id()",
        ResourceIdKind::LocalGatewayId => "super::local_gateway_id()",
        ResourceIdKind::NetworkAclId => "super::network_acl_id()",
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
        "sts.caller_identity" => "AWS::STS::CallerIdentity",
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

fn collapse_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
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
    result
}

fn escape_description(desc: &str) -> String {
    let stripped = strip_html_tags(desc);
    let normalized = stripped.replace('"', "\\\"").replace(['\n', '\t'], " ");
    collapse_whitespace(&normalized).trim().to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_resource_uses_string_enum_for_namespaced_enums() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../carina-provider-aws/tests/fixtures/smithy/s3.json");
        if !fixture.exists() {
            eprintln!(
                "Skipping: Smithy fixture not found: {}\nRun scripts/download-smithy-models.sh to enable this test",
                fixture.display()
            );
            return;
        }
        let file = std::fs::File::open(&fixture).expect("failed to open Smithy fixture");
        let model = carina_smithy::parse_reader(std::io::BufReader::new(file))
            .expect("failed to parse Smithy fixture");
        let resource = resource_defs::s3_resources()
            .into_iter()
            .find(|res| res.name == "s3.bucket")
            .expect("missing s3.bucket resource def");

        let generated = generate_resource(&resource, &model).expect("failed to generate resource");

        assert!(
            generated.contains("AttributeType::StringEnum {"),
            "enum-like strings should be emitted as StringEnum: {generated}"
        );
        assert!(
            !generated.contains(".with_completions("),
            "enum completions should come from schema type metadata: {generated}"
        );
    }
}
