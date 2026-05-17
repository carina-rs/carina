//! Effect - Representing side effects as values
//!
//! An Effect describes "what to do" without actually performing the side effect.
//! Side effects only occur when they are executed via a Provider.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::resource::{Directives, Resource, ResourceId, State};
use crate::wait::predicate::WaitPredicate;

/// Temporary name used during create-before-destroy replacement.
///
/// When a resource with a unique name constraint is replaced with create-before-destroy,
/// the new resource is created with a temporary name to avoid conflicts with the old resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporaryName {
    /// The attribute that holds the name (e.g., "bucket_name")
    pub attribute: String,
    /// The original (desired) name value (e.g., "my-bucket")
    pub original_value: String,
    /// The generated temporary name (e.g., "my-bucket-a1b2c3d4")
    pub temporary_value: String,
    /// Whether the name attribute can be updated after creation (not create-only)
    pub can_rename: bool,
}

/// A dependent resource that must be updated during a create_before_destroy replacement.
///
/// When a resource is replaced with create_before_destroy, dependent resources that
/// reference the replaced resource's computed attributes need to be updated between
/// the create (new) and delete (old) steps. The `to` field retains unresolved
/// `ResourceRef` values so that the apply phase can re-resolve them using the
/// newly created resource's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CascadingUpdate {
    pub id: ResourceId,
    pub from: Box<State>,
    pub to: Resource,
}

/// How an [`Effect::Wait`] obtains the cloud provider identifier its
/// polling `read()` needs.
///
/// The differ cannot always know the target's identifier at plan time:
/// when the target is *created in the same apply run* it has no prior
/// state, so its real identifier (e.g. an ACM certificate ARN) only
/// exists after the `Create` effect completes. Splitting "known now"
/// from "resolve at apply" into the type — instead of overloading
/// `Option<String>`'s `None` — forces the executor to handle the
/// apply-time case explicitly via an exhaustive `match`, so future code
/// cannot silently pass a stale plan-time `None` to `provider.read`
/// (carina#3119).
///
/// Guard: the old `Option<String>`-shaped pattern — treating the
/// plan-time value as something you can `.as_deref()` straight into the
/// poll loop — no longer type-checks. `WaitTarget` has no `Option`-like
/// API, so there is no `None` to forward:
///
/// ```compile_fail
/// use carina_core::effect::WaitTarget;
/// let t = WaitTarget::ResolvedAtApply;
/// // Was: `target_identifier.as_deref()` — `WaitTarget` has no
/// // `as_deref`, forcing callers through an exhaustive match that
/// // handles the apply-time resolution explicitly.
/// let _ = t.as_deref();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitTarget {
    /// The target already exists; its identifier was resolved from
    /// `current_states` at plan time.
    Known(String),
    /// The target is created or updated in this same run. The executor
    /// resolves the real identifier from the just-applied state
    /// (`applied_states`) before polling; falls back to no identifier
    /// only when the target was never produced in this plan.
    ResolvedAtApply,
}

/// Effect representing an operation on a resource
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// Read the current state of a resource (data source)
    Read { resource: Resource },

    /// Create a new resource
    Create(Resource),

    /// Update an existing resource
    Update {
        id: ResourceId,
        from: Box<State>,
        to: Resource,
        /// Attribute names that changed (including removed attributes)
        changed_attributes: Vec<String>,
    },

    /// Replace a resource (delete then create) due to create-only property changes
    Replace {
        id: ResourceId,
        from: Box<State>,
        to: Resource,
        #[serde(default)]
        directives: Directives,
        /// Which create-only attributes forced the replacement
        changed_create_only: Vec<String>,
        /// Dependent resources to update between create and delete (create_before_destroy only)
        #[serde(default)]
        cascading_updates: Vec<CascadingUpdate>,
        /// Temporary name for create-before-destroy when the resource has a unique name constraint
        #[serde(default)]
        temporary_name: Option<TemporaryName>,
        /// Hints mapping attribute names to their original ResourceRef expressions
        /// (e.g., `("vpc_id", "vpc.vpc_id")`). Used by display to show the binding
        /// reference instead of the resolved value for cascade-triggered replacements.
        #[serde(default)]
        cascade_ref_hints: Vec<(String, String)>,
    },

    /// Delete a resource
    Delete {
        id: ResourceId,
        identifier: String,
        #[serde(default)]
        directives: Directives,
        /// The binding name of the deleted resource (for plan tree display)
        #[serde(default)]
        binding: Option<String>,
        /// Binding names this resource depended on (for plan tree display).
        /// Includes both value-reference and explicit `directives.depends_on`
        /// edges — the union the executor needs for ordering.
        #[serde(default)]
        dependencies: HashSet<String>,
        /// Subset of `dependencies` that came from
        /// `directives { depends_on = [...] }` rather than value
        /// references. Captured at Delete construction time because the
        /// originating Resource is gone by the time the executor runs
        /// (#2871). Empty for legacy state files.
        #[serde(default)]
        explicit_dependencies: HashSet<String>,
    },

    /// Import an existing resource into state (via provider read)
    Import {
        /// Target resource address
        id: ResourceId,
        /// Cloud provider identifier (e.g., "vpc-0abc123def456")
        identifier: String,
    },

    /// Remove a resource from state without destroying it
    Remove {
        /// Resource address to remove from state
        id: ResourceId,
    },

    /// Move/rename a resource in state without destroy/recreate
    Move {
        /// Old resource address
        from: ResourceId,
        /// New resource address
        to: ResourceId,
    },

    /// Wait for `target` to satisfy `until` by polling `read()`.
    ///
    /// Emitted by the differ for a `let <binding> = wait <target> { ... }`
    /// declaration. The executor (not the provider) drives the polling
    /// loop on top of the existing `Provider::read` trait method, so
    /// providers — including WASM plugins — need no contract change.
    ///
    /// Wait effects do **not** persist to `carina.state.json`; they are
    /// re-evaluated on every plan/apply. See
    /// `notes/specs/2026-05-09-wait-construct-design.md` §State file.
    Wait {
        /// The wait's binding name (e.g. `"cert_issued"`).
        binding: String,
        /// Resolved id of the target resource (`wait cert { ... }` →
        /// `cert`'s `ResourceId`).
        target_id: ResourceId,
        /// How the executor obtains the target's cloud provider
        /// identifier for its polling `read()`. See [`WaitTarget`].
        target: WaitTarget,
        /// Typed predicate evaluated against each `read()` snapshot.
        until: WaitPredicate,
        /// Surface form of the `until` expression as the user wrote it
        /// (e.g. `"cert.status == aws.acm.Certificate.Status.Issued"`).
        /// Carried so plan-display never re-stringifies the parsed AST —
        /// same pattern as `Effect::Replace::cascade_ref_hints`.
        until_surface: String,
        /// Hard cap on total wait time. Resolved by the differ from the
        /// user-provided override or the target schema's default.
        #[serde(with = "crate::resource::duration_secs")]
        timeout: std::time::Duration,
        /// Poll cadence between `read()` calls. Resolved from the target
        /// schema's default; not user-visible in MVP.
        #[serde(with = "crate::resource::duration_secs")]
        interval: std::time::Duration,
        /// Additional bindings the wait must wait for before polling,
        /// declared via `depends_on = [...]` in the wait block. The
        /// scheduler treats these like `directives.depends_on` on a
        /// resource — extra ordering edges that aren't expressed via
        /// value references.
        #[serde(default)]
        explicit_dependencies: HashSet<String>,
    },
}

impl Effect {
    /// Returns the kind of Effect as a string (for display)
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::Read { .. } => "read",
            Effect::Create(_) => "create",
            Effect::Update { .. } => "update",
            Effect::Replace { .. } => "replace",
            Effect::Delete { .. } => "delete",
            Effect::Import { .. } => "import",
            Effect::Remove { .. } => "remove",
            Effect::Move { .. } => "move",
            Effect::Wait { .. } => "wait",
        }
    }

    /// Returns whether this Effect causes a mutation
    pub fn is_mutating(&self) -> bool {
        !matches!(self, Effect::Read { .. } | Effect::Wait { .. })
    }

    /// Returns whether this is a state-only operation (import/remove/move)
    pub fn is_state_operation(&self) -> bool {
        matches!(
            self,
            Effect::Import { .. } | Effect::Remove { .. } | Effect::Move { .. }
        )
    }

    /// Returns the resource ID for this effect
    pub fn resource_id(&self) -> &ResourceId {
        match self {
            Effect::Read { resource } => &resource.id,
            Effect::Create(r) => &r.id,
            Effect::Update { id, .. } => id,
            Effect::Replace { id, .. } => id,
            Effect::Delete { id, .. } => id,
            Effect::Import { id, .. } => id,
            Effect::Remove { id, .. } => id,
            Effect::Move { to, .. } => to,
            Effect::Wait { target_id, .. } => target_id,
        }
    }

    /// Returns a reference to the resource for this effect, if it has one.
    /// Delete, Import, Remove, Move, and Wait effects have no resource.
    pub fn resource(&self) -> Option<&Resource> {
        match self {
            Effect::Create(resource) => Some(resource),
            Effect::Update { to, .. } => Some(to),
            Effect::Replace { to, .. } => Some(to),
            Effect::Read { resource } => Some(resource),
            Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::Wait { .. } => None,
        }
    }

    /// Returns the binding name for this effect's resource, if it has one.
    pub fn binding_name(&self) -> Option<String> {
        if let Effect::Delete { binding, .. } = self {
            return binding.clone();
        }
        if let Effect::Wait { binding, .. } = self {
            return Some(binding.clone());
        }
        self.resource().and_then(|r| r.binding.clone())
    }

    /// Returns the binding names this effect depends on **via explicit
    /// `directives { depends_on = [...] }` declarations**, as a snapshot
    /// (cloned).
    ///
    /// For variants carrying a `Resource` (Create, Update, Replace,
    /// Read), the answer is derived live from
    /// `resource.directives.depends_on`. For Delete the answer comes
    /// from a stored `explicit_dependencies` set captured by the differ
    /// at construction time, because the originating Resource is gone
    /// by the time the executor runs (#2871).
    ///
    /// State-only effects (Import, Remove, Move) return an empty set —
    /// they are scheduling primitives, not resource-state operations.
    pub fn explicit_dependencies(&self) -> HashSet<String> {
        if let Some(res) = self.resource() {
            return res.directives.depends_on.iter().cloned().collect();
        }
        if let Effect::Delete {
            explicit_dependencies,
            ..
        } = self
        {
            return explicit_dependencies.clone();
        }
        if let Effect::Wait {
            explicit_dependencies,
            ..
        } = self
        {
            return explicit_dependencies.clone();
        }
        HashSet::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_is_not_mutating() {
        let resource = Resource::new("test", "example").with_read_only(true);
        let effect = Effect::Read { resource };
        assert!(!effect.is_mutating());
    }

    #[test]
    fn create_is_mutating() {
        let resource = Resource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Create(resource);
        assert!(effect.is_mutating());
    }

    #[test]
    fn resource_id_returns_correct_id() {
        let resource = Resource::new("s3.Bucket", "my-bucket").with_read_only(true);
        let effect = Effect::Read {
            resource: resource.clone(),
        };
        assert_eq!(effect.resource_id(), &resource.id);
    }

    #[test]
    fn resource_returns_some_for_create() {
        let resource = Resource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Create(resource.clone());
        assert_eq!(effect.resource().unwrap().id, resource.id);
    }

    #[test]
    fn resource_returns_none_for_delete() {
        let effect = Effect::Delete {
            id: ResourceId::new("test", "a"),
            identifier: "id-123".to_string(),
            directives: Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        assert!(effect.resource().is_none());
    }

    #[test]
    fn binding_name_returns_binding() {
        let resource = Resource::new("test", "my_binding").with_binding("my_binding");
        let effect = Effect::Create(resource);
        assert_eq!(effect.binding_name(), Some("my_binding".to_string()));
    }

    #[test]
    fn binding_name_returns_none_without_binding() {
        use crate::resource::{ConcreteValue, Value};
        let resource = Resource::new("test", "no_binding").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("test".to_string())),
        );
        let effect = Effect::Create(resource);
        assert_eq!(effect.binding_name(), None);
    }

    #[test]
    fn effect_serde_round_trip() {
        use crate::resource::{ConcreteValue, Value};
        use std::collections::HashMap;

        let effects = vec![
            Effect::Create(Resource::new("s3.Bucket", "my-bucket")),
            Effect::Read {
                resource: Resource::new("s3.Bucket", "existing").with_read_only(true),
            },
            Effect::Update {
                id: ResourceId::new("s3.Bucket", "my-bucket"),
                from: Box::new(State::existing(
                    ResourceId::new("s3.Bucket", "my-bucket"),
                    HashMap::from([(
                        "versioning".to_string(),
                        Value::Concrete(ConcreteValue::String("Disabled".to_string())),
                    )]),
                )),
                to: Resource::new("s3.Bucket", "my-bucket").with_attribute(
                    "versioning",
                    Value::Concrete(ConcreteValue::String("Enabled".to_string())),
                ),
                changed_attributes: vec!["versioning".to_string()],
            },
            Effect::Replace {
                id: ResourceId::new("ec2.Vpc", "my-vpc"),
                from: Box::new(State::existing(
                    ResourceId::new("ec2.Vpc", "my-vpc"),
                    HashMap::from([(
                        "cidr_block".to_string(),
                        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
                    )]),
                )),
                to: Resource::new("ec2.Vpc", "my-vpc").with_attribute(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
                ),
                directives: Directives::default(),
                changed_create_only: vec!["cidr_block".to_string()],
                cascading_updates: vec![],
                temporary_name: None,
                cascade_ref_hints: vec![],
            },
            Effect::Delete {
                id: ResourceId::new("s3.Bucket", "old-bucket"),
                identifier: "old-bucket".to_string(),
                directives: Directives::default(),
                binding: None,
                dependencies: HashSet::new(),
                explicit_dependencies: std::collections::HashSet::new(),
            },
        ];

        for effect in effects {
            let json = serde_json::to_string(&effect).unwrap();
            let deserialized: Effect = serde_json::from_str(&json).unwrap();
            assert_eq!(effect, deserialized, "Round-trip failed for {:?}", effect);
        }
    }

    #[test]
    fn explicit_dependencies_derived_from_resource_directives() {
        use crate::resource::Directives;
        let mut bucket = Resource::new("s3.Bucket", "b");
        bucket.directives = Directives {
            depends_on: vec!["role".to_string(), "kms".to_string()],
            ..Directives::default()
        };
        let create = Effect::Create(bucket.clone());
        let got = create.explicit_dependencies();
        assert!(got.contains("role") && got.contains("kms"), "got {:?}", got);
    }

    #[test]
    fn explicit_dependencies_for_delete_uses_stored_set() {
        let effect = Effect::Delete {
            id: ResourceId::new("s3.Bucket", "b"),
            identifier: "x".to_string(),
            directives: Directives::default(),
            binding: Some("bucket".to_string()),
            dependencies: HashSet::from(["role".to_string(), "kms".to_string()]),
            explicit_dependencies: HashSet::from(["role".to_string()]),
        };
        let got = effect.explicit_dependencies();
        assert!(got.contains("role"));
        assert!(!got.contains("kms"), "kms is value-ref-only; got {:?}", got);
    }

    #[test]
    fn explicit_dependencies_empty_for_state_only_effects() {
        let imp = Effect::Import {
            id: ResourceId::new("s3.Bucket", "b"),
            identifier: "x".to_string(),
        };
        let rem = Effect::Remove {
            id: ResourceId::new("s3.Bucket", "b"),
        };
        let mov = Effect::Move {
            from: ResourceId::new("s3.Bucket", "old"),
            to: ResourceId::new("s3.Bucket", "new"),
        };
        for e in [imp, rem, mov] {
            assert!(
                e.explicit_dependencies().is_empty(),
                "expected empty for state-only, got {:?}",
                e.explicit_dependencies()
            );
        }
    }

    #[test]
    fn delete_legacy_state_without_explicit_deps_deserialises_to_empty() {
        // Pre-#2871 state files have no `explicit_dependencies` field.
        // `#[serde(default)]` must populate it as an empty HashSet so
        // round-tripping legacy state never fails.
        let legacy = serde_json::json!({
            "Delete": {
                "id": {"provider": "aws", "resource_type": "s3.Bucket", "name": "b"},
                "identifier": "x",
                "directives": {},
                "binding": null,
                "dependencies": ["role"],
            }
        });
        let effect: Effect = serde_json::from_value(legacy).unwrap();
        if let Effect::Delete {
            explicit_dependencies,
            ..
        } = &effect
        {
            assert!(explicit_dependencies.is_empty());
        } else {
            panic!("expected Delete, got {:?}", effect);
        }
    }

    #[test]
    fn wait_variant_constructs() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let _ = Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
            target: WaitTarget::ResolvedAtApply,
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(75 * 60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
    }

    #[test]
    fn wait_binding_name_returns_wait_binding() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let e = Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
            target: WaitTarget::ResolvedAtApply,
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        assert_eq!(e.binding_name(), Some("cert_issued".to_string()));
    }

    #[test]
    fn wait_is_not_mutating() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let e = Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
            target: WaitTarget::ResolvedAtApply,
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        assert!(!e.is_mutating());
        assert_eq!(e.kind(), "wait");
    }

    #[test]
    fn wait_serde_round_trip() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let original = Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
            target: WaitTarget::ResolvedAtApply,
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(4500),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        // Duration must round-trip as plain integer seconds (matches the
        // project's "no { secs, nanos }" rule from #2824).
        assert!(
            json.contains("\"timeout\":4500"),
            "expected `\"timeout\":4500` in JSON, got: {}",
            json
        );
        let decoded: Effect = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, original);
    }
}
