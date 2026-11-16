//! Layout-only copies used by the resolver's test output kind. No evaluation.
use super::*;

type ReferenceCopy<'a, P, Q, B> =
    dyn FnMut(&<P as ValueShape>::ReferencePayload<B>) -> Value<Q, B> + 'a;
type ConversionCopy<'a, P, Q, B> =
    dyn FnMut(&<P as ValueShape>::ConversionPayload<B>) -> Value<Q, B> + 'a;

pub(crate) fn copy_storage<P: ValueShape, Q: ValueShape, B: BindingAddress>(
    value: &Value<P, B>,
    leaf: &mut dyn FnMut(&P) -> Q,
    reference: &mut ReferenceCopy<'_, P, Q, B>,
    conversion: &mut ConversionCopy<'_, P, Q, B>,
) -> Value<Q, B> {
    Copier {
        leaf,
        reference,
        conversion,
    }
    .value(value)
}

struct Copier<'a, P: ValueShape, Q: ValueShape, B: BindingAddress> {
    leaf: &'a mut dyn FnMut(&P) -> Q,
    reference: &'a mut ReferenceCopy<'a, P, Q, B>,
    conversion: &'a mut ConversionCopy<'a, P, Q, B>,
}

impl<P: ValueShape, Q: ValueShape, B: BindingAddress> Copier<'_, P, Q, B> {
    fn argument(&mut self, argument: &FunctionArgument<P, B>) -> FunctionArgument<Q, B> {
        let FunctionArgument { operand } = argument;
        let operand = match operand {
            StructuralOperand::Value { value, access_base } => StructuralOperand::Value {
                value: self.value(value),
                access_base: access_base.as_ref().map(|base| self.value(base)),
            },
            StructuralOperand::Handle {
                handle,
                access_base,
            } => StructuralOperand::Handle {
                handle: handle.clone(),
                access_base: self.value(access_base),
            },
            StructuralOperand::Parameter { name, access_base } => StructuralOperand::Parameter {
                name: name.clone(),
                access_base: self.value(access_base),
            },
        };
        FunctionArgument { operand }
    }

    fn value(&mut self, value: &Value<P, B>) -> Value<Q, B> {
        crate::parser::count_resolver_event(crate::parser::ResolverEvent::CopyValue);
        if matches!(
            value,
            Value::Concrete(ConcreteValue::List(_) | ConcreteValue::Map(_))
        ) {
            crate::parser::count_resolver_event(crate::parser::ResolverEvent::CopyContainer);
        }
        match value {
            Value::Reference(payload) => (self.reference)(payload),
            Value::Conversion(payload) => (self.conversion)(payload),
            Value::Concrete(value) => Value::Concrete(match value {
                ConcreteValue::String(value) => ConcreteValue::String(value.clone()),
                ConcreteValue::EnumIdentifier(value) => {
                    ConcreteValue::EnumIdentifier(value.clone())
                }
                ConcreteValue::CanonicalEnum(value) => ConcreteValue::CanonicalEnum(value.clone()),
                ConcreteValue::Int(value) => ConcreteValue::Int(*value),
                ConcreteValue::Float(value) => ConcreteValue::Float(*value),
                ConcreteValue::Bool(value) => ConcreteValue::Bool(*value),
                ConcreteValue::Duration(value) => ConcreteValue::Duration(*value),
                ConcreteValue::StringList(value) => ConcreteValue::StringList(value.clone()),
                ConcreteValue::List(values) => {
                    ConcreteValue::List(values.iter().map(|v| self.value(v)).collect())
                }
                ConcreteValue::Map(values) => ConcreteValue::Map(
                    values
                        .iter()
                        .map(|(k, v)| (k.clone(), self.value(v)))
                        .collect(),
                ),
            }),
            Value::Deferred(value) => Value::Deferred(match value {
                DeferredValue::ModuleIf(deferred) => {
                    let crate::parser::DeferredValueIf {
                        condition,
                        then_value,
                        else_value,
                    } = deferred.as_ref();
                    let crate::parser::DeferredIfCondition {
                        value,
                        source_file,
                        source_line,
                        source_expression,
                        node_id,
                    } = condition;
                    DeferredValue::ModuleIf(Box::new(crate::parser::DeferredValueIf {
                        condition: crate::parser::DeferredIfCondition {
                            value: self.value(value),
                            source_file: source_file.clone(),
                            source_line: *source_line,
                            source_expression: source_expression.clone(),
                            node_id: *node_id,
                        },
                        then_value: self.value(then_value),
                        else_value: self.value(else_value),
                    }))
                }
                DeferredValue::ResourceRef { path } => {
                    DeferredValue::ResourceRef { path: path.clone() }
                }
                DeferredValue::BindingRef { binding } => DeferredValue::BindingRef {
                    binding: binding.clone(),
                },
                DeferredValue::Coalesce { value, default } => DeferredValue::Coalesce {
                    value: Box::new(self.value(value)),
                    default: Box::new(self.value(default)),
                },
                DeferredValue::IntegerArithmetic(expr) => {
                    DeferredValue::IntegerArithmetic(expr.copy_storage(&mut |v| self.value(v)))
                }
                DeferredValue::Interpolation(parts) => DeferredValue::Interpolation(
                    parts
                        .iter()
                        .map(|part| match part {
                            InterpolationPart::Literal(text) => {
                                InterpolationPart::Literal(text.clone())
                            }
                            InterpolationPart::Expr(value) => {
                                InterpolationPart::Expr(self.value(value))
                            }
                        })
                        .collect(),
                ),
                DeferredValue::FunctionCall {
                    name,
                    args,
                    source_line,
                } => DeferredValue::FunctionCall {
                    name: *name,
                    args: args.iter().map(|a| self.argument(a)).collect(),
                    source_line: *source_line,
                },
                DeferredValue::Unavailable {
                    name,
                    args,
                    reason,
                    source_line,
                } => DeferredValue::Unavailable {
                    name: *name,
                    args: args.iter().map(|a| self.argument(a)).collect(),
                    reason: reason.clone(),
                    source_line: *source_line,
                },
                DeferredValue::ResolvedFunctionCall {
                    function,
                    functions,
                    args,
                    source_line,
                } => DeferredValue::ResolvedFunctionCall {
                    function: function.clone(),
                    functions: functions.clone(),
                    args: args.iter().map(|a| self.argument(a)).collect(),
                    source_line: *source_line,
                },
                DeferredValue::Access(access) => {
                    let AccessExpr {
                        base,
                        step,
                        source_line,
                        display_root,
                        reference,
                    } = access;
                    DeferredValue::Access(AccessExpr {
                        base: Box::new(self.value(base)),
                        step: step.clone(),
                        source_line: *source_line,
                        display_root: display_root.clone(),
                        reference: reference.clone(),
                    })
                }
                DeferredValue::Secret(payload) => {
                    DeferredValue::Secret(payload.copy_storage(&mut |v| self.value(v)))
                }
                DeferredValue::Unknown(reason) => DeferredValue::Unknown(reason.clone()),
                DeferredValue::Placeholder(placeholder) => {
                    DeferredValue::Placeholder((self.leaf)(placeholder))
                }
            }),
        }
    }
}

/// Representation equality, including metadata intentionally ignored by semantic Eq.
/// Never formats values: retained secret operands may contain unmarked plaintext.
pub(crate) fn same_plain_representation(a: &AuthoredValue, b: &AuthoredValue) -> bool {
    same_value(a, b)
}

fn same_slice<T>(a: &[T], b: &[T], eq: impl Fn(&T, &T) -> bool) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| eq(a, b))
}
fn same_option<T>(a: &Option<T>, b: &Option<T>, eq: impl Fn(&T, &T) -> bool) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => eq(a, b),
        (None, None) => true,
        _ => false,
    }
}
fn same_text(a: &ValueString, b: &ValueString) -> bool {
    let ValueString {
        plaintext: a,
        diagnostic_sensitive: am,
    } = a;
    let ValueString {
        plaintext: b,
        diagnostic_sensitive: bm,
    } = b;
    a == b && am == bm
}
fn same_span(a: &DiagnosticSourceSpan, b: &DiagnosticSourceSpan) -> bool {
    let DiagnosticSourceSpan(a) = a;
    let DiagnosticSourceSpan(b) = b;
    match (a, b) {
        (DiagnosticSourceSpanState::Deserialized, DiagnosticSourceSpanState::Deserialized) => true,
        (DiagnosticSourceSpanState::Authored(a), DiagnosticSourceSpanState::Authored(b)) => {
            let AuthoredDiagnosticSourceSpan {
                location: a,
                source_file: af,
                range: ar,
            } = a;
            let AuthoredDiagnosticSourceSpan {
                location: b,
                source_file: bf,
                range: br,
            } = b;
            a == b && af == bf && ar == br
        }
        _ => false,
    }
}
fn same_placeholder(a: &ParserPlaceholder, b: &ParserPlaceholder) -> bool {
    match a {
        ParserPlaceholder::FnParam { name: a } => {
            matches!(b,ParserPlaceholder::FnParam {name:b} if a==b)
        }
        ParserPlaceholder::FnLocal { name: a } => {
            matches!(b,ParserPlaceholder::FnLocal {name:b} if a==b)
        }
        ParserPlaceholder::Loop(a) => matches!(b,ParserPlaceholder::Loop(b) if a==b),
        ParserPlaceholder::EmptyInterpolation { source: a } => {
            matches!(b,ParserPlaceholder::EmptyInterpolation {source:b} if same_span(a,b))
        }
        ParserPlaceholder::ModuleArgument {
            name: a,
            diagnostic: ad,
        } => match b {
            ParserPlaceholder::ModuleArgument {
                name: b,
                diagnostic: bd,
            } => {
                let ModuleArgumentDiagnostic {
                    source: a_source,
                    name: an,
                } = ad.as_ref();
                let ModuleArgumentDiagnostic {
                    source: b_source,
                    name: bn,
                } = bd.as_ref();
                a == b && an == bn && same_option(a_source, b_source, same_span)
            }
            _ => false,
        },
    }
}
fn same_step(a: &PathSegment, b: &PathSegment) -> bool {
    match a {
        PathSegment::Field { name: a } => matches!(b,PathSegment::Field {name:b} if a==b),
        PathSegment::Subscript { index: a } => match b {
            PathSegment::Subscript { index: b } => match a {
                Subscript::Str { key: a } => matches!(b,Subscript::Str {key:b} if same_text(a,b)),
                Subscript::Int { index: a } => matches!(b,Subscript::Int {index:b} if a==b),
            },
            _ => false,
        },
    }
}
fn same_path<B: BindingAddress>(a: &AccessPath<B>, b: &AccessPath<B>) -> bool {
    let AccessPath {
        binding: a,
        attribute: aa,
        segments: as_,
        reference: ar,
    } = a;
    let AccessPath {
        binding: b,
        attribute: ba,
        segments: bs,
        reference: br,
    } = b;
    a == b && aa == ba && same_slice(as_, bs, same_step) && ar.locations() == br.locations()
}
fn same_access_locations(
    a: &crate::parser::AccessLocations,
    b: &crate::parser::AccessLocations,
) -> bool {
    use crate::parser::AccessLocations;
    match a {
        AccessLocations::Reference(a) => matches!(b,AccessLocations::Reference(b) if a==b),
        AccessLocations::Selection(a) => matches!(b,AccessLocations::Selection(b) if a==b),
        AccessLocations::Authored {
            locations: a,
            node: an,
        } => {
            matches!(b,AccessLocations::Authored {locations:b,node:bn} if an==bn && same_access_locations(a,b))
        }
    }
}
fn same_display_root(a: &LoopAccessDisplayRoot, b: &LoopAccessDisplayRoot) -> bool {
    match a {
        LoopAccessDisplayRoot::Rooted(a) => matches!(b,LoopAccessDisplayRoot::Rooted(b) if a==b),
        LoopAccessDisplayRoot::DeferredMiss(a) => {
            matches!(b,LoopAccessDisplayRoot::DeferredMiss(b) if a==b)
        }
    }
}
fn same_argument<B: BindingAddress>(
    a: &FunctionArgument<ParserPlaceholder, B>,
    b: &FunctionArgument<ParserPlaceholder, B>,
) -> bool {
    let FunctionArgument { operand: a } = a;
    let FunctionArgument { operand: b } = b;
    match a {
        StructuralOperand::Value {
            value: a,
            access_base: ab,
        } => {
            matches!(b,StructuralOperand::Value {value:b,access_base:bb} if same_value(a,b) && same_option(ab,bb,same_value))
        }
        StructuralOperand::Handle {
            handle: a,
            access_base: ab,
        } => {
            matches!(b,StructuralOperand::Handle {handle:b,access_base:bb} if a==b && same_value(ab,bb))
        }
        StructuralOperand::Parameter {
            name: a,
            access_base: ab,
        } => {
            matches!(b,StructuralOperand::Parameter {name:b,access_base:bb} if a==b && same_value(ab,bb))
        }
    }
}
fn same_function<B: BindingAddress>(
    a: &crate::parser::UserFunction<B>,
    b: &crate::parser::UserFunction<B>,
) -> bool {
    use crate::parser::{FnParam, UserFunction, UserFunctionBody};
    let UserFunction {
        type_annotations: a_annotations,
        name: a,
        params: ap,
        return_type: at,
        return_type_span: ats,
        annotation_source_file: af,
        local_lets: al,
        body: ab,
    } = a;
    let UserFunction {
        type_annotations: b_annotations,
        name: b,
        params: bp,
        return_type: bt,
        return_type_span: bts,
        annotation_source_file: bf,
        local_lets: bl,
        body: bb,
    } = b;
    let UserFunctionBody {
        location: ab_loc,
        value: av,
        source_line: asl,
        source_file: asf,
    } = ab;
    let UserFunctionBody {
        location: bb_loc,
        value: bv,
        source_line: bsl,
        source_file: bsf,
    } = bb;
    a == b
        && at == bt
        && ats == bts
        && af == bf
        && ab_loc == bb_loc
        && asl == bsl
        && asf == bsf
        && same_value(av, bv)
        && same_slice(a_annotations, b_annotations, |a, b| {
            a.location() == b.location() && a.detail() == b.detail()
        })
        && same_slice(ap, bp, |a, b| {
            let FnParam {
                name: a,
                param_type: at,
                type_span: ats,
                default: ad,
            } = a;
            let FnParam {
                name: b,
                param_type: bt,
                type_span: bts,
                default: bd,
            } = b;
            a == b && at == bt && ats == bts && same_option(ad, bd, same_value)
        })
        && same_slice(al, bl, |(an, av, al), (bn, bv, bl)| {
            an == bn && al == bl && same_value(av, bv)
        })
}
fn same_value<B: BindingAddress>(
    a: &Value<ParserPlaceholder, B>,
    b: &Value<ParserPlaceholder, B>,
) -> bool {
    match a {
        Value::Reference(never) | Value::Conversion(never) => match *never {},
        Value::Concrete(a) => match b {
            Value::Concrete(b) => match a {
                ConcreteValue::String(a) => matches!(b,ConcreteValue::String(b) if same_text(a,b)),
                ConcreteValue::EnumIdentifier(a) => {
                    matches!(b,ConcreteValue::EnumIdentifier(b) if a.as_str()==b.as_str() && a.parsed()==b.parsed())
                }
                ConcreteValue::CanonicalEnum(a) => {
                    matches!(b,ConcreteValue::CanonicalEnum(b) if a==b)
                }
                ConcreteValue::Int(a) => matches!(b,ConcreteValue::Int(b) if a==b),
                ConcreteValue::Float(a) => {
                    matches!(b,ConcreteValue::Float(b) if a.get().to_bits()==b.get().to_bits())
                }
                ConcreteValue::Bool(a) => matches!(b,ConcreteValue::Bool(b) if a==b),
                ConcreteValue::Duration(a) => matches!(b,ConcreteValue::Duration(b) if a==b),
                ConcreteValue::StringList(a) => {
                    matches!(b,ConcreteValue::StringList(b) if same_slice(a,b,same_text))
                }
                ConcreteValue::List(a) => {
                    matches!(b,ConcreteValue::List(b) if same_slice(a,b,same_value))
                }
                ConcreteValue::Map(a) => {
                    matches!(b,ConcreteValue::Map(b) if a.len()==b.len() && a.iter().zip(b).all(|((ak,av),(bk,bv))| ak==bk && same_value(av,bv)))
                }
            },
            _ => false,
        },
        Value::Deferred(a) => match b {
            Value::Deferred(b) => same_deferred(a, b),
            _ => false,
        },
    }
}
fn same_deferred<B: BindingAddress>(
    a: &DeferredValue<ParserPlaceholder, B>,
    b: &DeferredValue<ParserPlaceholder, B>,
) -> bool {
    match a {
        DeferredValue::ModuleIf(a) => match b {
            DeferredValue::ModuleIf(b) => {
                let crate::parser::DeferredValueIf {
                    condition: a,
                    then_value: at,
                    else_value: ae,
                } = a.as_ref();
                let crate::parser::DeferredValueIf {
                    condition: b,
                    then_value: bt,
                    else_value: be,
                } = b.as_ref();
                let crate::parser::DeferredIfCondition {
                    value: av,
                    source_file: af,
                    source_line: al,
                    source_expression: ax,
                    node_id: an,
                } = a;
                let crate::parser::DeferredIfCondition {
                    value: bv,
                    source_file: bf,
                    source_line: bl,
                    source_expression: bx,
                    node_id: bn,
                } = b;
                af == bf
                    && al == bl
                    && ax == bx
                    && an == bn
                    && same_value(av, bv)
                    && same_value(at, bt)
                    && same_value(ae, be)
            }
            _ => false,
        },
        DeferredValue::ResourceRef { path: a } => {
            matches!(b,DeferredValue::ResourceRef {path:b} if same_path(a,b))
        }
        DeferredValue::BindingRef { binding: a } => {
            matches!(b,DeferredValue::BindingRef {binding:b} if a==b)
        }
        DeferredValue::Coalesce {
            value: a,
            default: ad,
        } => {
            matches!(b,DeferredValue::Coalesce {value:b,default:bd} if same_value(a,b) && same_value(ad,bd))
        }
        DeferredValue::IntegerArithmetic(a) => {
            matches!(b,DeferredValue::IntegerArithmetic(b) if a.same_storage(b,same_value))
        }
        DeferredValue::Interpolation(a) => {
            matches!(b,DeferredValue::Interpolation(b) if same_slice(a,b,|a,b| match a {
                InterpolationPart::Literal(a)=>matches!(b,InterpolationPart::Literal(b) if same_text(a,b)),
                InterpolationPart::Expr(a)=>matches!(b,InterpolationPart::Expr(b) if same_value(a,b)),
            }))
        }
        DeferredValue::FunctionCall {
            name: a,
            args: aa,
            source_line: al,
        } => {
            matches!(b,DeferredValue::FunctionCall {name:b,args:ba,source_line:bl} if a==b && al.get()==bl.get() && same_slice(aa,ba,same_argument))
        }
        DeferredValue::Unavailable {
            name: a,
            args: aa,
            source_line: al,
            reason: ar,
        } => {
            matches!(b,DeferredValue::Unavailable {name:b,args:ba,source_line:bl,reason:br} if a==b && al.get()==bl.get() && ar==br && same_slice(aa,ba,same_argument))
        }
        DeferredValue::ResolvedFunctionCall {
            function: a,
            functions: af,
            args: aa,
            source_line: al,
        } => {
            matches!(b,DeferredValue::ResolvedFunctionCall {function:b,functions:bf,args:ba,source_line:bl} if al.get()==bl.get() && same_function(a,b) && af.len()==bf.len() && af.iter().zip(bf.iter()).all(|((ak,av),(bk,bv))| ak==bk && same_function(av,bv)) && same_slice(aa,ba,same_argument))
        }
        DeferredValue::Access(a) => match b {
            DeferredValue::Access(b) => {
                let AccessExpr {
                    base: a,
                    step: as_,
                    source_line: al,
                    display_root: ad,
                    reference: ar,
                } = a;
                let AccessExpr {
                    base: b,
                    step: bs,
                    source_line: bl,
                    display_root: bd,
                    reference: br,
                } = b;
                al == bl
                    && same_step(as_, bs)
                    && same_value(a, b)
                    && same_option(ad, bd, same_display_root)
                    && match (ar.locations(), br.locations()) {
                        (Some(a), Some(b)) => same_access_locations(a, b),
                        (None, None) => true,
                        _ => false,
                    }
            }
            _ => false,
        },
        DeferredValue::Secret(a) => {
            matches!(b,DeferredValue::Secret(b) if a.same_storage(b,same_value))
        }
        DeferredValue::Placeholder(a) => {
            matches!(b,DeferredValue::Placeholder(b) if same_placeholder(a,b))
        }
        DeferredValue::Unknown(a) => match b {
            DeferredValue::Unknown(b) => match a {
                ApplyTimeUnknown::UpstreamExport { path: a } => {
                    matches!(b,ApplyTimeUnknown::UpstreamExport {path:b} if same_path(a,b))
                }
                ApplyTimeUnknown::UpstreamAllExports { binding: a } => {
                    matches!(b,ApplyTimeUnknown::UpstreamAllExports {binding:b} if a==b)
                }
                ApplyTimeUnknown::PendingReRead(a) => {
                    matches!(b,ApplyTimeUnknown::PendingReRead(b) if a==b)
                }
            },
            _ => false,
        },
    }
}
