//! Parsing and validation of builtin attributes

use ast::{self, Attribute, MetaItem, Name, NestedMetaItemKind};
use errors::{Applicability, Handler};
use feature_gate::{Features, GatedCfg};
use parse::ParseSess;
use syntax_pos::{symbol::Symbol, Span};

use super::{list_contains_name, mark_used, MetaItemKind};

enum AttrError {
    MultipleItem(Name),
    UnknownMetaItem(Name, &'static [&'static str]),
    MissingSince,
    MissingFeature,
    MultipleStabilityLevels,
    UnsupportedLiteral(&'static str, /* is_bytestr */ bool),
}

fn handle_errors(sess: &ParseSess, span: Span, error: AttrError) {
    let diag = &sess.span_diagnostic;
    match error {
        AttrError::MultipleItem(item) => span_err!(diag, span, E0538,
                                                   "multiple '{}' items", item),
        AttrError::UnknownMetaItem(item, expected) => {
            let expected = expected
                .iter()
                .map(|name| format!("`{}`", name))
                .collect::<Vec<_>>();
            struct_span_err!(diag, span, E0541, "unknown meta item '{}'", item)
                .span_label(span, format!("expected one of {}", expected.join(", ")))
                .emit();
        }
        AttrError::MissingSince => span_err!(diag, span, E0542, "missing 'since'"),
        AttrError::MissingFeature => span_err!(diag, span, E0546, "missing 'feature'"),
        AttrError::MultipleStabilityLevels => span_err!(diag, span, E0544,
                                                        "multiple stability levels"),
        AttrError::UnsupportedLiteral(
            msg,
            is_bytestr,
        ) => {
            let mut err = struct_span_err!(diag, span, E0565, "{}", msg);
            if is_bytestr {
                if let Ok(lint_str) = sess.source_map().span_to_snippet(span) {
                    err.span_suggestion_with_applicability(
                        span,
                        "consider removing the prefix",
                        format!("{}", &lint_str[1..]),
                        Applicability::MaybeIncorrect,
                    );
                }
            }
            err.emit();
        }
    }
}

#[derive(Copy, Clone, Hash, PartialEq, RustcEncodable, RustcDecodable)]
pub enum InlineAttr {
    None,
    Hint,
    Always,
    Never,
}

#[derive(Copy, Clone, PartialEq)]
pub enum UnwindAttr {
    Allowed,
    Aborts,
}

/// Determine what `#[unwind]` attribute is present in `attrs`, if any.
pub fn find_unwind_attr(diagnostic: Option<&Handler>, attrs: &[Attribute]) -> Option<UnwindAttr> {
    let syntax_error = |attr: &Attribute| {
        mark_used(attr);
        diagnostic.map(|d| {
            span_err!(d, attr.span, E0633, "malformed `#[unwind]` attribute");
        });
        None
    };

    attrs.iter().fold(None, |ia, attr| {
        if attr.path != "unwind" {
            return ia;
        }
        let meta = match attr.meta() {
            Some(meta) => meta.node,
            None => return ia,
        };
        match meta {
            MetaItemKind::Word => {
                syntax_error(attr)
            }
            MetaItemKind::List(ref items) => {
                mark_used(attr);
                if items.len() != 1 {
                    syntax_error(attr)
                } else if list_contains_name(&items[..], "allowed") {
                    Some(UnwindAttr::Allowed)
                } else if list_contains_name(&items[..], "aborts") {
                    Some(UnwindAttr::Aborts)
                } else {
                    syntax_error(attr)
                }
            }
            _ => ia,
        }
    })
}

/// Represents the #[stable], #[unstable], #[rustc_{deprecated,const_unstable}] attributes.
#[derive(RustcEncodable, RustcDecodable, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Stability {
    pub level: StabilityLevel,
    pub feature: Symbol,
    pub rustc_depr: Option<RustcDeprecation>,
    /// `None` means the function is stable but needs to be a stable const fn, too
    /// `Some` contains the feature gate required to be able to use the function
    /// as const fn
    pub const_stability: Option<Symbol>,
    /// whether the function has a `#[rustc_promotable]` attribute
    pub promotable: bool,
}

/// The available stability levels.
#[derive(RustcEncodable, RustcDecodable, PartialEq, PartialOrd, Clone, Debug, Eq, Hash)]
pub enum StabilityLevel {
    // Reason for the current stability level and the relevant rust-lang issue
    Unstable { reason: Option<Symbol>, issue: u32 },
    Stable { since: Symbol },
}

impl StabilityLevel {
    pub fn is_unstable(&self) -> bool {
        if let StabilityLevel::Unstable {..} = *self {
            true
        } else {
            false
        }
    }
    pub fn is_stable(&self) -> bool {
        if let StabilityLevel::Stable {..} = *self {
            true
        } else {
            false
        }
    }
}

#[derive(RustcEncodable, RustcDecodable, PartialEq, PartialOrd, Clone, Debug, Eq, Hash)]
pub struct RustcDeprecation {
    pub since: Symbol,
    pub reason: Symbol,
}

/// Check if `attrs` contains an attribute like `#![feature(feature_name)]`.
/// This will not perform any "sanity checks" on the form of the attributes.
pub fn contains_feature_attr(attrs: &[Attribute], feature_name: &str) -> bool {
    attrs.iter().any(|item| {
        item.check_name("feature") &&
        item.meta_item_list().map(|list| {
            list.iter().any(|mi| {
                mi.word().map(|w| w.name() == feature_name)
                         .unwrap_or(false)
            })
        }).unwrap_or(false)
    })
}

/// Find the first stability attribute. `None` if none exists.
pub fn find_stability(sess: &ParseSess, attrs: &[Attribute],
                      item_sp: Span) -> Option<Stability> {
    find_stability_generic(sess, attrs.iter(), item_sp)
}

fn find_stability_generic<'a, I>(sess: &ParseSess,
                                 attrs_iter: I,
                                 item_sp: Span)
                                 -> Option<Stability>
    where I: Iterator<Item = &'a Attribute>
{
    use self::StabilityLevel::*;

    let mut stab: Option<Stability> = None;
    let mut rustc_depr: Option<RustcDeprecation> = None;
    let mut rustc_const_unstable: Option<Symbol> = None;
    let mut promotable = false;
    let diagnostic = &sess.span_diagnostic;

    'outer: for attr in attrs_iter {
        if ![
            "rustc_deprecated",
            "rustc_const_unstable",
            "unstable",
            "stable",
            "rustc_promotable",
        ].iter().any(|&s| attr.path == s) {
            continue // not a stability level
        }

        mark_used(attr);

        let meta = attr.meta();

        if attr.path == "rustc_promotable" {
            promotable = true;
        }
        // attributes with data
        else if let Some(MetaItem { node: MetaItemKind::List(ref metas), .. }) = meta {
            let meta = meta.as_ref().unwrap();
            let get = |meta: &MetaItem, item: &mut Option<Symbol>| {
                if item.is_some() {
                    handle_errors(sess, meta.span, AttrError::MultipleItem(meta.name()));
                    return false
                }
                if let Some(v) = meta.value_str() {
                    *item = Some(v);
                    true
                } else {
                    span_err!(diagnostic, meta.span, E0539, "incorrect meta item");
                    false
                }
            };

            macro_rules! get_meta {
                ($($name:ident),+) => {
                    $(
                        let mut $name = None;
                    )+
                    for meta in metas {
                        if let Some(mi) = meta.meta_item() {
                            match &*mi.name().as_str() {
                                $(
                                    stringify!($name)
                                        => if !get(mi, &mut $name) { continue 'outer },
                                )+
                                _ => {
                                    let expected = &[ $( stringify!($name) ),+ ];
                                    handle_errors(
                                        sess,
                                        mi.span,
                                        AttrError::UnknownMetaItem(mi.name(), expected),
                                    );
                                    continue 'outer
                                }
                            }
                        } else {
                            handle_errors(
                                sess,
                                meta.span,
                                AttrError::UnsupportedLiteral(
                                    "unsupported literal",
                                    false,
                                ),
                            );
                            continue 'outer
                        }
                    }
                }
            }

            match &*meta.name().as_str() {
                "rustc_deprecated" => {
                    if rustc_depr.is_some() {
                        span_err!(diagnostic, item_sp, E0540,
                                  "multiple rustc_deprecated attributes");
                        continue 'outer
                    }

                    get_meta!(since, reason);

                    match (since, reason) {
                        (Some(since), Some(reason)) => {
                            rustc_depr = Some(RustcDeprecation {
                                since,
                                reason,
                            })
                        }
                        (None, _) => {
                            handle_errors(sess, attr.span(), AttrError::MissingSince);
                            continue
                        }
                        _ => {
                            span_err!(diagnostic, attr.span(), E0543, "missing 'reason'");
                            continue
                        }
                    }
                }
                "rustc_const_unstable" => {
                    if rustc_const_unstable.is_some() {
                        span_err!(diagnostic, item_sp, E0553,
                                  "multiple rustc_const_unstable attributes");
                        continue 'outer
                    }

                    get_meta!(feature);
                    if let Some(feature) = feature {
                        rustc_const_unstable = Some(feature);
                    } else {
                        span_err!(diagnostic, attr.span(), E0629, "missing 'feature'");
                        continue
                    }
                }
                "unstable" => {
                    if stab.is_some() {
                        handle_errors(sess, attr.span(), AttrError::MultipleStabilityLevels);
                        break
                    }

                    let mut feature = None;
                    let mut reason = None;
                    let mut issue = None;
                    for meta in metas {
                        if let Some(mi) = meta.meta_item() {
                            match &*mi.name().as_str() {
                                "feature" => if !get(mi, &mut feature) { continue 'outer },
                                "reason" => if !get(mi, &mut reason) { continue 'outer },
                                "issue" => if !get(mi, &mut issue) { continue 'outer },
                                _ => {
                                    handle_errors(
                                        sess,
                                        meta.span,
                                        AttrError::UnknownMetaItem(
                                            mi.name(),
                                            &["feature", "reason", "issue"]
                                        ),
                                    );
                                    continue 'outer
                                }
                            }
                        } else {
                            handle_errors(
                                sess,
                                meta.span,
                                AttrError::UnsupportedLiteral(
                                    "unsupported literal",
                                    false,
                                ),
                            );
                            continue 'outer
                        }
                    }

                    match (feature, reason, issue) {
                        (Some(feature), reason, Some(issue)) => {
                            stab = Some(Stability {
                                level: Unstable {
                                    reason,
                                    issue: {
                                        if let Ok(issue) = issue.as_str().parse() {
                                            issue
                                        } else {
                                            span_err!(diagnostic, attr.span(), E0545,
                                                      "incorrect 'issue'");
                                            continue
                                        }
                                    }
                                },
                                feature,
                                rustc_depr: None,
                                const_stability: None,
                                promotable: false,
                            })
                        }
                        (None, _, _) => {
                            handle_errors(sess, attr.span(), AttrError::MissingFeature);
                            continue
                        }
                        _ => {
                            span_err!(diagnostic, attr.span(), E0547, "missing 'issue'");
                            continue
                        }
                    }
                }
                "stable" => {
                    if stab.is_some() {
                        handle_errors(sess, attr.span(), AttrError::MultipleStabilityLevels);
                        break
                    }

                    let mut feature = None;
                    let mut since = None;
                    for meta in metas {
                        match &meta.node {
                            NestedMetaItemKind::MetaItem(mi) => {
                                match &*mi.name().as_str() {
                                    "feature" => if !get(mi, &mut feature) { continue 'outer },
                                    "since" => if !get(mi, &mut since) { continue 'outer },
                                    _ => {
                                        handle_errors(
                                            sess,
                                            meta.span,
                                            AttrError::UnknownMetaItem(
                                                mi.name(), &["since", "note"],
                                            ),
                                        );
                                        continue 'outer
                                    }
                                }
                            },
                            NestedMetaItemKind::Literal(lit) => {
                                handle_errors(
                                    sess,
                                    lit.span,
                                    AttrError::UnsupportedLiteral(
                                        "unsupported literal",
                                        false,
                                    ),
                                );
                                continue 'outer
                            }
                        }
                    }

                    match (feature, since) {
                        (Some(feature), Some(since)) => {
                            stab = Some(Stability {
                                level: Stable {
                                    since,
                                },
                                feature,
                                rustc_depr: None,
                                const_stability: None,
                                promotable: false,
                            })
                        }
                        (None, _) => {
                            handle_errors(sess, attr.span(), AttrError::MissingFeature);
                            continue
                        }
                        _ => {
                            handle_errors(sess, attr.span(), AttrError::MissingSince);
                            continue
                        }
                    }
                }
                _ => unreachable!()
            }
        }
    }

    // Merge the deprecation info into the stability info
    if let Some(rustc_depr) = rustc_depr {
        if let Some(ref mut stab) = stab {
            stab.rustc_depr = Some(rustc_depr);
        } else {
            span_err!(diagnostic, item_sp, E0549,
                      "rustc_deprecated attribute must be paired with \
                       either stable or unstable attribute");
        }
    }

    // Merge the const-unstable info into the stability info
    if let Some(feature) = rustc_const_unstable {
        if let Some(ref mut stab) = stab {
            stab.const_stability = Some(feature);
        } else {
            span_err!(diagnostic, item_sp, E0630,
                      "rustc_const_unstable attribute must be paired with \
                       either stable or unstable attribute");
        }
    }

    // Merge the const-unstable info into the stability info
    if promotable {
        if let Some(ref mut stab) = stab {
            stab.promotable = true;
        } else {
            span_err!(diagnostic, item_sp, E0717,
                      "rustc_promotable attribute must be paired with \
                       either stable or unstable attribute");
        }
    }

    stab
}

pub fn find_crate_name(attrs: &[Attribute]) -> Option<Symbol> {
    super::first_attr_value_str_by_name(attrs, "crate_name")
}

/// Tests if a cfg-pattern matches the cfg set
pub fn cfg_matches(cfg: &ast::MetaItem, sess: &ParseSess, features: Option<&Features>) -> bool {
    eval_condition(cfg, sess, &mut |cfg| {
        if let (Some(feats), Some(gated_cfg)) = (features, GatedCfg::gate(cfg)) {
            gated_cfg.check_and_emit(sess, feats);
        }
        let error = |span, msg| { sess.span_diagnostic.span_err(span, msg); true };
        if cfg.ident.segments.len() != 1 {
            return error(cfg.ident.span, "`cfg` predicate key must be an identifier");
        }
        match &cfg.node {
            MetaItemKind::List(..) => {
                error(cfg.span, "unexpected parentheses after `cfg` predicate key")
            }
            MetaItemKind::NameValue(lit) if !lit.node.is_str() => {
                handle_errors(
                    sess,
                    lit.span,
                    AttrError::UnsupportedLiteral(
                        "literal in `cfg` predicate value must be a string",
                        lit.node.is_bytestr()
                    ),
                );
                true
            }
            MetaItemKind::NameValue(..) | MetaItemKind::Word => {
                sess.config.contains(&(cfg.name(), cfg.value_str()))
            }
        }
    })
}

/// Evaluate a cfg-like condition (with `any` and `all`), using `eval` to
/// evaluate individual items.
pub fn eval_condition<F>(cfg: &ast::MetaItem, sess: &ParseSess, eval: &mut F)
                         -> bool
    where F: FnMut(&ast::MetaItem) -> bool
{
    match cfg.node {
        ast::MetaItemKind::List(ref mis) => {
            for mi in mis.iter() {
                if !mi.is_meta_item() {
                    handle_errors(
                        sess,
                        mi.span,
                        AttrError::UnsupportedLiteral(
                            "unsupported literal",
                            false
                        ),
                    );
                    return false;
                }
            }

            // The unwraps below may look dangerous, but we've already asserted
            // that they won't fail with the loop above.
            match &*cfg.name().as_str() {
                "any" => mis.iter().any(|mi| {
                    eval_condition(mi.meta_item().unwrap(), sess, eval)
                }),
                "all" => mis.iter().all(|mi| {
                    eval_condition(mi.meta_item().unwrap(), sess, eval)
                }),
                "not" => {
                    if mis.len() != 1 {
                        span_err!(sess.span_diagnostic, cfg.span, E0536, "expected 1 cfg-pattern");
                        return false;
                    }

                    !eval_condition(mis[0].meta_item().unwrap(), sess, eval)
                },
                p => {
                    span_err!(sess.span_diagnostic, cfg.span, E0537, "invalid predicate `{}`", p);
                    false
                }
            }
        },
        ast::MetaItemKind::Word | ast::MetaItemKind::NameValue(..) => {
            eval(cfg)
        }
    }
}


#[derive(RustcEncodable, RustcDecodable, PartialEq, PartialOrd, Clone, Debug, Eq, Hash)]
pub struct Deprecation {
    pub since: Option<Symbol>,
    pub note: Option<Symbol>,
}

/// Find the deprecation attribute. `None` if none exists.
pub fn find_deprecation(sess: &ParseSess, attrs: &[Attribute],
                        item_sp: Span) -> Option<Deprecation> {
    find_deprecation_generic(sess, attrs.iter(), item_sp)
}

fn find_deprecation_generic<'a, I>(sess: &ParseSess,
                                   attrs_iter: I,
                                   item_sp: Span)
                                   -> Option<Deprecation>
    where I: Iterator<Item = &'a Attribute>
{
    let mut depr: Option<Deprecation> = None;
    let diagnostic = &sess.span_diagnostic;

    'outer: for attr in attrs_iter {
        if attr.path != "deprecated" {
            continue
        }

        mark_used(attr);

        if depr.is_some() {
            span_err!(diagnostic, item_sp, E0550, "multiple deprecated attributes");
            break
        }

        depr = if let Some(metas) = attr.meta_item_list() {
            let get = |meta: &MetaItem, item: &mut Option<Symbol>| {
                if item.is_some() {
                    handle_errors(sess, meta.span, AttrError::MultipleItem(meta.name()));
                    return false
                }
                if let Some(v) = meta.value_str() {
                    *item = Some(v);
                    true
                } else {
                    if let Some(lit) = meta.name_value_literal() {
                        handle_errors(
                            sess,
                            lit.span,
                            AttrError::UnsupportedLiteral(
                                "literal in `deprecated` \
                                value must be a string",
                                lit.node.is_bytestr()
                            ),
                        );
                    } else {
                        span_err!(diagnostic, meta.span, E0551, "incorrect meta item");
                    }

                    false
                }
            };

            let mut since = None;
            let mut note = None;
            for meta in metas {
                match &meta.node {
                    NestedMetaItemKind::MetaItem(mi) => {
                        match &*mi.name().as_str() {
                            "since" => if !get(mi, &mut since) { continue 'outer },
                            "note" => if !get(mi, &mut note) { continue 'outer },
                            _ => {
                                handle_errors(
                                    sess,
                                    meta.span,
                                    AttrError::UnknownMetaItem(mi.name(), &["since", "note"]),
                                );
                                continue 'outer
                            }
                        }
                    }
                    NestedMetaItemKind::Literal(lit) => {
                        handle_errors(
                            sess,
                            lit.span,
                            AttrError::UnsupportedLiteral(
                                "item in `deprecated` must be a key/value pair",
                                false,
                            ),
                        );
                        continue 'outer
                    }
                }
            }

            Some(Deprecation {since: since, note: note})
        } else {
            Some(Deprecation{since: None, note: None})
        }
    }

    depr
}

#[derive(PartialEq, Debug, RustcEncodable, RustcDecodable, Copy, Clone)]
pub enum ReprAttr {
    ReprInt(IntType),
    ReprC,
    ReprPacked(u32),
    ReprSimd,
    ReprTransparent,
    ReprAlign(u32),
}

#[derive(Eq, Hash, PartialEq, Debug, RustcEncodable, RustcDecodable, Copy, Clone)]
pub enum IntType {
    SignedInt(ast::IntTy),
    UnsignedInt(ast::UintTy)
}

impl IntType {
    #[inline]
    pub fn is_signed(self) -> bool {
        use self::IntType::*;

        match self {
            SignedInt(..) => true,
            UnsignedInt(..) => false
        }
    }
}

/// Parse #[repr(...)] forms.
///
/// Valid repr contents: any of the primitive integral type names (see
/// `int_type_of_word`, below) to specify enum discriminant type; `C`, to use
/// the same discriminant size that the corresponding C enum would or C
/// structure layout, `packed` to remove padding, and `transparent` to elegate representation
/// concerns to the only non-ZST field.
pub fn find_repr_attrs(sess: &ParseSess, attr: &Attribute) -> Vec<ReprAttr> {
    use self::ReprAttr::*;

    let mut acc = Vec::new();
    let diagnostic = &sess.span_diagnostic;
    if attr.path == "repr" {
        if let Some(items) = attr.meta_item_list() {
            mark_used(attr);
            for item in items {
                if !item.is_meta_item() {
                    handle_errors(
                        sess,
                        item.span,
                        AttrError::UnsupportedLiteral(
                            "meta item in `repr` must be an identifier",
                            false,
                        ),
                    );
                    continue
                }

                let mut recognised = false;
                if let Some(mi) = item.word() {
                    let word = &*mi.name().as_str();
                    let hint = match word {
                        "C" => Some(ReprC),
                        "packed" => Some(ReprPacked(1)),
                        "simd" => Some(ReprSimd),
                        "transparent" => Some(ReprTransparent),
                        _ => match int_type_of_word(word) {
                            Some(ity) => Some(ReprInt(ity)),
                            None => {
                                None
                            }
                        }
                    };

                    if let Some(h) = hint {
                        recognised = true;
                        acc.push(h);
                    }
                } else if let Some((name, value)) = item.name_value_literal() {
                    let parse_alignment = |node: &ast::LitKind| -> Result<u32, &'static str> {
                        if let ast::LitKind::Int(literal, ast::LitIntType::Unsuffixed) = node {
                            if literal.is_power_of_two() {
                                // rustc::ty::layout::Align restricts align to <= 2^29
                                if *literal <= 1 << 29 {
                                    Ok(*literal as u32)
                                } else {
                                    Err("larger than 2^29")
                                }
                            } else {
                                Err("not a power of two")
                            }
                        } else {
                            Err("not an unsuffixed integer")
                        }
                    };

                    let mut literal_error = None;
                    if name == "align" {
                        recognised = true;
                        match parse_alignment(&value.node) {
                            Ok(literal) => acc.push(ReprAlign(literal)),
                            Err(message) => literal_error = Some(message)
                        };
                    }
                    else if name == "packed" {
                        recognised = true;
                        match parse_alignment(&value.node) {
                            Ok(literal) => acc.push(ReprPacked(literal)),
                            Err(message) => literal_error = Some(message)
                        };
                    }
                    if let Some(literal_error) = literal_error {
                        span_err!(diagnostic, item.span, E0589,
                                  "invalid `repr(align)` attribute: {}", literal_error);
                    }
                } else {
                    if let Some(meta_item) = item.meta_item() {
                        if meta_item.name() == "align" {
                            if let MetaItemKind::NameValue(ref value) = meta_item.node {
                                recognised = true;
                                let mut err = struct_span_err!(diagnostic, item.span, E0693,
                                    "incorrect `repr(align)` attribute format");
                                match value.node {
                                    ast::LitKind::Int(int, ast::LitIntType::Unsuffixed) => {
                                        err.span_suggestion_with_applicability(
                                            item.span,
                                            "use parentheses instead",
                                            format!("align({})", int),
                                            Applicability::MachineApplicable
                                        );
                                    }
                                    ast::LitKind::Str(s, _) => {
                                        err.span_suggestion_with_applicability(
                                            item.span,
                                            "use parentheses instead",
                                            format!("align({})", s),
                                            Applicability::MachineApplicable
                                        );
                                    }
                                    _ => {}
                                }
                                err.emit();
                            }
                        }
                    }
                }
                if !recognised {
                    // Not a word we recognize
                    span_err!(diagnostic, item.span, E0552,
                              "unrecognized representation hint");
                }
            }
        }
    }
    acc
}

fn int_type_of_word(s: &str) -> Option<IntType> {
    use self::IntType::*;

    match s {
        "i8" => Some(SignedInt(ast::IntTy::I8)),
        "u8" => Some(UnsignedInt(ast::UintTy::U8)),
        "i16" => Some(SignedInt(ast::IntTy::I16)),
        "u16" => Some(UnsignedInt(ast::UintTy::U16)),
        "i32" => Some(SignedInt(ast::IntTy::I32)),
        "u32" => Some(UnsignedInt(ast::UintTy::U32)),
        "i64" => Some(SignedInt(ast::IntTy::I64)),
        "u64" => Some(UnsignedInt(ast::UintTy::U64)),
        "i128" => Some(SignedInt(ast::IntTy::I128)),
        "u128" => Some(UnsignedInt(ast::UintTy::U128)),
        "isize" => Some(SignedInt(ast::IntTy::Isize)),
        "usize" => Some(UnsignedInt(ast::UintTy::Usize)),
        _ => None
    }
}
