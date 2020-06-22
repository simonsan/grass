use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::Hash,
};

use codemap::Span;

use indexmap::IndexMap;

use crate::error::SassResult;

use super::{
    ComplexSelector, ComplexSelectorComponent, CompoundSelector, Pseudo, SelectorList,
    SimpleSelector,
};

use extension::Extension;
pub(crate) use functions::unify_complex;
use functions::{paths, weave};
use merged::MergedExtension;
pub(crate) use rule::ExtendRule;

mod extension;
mod functions;
mod merged;
mod rule;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct CssMediaQuery;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
/// Different modes in which extension can run.
enum ExtendMode {
    /// Normal mode, used with the `@extend` rule.
    ///
    /// This preserves existing selectors and extends each target individually.
    Normal,

    /// Replace mode, used by the `selector-replace()` function.
    ///
    /// This replaces existing selectors and requires every target to match to
    /// extend a given compound selector.
    Replace,

    /// All-targets mode, used by the `selector-extend()` function.
    ///
    /// This preserves existing selectors but requires every target to match to
    /// extend a given compound selector.
    AllTargets,
}

impl Default for ExtendMode {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Extender {
    /// A map from all simple selectors in the stylesheet to the selector lists
    /// that contain them.
    ///
    /// This is used to find which selectors an `@extend` applies to and adjust
    /// them.
    selectors: HashMap<SimpleSelector, HashSet<SelectorList>>,

    /// A map from all extended simple selectors to the sources of those
    /// extensions.
    extensions: HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>,

    /// A map from all simple selectors in extenders to the extensions that those
    /// extenders define.
    extensions_by_extender: HashMap<SimpleSelector, Vec<Extension>>,

    /// A map from CSS selectors to the media query contexts they're defined in.
    ///
    /// This tracks the contexts in which each selector's style rule is defined.
    /// If a rule is defined at the top level, it doesn't have an entry.
    media_contexts: HashMap<SelectorList, Vec<CssMediaQuery>>,

    /// A map from `SimpleSelector`s to the specificity of their source
    /// selectors.
    ///
    /// This tracks the maximum specificity of the `ComplexSelector` that
    /// originally contained each `SimpleSelector`. This allows us to ensure that
    /// we don't trim any selectors that need to exist to satisfy the [second law
    /// of extend][].
    ///
    /// [second law of extend]: https://github.com/sass/sass/issues/324#issuecomment-4607184
    source_specificity: HashMap<SimpleSelector, i32>,

    /// A set of `ComplexSelector`s that were originally part of
    /// their component `SelectorList`s, as opposed to being added by `@extend`.
    ///
    /// This allows us to ensure that we don't trim any selectors that need to
    /// exist to satisfy the [first law of extend][].
    ///
    /// [first law of extend]: https://github.com/sass/sass/issues/324#issuecomment-4607184
    originals: HashSet<ComplexSelector>,

    /// The mode that controls this extender's behavior.
    mode: ExtendMode,

    span: Span,
}

impl Extender {
    /// An `Extender` that contains no extensions and can have no extensions added.
    // TODO: empty extender
    #[allow(dead_code)]
    const EMPTY: () = ();

    pub fn extend(
        selector: SelectorList,
        source: SelectorList,
        targets: SelectorList,
        span: Span,
    ) -> SassResult<SelectorList> {
        Self::extend_or_replace(selector, source, targets, ExtendMode::AllTargets, span)
    }

    pub fn new(span: Span) -> Self {
        Self {
            selectors: HashMap::new(),
            extensions: HashMap::new(),
            extensions_by_extender: HashMap::new(),
            media_contexts: HashMap::new(),
            source_specificity: HashMap::new(),
            originals: HashSet::new(),
            mode: ExtendMode::Normal,
            span,
        }
    }

    /// Whether there exist any extensions
    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty()
    }

    pub fn replace(
        selector: SelectorList,
        source: SelectorList,
        targets: SelectorList,
        span: Span,
    ) -> SassResult<SelectorList> {
        Self::extend_or_replace(selector, source, targets, ExtendMode::Replace, span)
    }

    fn extend_or_replace(
        selector: SelectorList,
        source: SelectorList,
        targets: SelectorList,
        mode: ExtendMode,
        span: Span,
    ) -> SassResult<SelectorList> {
        let extenders: IndexMap<ComplexSelector, Extension> = source
            .components
            .into_iter()
            .map(|complex| (complex.clone(), Extension::one_off(complex, None, false)))
            .collect();

        let compound_targets = targets
            .components
            .into_iter()
            .map(|complex| {
                if complex.components.len() == 1 {
                    Ok(complex.components.first().unwrap().as_compound().clone())
                } else {
                    return Err(("Can't extend complex selector $complex.", span).into());
                }
            })
            .collect::<SassResult<Vec<CompoundSelector>>>()?;

        let extensions: HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>> =
            compound_targets
                .into_iter()
                .flat_map(|compound| {
                    compound
                        .components
                        .into_iter()
                        .map(|simple| (simple, extenders.clone()))
                })
                .collect();

        let mut extender = Extender::with_mode(mode, span);

        if !selector.is_invisible() {
            extender
                .originals
                .extend(selector.components.iter().cloned());
        }

        Ok(extender.extend_list(selector, &extensions, &None))
    }

    fn with_mode(mode: ExtendMode, span: Span) -> Self {
        Self {
            mode,
            ..Extender::new(span)
        }
    }

    /// Extends `list` using `extensions`.
    fn extend_list(
        &mut self,
        list: SelectorList,
        extensions: &HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>,
        media_query_context: &Option<Vec<CssMediaQuery>>,
    ) -> SelectorList {
        // This could be written more simply using Vec<Vec<T>>, but we want to avoid
        // any allocations in the common case where no extends apply.
        let mut extended: Vec<ComplexSelector> = Vec::new();
        for i in 0..list.components.len() {
            let complex = list.components.get(i).unwrap().clone();

            if let Some(result) =
                self.extend_complex(complex.clone(), extensions, media_query_context)
            {
                if extended.is_empty() && i != 0 {
                    extended = list.components[0..i].to_vec();
                }
                extended.extend(result.into_iter());
            } else if !extended.is_empty() {
                extended.push(complex);
            }
        }

        if extended.is_empty() {
            return list;
        }

        SelectorList {
            components: self.trim(extended, |complex| self.originals.contains(complex)),
            span: self.span,
        }
    }

    /// Extends `complex` using `extensions`, and returns the contents of a
    /// `SelectorList`.
    fn extend_complex(
        &mut self,
        complex: ComplexSelector,
        extensions: &HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>,
        media_query_context: &Option<Vec<CssMediaQuery>>,
    ) -> Option<Vec<ComplexSelector>> {
        // The complex selectors that each compound selector in `complex.components`
        // can expand to.
        //
        // For example, given
        //
        //     .a .b {...}
        //     .x .y {@extend .b}
        //
        // this will contain
        //
        //     [
        //       [.a],
        //       [.b, .x .y]
        //     ]
        //
        // This could be written more simply using `Vec::into_iter::map`, but we want to avoid
        // any allocations in the common case where no extends apply.
        let mut extended_not_expanded: Vec<Vec<ComplexSelector>> = Vec::new();

        let complex_has_line_break = complex.line_break;

        for i in 0..complex.components.len() {
            if let Some(ComplexSelectorComponent::Compound(component)) = complex.components.get(i) {
                if let Some(extended) =
                    self.extend_compound(component, extensions, media_query_context)
                {
                    if extended_not_expanded.is_empty() {
                        extended_not_expanded = complex
                            .components
                            .clone()
                            .into_iter()
                            .take(i)
                            .map(|component| {
                                vec![ComplexSelector {
                                    components: vec![component],
                                    line_break: complex.line_break,
                                }]
                            })
                            .collect();
                    }
                    extended_not_expanded.push(extended);
                } else {
                    extended_not_expanded.push(vec![ComplexSelector {
                        components: vec![ComplexSelectorComponent::Compound(component.clone())],
                        line_break: false,
                    }])
                }
            } else if let Some(component @ ComplexSelectorComponent::Combinator(..)) =
                complex.components.get(i)
            {
                extended_not_expanded.push(vec![ComplexSelector {
                    components: vec![component.clone()],
                    line_break: false,
                }])
            }
        }

        if extended_not_expanded.is_empty() {
            return None;
        }

        let mut first = true;

        Some(
            paths(extended_not_expanded)
                .into_iter()
                .flat_map(move |path| {
                    weave(
                        path.clone()
                            .into_iter()
                            .map(move |complex| complex.components)
                            .collect(),
                    )
                    .into_iter()
                    .map(|components| {
                        let output_complex = ComplexSelector {
                            components,
                            line_break: complex_has_line_break
                                || path.iter().any(|input_complex| input_complex.line_break),
                        };

                        // Make sure that copies of `complex` retain their status as "original"
                        // selectors. This includes selectors that are modified because a :not()
                        // was extended into.
                        if first && self.originals.contains(&complex.clone()) {
                            self.originals.insert(output_complex.clone());
                        }
                        first = false;

                        output_complex
                    })
                    .collect::<Vec<ComplexSelector>>()
                })
                .collect(),
        )
    }

    /// Extends `compound` using `extensions`, and returns the contents of a
    /// `SelectorList`.
    ///
    /// The `in_original` parameter indicates whether this is in an original
    /// complex selector, meaning that `compound` should not be trimmed out.
    // todo: `in_original` is actually obsolete and we should upstream its removal
    // to dart-sass
    fn extend_compound(
        &mut self,
        compound: &CompoundSelector,
        extensions: &HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>,
        media_query_context: &Option<Vec<CssMediaQuery>>,
    ) -> Option<Vec<ComplexSelector>> {
        // If there's more than one target and they all need to match, we track
        // which targets are actually extended.
        let mut targets_used: HashSet<SimpleSelector> = HashSet::new();

        let mut options: Vec<Vec<Extension>> = Vec::new();

        for i in 0..compound.components.len() {
            let simple = compound.components.get(i).cloned().unwrap();

            if let Some(extended) = self.extend_simple(
                simple.clone(),
                extensions,
                media_query_context,
                &mut targets_used,
            ) {
                if options.is_empty() && i != 0 {
                    options.push(vec![self.extension_for_compound(
                        compound.components.clone().into_iter().take(i).collect(),
                    )]);
                }

                options.extend(extended.into_iter());
            } else {
                options.push(vec![self.extension_for_simple(simple)]);
            }
        }

        if options.is_empty() {
            return None;
        }

        // If `self.mode` isn't `ExtendMode::Normal` and we didn't use all the targets in
        // `extensions`, extension fails for `compound`.
        // todo: test for `extensions.len() > 2`. may cause issues
        if !targets_used.is_empty()
            && targets_used.len() != extensions.len()
            && self.mode != ExtendMode::Normal
        {
            return None;
        }

        // Optimize for the simple case of a single simple selector that doesn't
        // need any unification.
        if options.len() == 1 {
            return Some(
                options
                    .first()?
                    .clone()
                    .into_iter()
                    .map(|state| {
                        state.assert_compatible_media_context(media_query_context);
                        state.extender
                    })
                    .collect(),
            );
        }

        // Find all paths through `options`. In this case, each path represents a
        // different unification of the base selector. For example, if we have:
        //
        //     .a.b {...}
        //     .w .x {@extend .a}
        //     .y .z {@extend .b}
        //
        // then `options` is `[[.a, .w .x], [.b, .y .z]]` and `paths(options)` is
        //
        //     [
        //       [.a, .b],
        //       [.a, .y .z],
        //       [.w .x, .b],
        //       [.w .x, .y .z]
        //     ]
        //
        // We then unify each path to get a list of complex selectors:
        //
        //     [
        //       [.a.b],
        //       [.y .a.z],
        //       [.w .x.b],
        //       [.w .y .x.z, .y .w .x.z]
        //     ]
        let mut first = self.mode != ExtendMode::Replace;

        let unified_paths: Vec<Option<Vec<ComplexSelector>>> = paths(options)
            .into_iter()
            .map(|path| {
                let complexes: Vec<Vec<ComplexSelectorComponent>> = if first {
                    // The first path is always the original selector. We can't just
                    // return `compound` directly because pseudo selectors may be
                    // modified, but we don't have to do any unification.
                    first = false;

                    vec![vec![ComplexSelectorComponent::Compound(CompoundSelector {
                        components: path
                            .clone()
                            .into_iter()
                            .flat_map(|state| {
                                debug_assert!(state.extender.components.len() == 1);
                                match state.extender.components.last().cloned() {
                                    Some(ComplexSelectorComponent::Compound(c)) => c.components,
                                    Some(..) | None => unreachable!(),
                                }
                            })
                            .collect(),
                    })]]
                } else {
                    let mut to_unify: VecDeque<Vec<ComplexSelectorComponent>> = VecDeque::new();
                    let mut originals: Vec<SimpleSelector> = Vec::new();

                    for state in path.clone() {
                        if state.is_original {
                            originals.extend(match state.extender.components.last().cloned() {
                                Some(ComplexSelectorComponent::Compound(c)) => c.components,
                                Some(..) | None => unreachable!(),
                            });
                        } else {
                            to_unify.push_back(state.extender.components.clone());
                        }
                    }
                    if !originals.is_empty() {
                        to_unify.push_front(vec![ComplexSelectorComponent::Compound(
                            CompoundSelector {
                                components: originals,
                            },
                        )]);
                    }

                    unify_complex(Vec::from(to_unify))?
                };

                let mut line_break = false;

                for state in path {
                    state.assert_compatible_media_context(media_query_context);
                    line_break = line_break || state.extender.line_break;
                }

                Some(
                    complexes
                        .into_iter()
                        .map(|components| ComplexSelector {
                            components,
                            line_break,
                        })
                        .collect(),
                )
            })
            .collect();

        Some(
            unified_paths
                .into_iter()
                .filter_map(|complexes| complexes)
                .flatten()
                .collect(),
        )
    }

    fn extend_simple(
        &mut self,
        simple: SimpleSelector,
        extensions: &HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>,
        media_query_context: &Option<Vec<CssMediaQuery>>,
        targets_used: &mut HashSet<SimpleSelector>,
    ) -> Option<Vec<Vec<Extension>>> {
        if let SimpleSelector::Pseudo(
            simple @ Pseudo {
                selector: Some(..), ..
            },
        ) = simple.clone()
        {
            if let Some(extended) = self.extend_pseudo(simple, extensions, media_query_context) {
                return Some(
                    extended
                        .into_iter()
                        .map(move |pseudo| {
                            self.without_pseudo(
                                SimpleSelector::Pseudo(pseudo.clone()),
                                extensions,
                                targets_used,
                                self.mode,
                            )
                            .unwrap_or_else(|| {
                                vec![self.extension_for_simple(SimpleSelector::Pseudo(pseudo))]
                            })
                        })
                        .collect(),
                );
            }
        }

        self.without_pseudo(simple, extensions, targets_used, self.mode)
            .map(|v| vec![v])
    }

    /// Extends `pseudo` using `extensions`, and returns a list of resulting
    /// pseudo selectors.
    fn extend_pseudo(
        &mut self,
        pseudo: Pseudo,
        extensions: &HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>,
        media_query_context: &Option<Vec<CssMediaQuery>>,
    ) -> Option<Vec<Pseudo>> {
        let extended = self.extend_list(
            pseudo
                .selector
                .clone()
                .unwrap_or_else(|| SelectorList::new(self.span)),
            extensions,
            media_query_context,
        );
        /*todo: identical(extended, pseudo.selector)*/
        if Some(&extended) == pseudo.selector.as_ref() {
            return None;
        }

        // For `:not()`, we usually want to get rid of any complex selectors because
        // that will cause the selector to fail to parse on all browsers at time of
        // writing. We can keep them if either the original selector had a complex
        // selector, or the result of extending has only complex selectors, because
        // either way we aren't breaking anything that isn't already broken.
        let mut complexes = if pseudo.normalized_name == "not"
            && !pseudo
                .selector
                .clone()
                .unwrap()
                .components
                .iter()
                .any(|complex| complex.components.len() > 1)
            && extended
                .components
                .iter()
                .any(|complex| complex.components.len() == 1)
        {
            extended
                .components
                .into_iter()
                .filter(|complex| complex.components.len() <= 1)
                .collect()
        } else {
            extended.components
        };

        complexes = complexes
            .into_iter()
            .flat_map(|complex| {
                if complex.components.len() != 1 {
                    return vec![complex];
                }
                let compound = match complex.components.first() {
                    Some(ComplexSelectorComponent::Compound(c)) => c,
                    Some(..) | None => return vec![complex],
                };
                if compound.components.len() != 1 {
                    return vec![complex];
                }
                if !compound.components.first().unwrap().is_pseudo() {
                    return vec![complex];
                }
                let inner_pseudo = match compound.components.first() {
                    Some(SimpleSelector::Pseudo(pseudo)) => pseudo,
                    Some(..) | None => return vec![complex],
                };
                if inner_pseudo.selector.is_none() {
                    return vec![complex];
                }

                match pseudo.normalized_name.as_str() {
                    "not" => {
                        // In theory, if there's a `:not` nested within another `:not`, the
                        // inner `:not`'s contents should be unified with the return value.
                        // For example, if `:not(.foo)` extends `.bar`, `:not(.bar)` should
                        // become `.foo:not(.bar)`. However, this is a narrow edge case and
                        // supporting it properly would make this code and the code calling it
                        // a lot more complicated, so it's not supported for now.
                        if inner_pseudo.normalized_name == "matches" {
                            inner_pseudo.selector.clone().unwrap().components
                        } else {
                            Vec::new()
                        }
                    }
                    "matches" | "any" | "current" | "nth-child" | "nth-last-child" => {
                        // As above, we could theoretically support :not within :matches, but
                        // doing so would require this method and its callers to handle much
                        // more complex cases that likely aren't worth the pain.
                        if inner_pseudo.name != pseudo.name
                            || inner_pseudo.argument != pseudo.argument
                        {
                            Vec::new()
                        } else {
                            inner_pseudo.selector.clone().unwrap().components
                        }
                    }
                    "has" | "host" | "host-context" | "slotted" => {
                        // We can't expand nested selectors here, because each layer adds an
                        // additional layer of semantics. For example, `:has(:has(img))`
                        // doesn't match `<div><img></div>` but `:has(img)` does.
                        vec![complex]
                    }
                    _ => Vec::new(),
                }
            })
            .collect();

        // Older browsers support `:not`, but only with a single complex selector.
        // In order to support those browsers, we break up the contents of a `:not`
        // unless it originally contained a selector list.
        if pseudo.normalized_name == "not" && pseudo.selector.clone().unwrap().components.len() == 1
        {
            let result = complexes
                .into_iter()
                .map(|complex| {
                    pseudo.clone().with_selector(Some(SelectorList {
                        components: vec![complex],
                        span: self.span,
                    }))
                })
                .collect::<Vec<Pseudo>>();
            if result.is_empty() {
                None
            } else {
                Some(result)
            }
        } else {
            Some(vec![pseudo.with_selector(Some(SelectorList {
                components: complexes,
                span: self.span,
            }))])
        }
    }

    /// Extends `simple` without extending the contents of any selector pseudos
    /// it contains.
    fn without_pseudo(
        &self,
        simple: SimpleSelector,
        extensions: &HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>,
        targets_used: &mut HashSet<SimpleSelector>,
        mode: ExtendMode,
    ) -> Option<Vec<Extension>> {
        let extenders = extensions.get(&simple)?;

        targets_used.insert(simple.clone());

        if mode == ExtendMode::Replace {
            return Some(extenders.values().cloned().collect());
        }

        let mut tmp = vec![self.extension_for_simple(simple)];
        tmp.extend(extenders.values().cloned());

        Some(tmp)
    }

    /// Returns a one-off `Extension` whose extender is composed solely of
    /// `simple`.
    fn extension_for_simple(&self, simple: SimpleSelector) -> Extension {
        let specificity = Some(*self.source_specificity.get(&simple).unwrap_or(&0_i32));
        Extension::one_off(
            ComplexSelector {
                components: vec![ComplexSelectorComponent::Compound(CompoundSelector {
                    components: vec![simple],
                })],
                line_break: false,
            },
            specificity,
            true,
        )
    }

    /// Returns a one-off `Extension` whose extender is composed solely of a
    /// compound selector containing `simples`.
    fn extension_for_compound(&self, simples: Vec<SimpleSelector>) -> Extension {
        let compound = CompoundSelector {
            components: simples,
        };
        let specificity = Some(self.source_specificity_for(&compound));
        Extension::one_off(
            ComplexSelector {
                components: vec![ComplexSelectorComponent::Compound(compound)],
                line_break: false,
            },
            specificity,
            true,
        )
    }

    /// Returns the maximum specificity for sources that went into producing
    /// `compound`.
    fn source_specificity_for(&self, compound: &CompoundSelector) -> i32 {
        let mut specificity = 0;
        for simple in &compound.components {
            specificity = specificity.max(*self.source_specificity.get(simple).unwrap_or(&0));
        }
        specificity
    }

    /// Removes elements from `selectors` if they're subselectors of other
    /// elements.
    ///
    /// The `is_original` callback indicates which selectors are original to the
    /// document, and thus should never be trimmed.
    fn trim(
        &self,
        selectors: Vec<ComplexSelector>,
        is_original: impl Fn(&ComplexSelector) -> bool,
    ) -> Vec<ComplexSelector> {
        // Avoid truly horrific quadratic behavior.
        //
        // TODO(nweiz): I think there may be a way to get perfect trimming without
        // going quadratic by building some sort of trie-like data structure that
        // can be used to look up superselectors.
        if selectors.len() > 100 {
            return selectors;
        }

        // This is n² on the sequences, but only comparing between separate
        // sequences should limit the quadratic behavior. We iterate from last to
        // first and reverse the result so that, if two selectors are identical, we
        // keep the first one.
        let mut result: VecDeque<ComplexSelector> = VecDeque::new();
        let mut num_originals = 0;

        // :outer
        for i in (0..=(selectors.len().saturating_sub(1))).rev() {
            let mut should_continue_to_outer = false;
            let complex1 = selectors.get(i).unwrap();
            if is_original(complex1) {
                // Make sure we don't include duplicate originals, which could happen if
                // a style rule extends a component of its own selector.
                for j in 0..num_originals {
                    if result.get(j) == Some(complex1) {
                        rotate_slice(&mut result, 0, j + 1);
                        should_continue_to_outer = true;
                        break;
                    }
                }
                if should_continue_to_outer {
                    continue;
                }
                num_originals += 1;
                result.push_front(complex1.clone());
                continue;
            }

            // The maximum specificity of the sources that caused `complex1` to be
            // generated. In order for `complex1` to be removed, there must be another
            // selector that's a superselector of it *and* that has specificity
            // greater or equal to this.
            let mut max_specificity = 0;
            for component in &complex1.components {
                if let ComplexSelectorComponent::Compound(compound) = component {
                    max_specificity = max_specificity.max(self.source_specificity_for(compound))
                }
            }

            // Look in `result` rather than `selectors` for selectors after `i`. This
            // ensures that we aren't comparing against a selector that's already been
            // trimmed, and thus that if there are two identical selectors only one is
            // trimmed.
            let should_continue = result.iter().any(|complex2| {
                complex2.min_specificity() >= max_specificity
                    && complex2.is_super_selector(complex1)
            });
            if should_continue {
                continue;
            }

            let should_continue = selectors.iter().take(i).any(|complex2| {
                complex2.min_specificity() >= max_specificity
                    && complex2.is_super_selector(complex1)
            });
            if should_continue {
                continue;
            }

            result.push_front(complex1.clone());
        }

        Vec::from(result)
    }

    /// Adds `selector` to this extender.
    ///
    /// Extends `selector` using any registered extensions, then returns the resulting
    /// selector. If any more relevant extensions are added, the returned selector
    /// is automatically updated.
    ///
    /// The `media_query_context` is the media query context in which the selector was
    /// defined, or `None` if it was defined at the top level of the document.
    // todo: the docs are wrong, and we may want to consider returning an `Rc<RefCell<SelectorList>>`
    // the reason we don't is that it would interfere with hashing
    pub fn add_selector(
        &mut self,
        mut selector: SelectorList,
        // span: Span,
        media_query_context: Option<Vec<CssMediaQuery>>,
    ) -> SelectorList {
        // todo: we should be able to remove this variable and clone
        let original_selector = selector.clone();
        if !original_selector.is_invisible() {
            for complex in &original_selector.components {
                self.originals.insert(complex.clone());
            }
        }

        if !self.extensions.is_empty() {
            let extensions = self.extensions.clone();
            selector = self.extend_list(original_selector, &extensions, &media_query_context);
            /*
              todo: when we have error handling
                  } on SassException catch (error) {
              throw SassException(
                  "From ${error.span.message('')}\n"
                  "${error.message}",
                  span);
            }
              */
        }
        if let Some(mut media_query_context) = media_query_context {
            self.media_contexts
                .get_mut(&selector)
                .replace(&mut media_query_context);
        }
        self.register_selector(selector.clone(), &selector);
        selector
    }

    /// Registers the `SimpleSelector`s in `list` to point to `selector` in
    /// `self.selectors`.
    fn register_selector(&mut self, list: SelectorList, selector: &SelectorList) {
        for complex in list.components {
            for component in complex.components {
                if let ComplexSelectorComponent::Compound(component) = component {
                    for simple in component.components {
                        self.selectors
                            .entry(simple.clone())
                            .or_insert_with(HashSet::new)
                            .insert(selector.clone());

                        if let SimpleSelector::Pseudo(Pseudo {
                            selector: Some(simple_selector),
                            ..
                        }) = simple
                        {
                            self.register_selector(simple_selector, selector);
                        }
                    }
                }
            }
        }
    }

    /// Adds an extension to this extender.
    ///
    /// The `extender` is the selector for the style rule in which the extension
    /// is defined, and `target` is the selector passed to `@extend`. The `extend`
    /// provides the extend span and indicates whether the extension is optional.
    ///
    /// The `media_context` defines the media query context in which the extension
    /// is defined. It can only extend selectors within the same context. A `None`
    /// context indicates no media queries.
    pub fn add_extension(
        &mut self,
        extender: SelectorList,
        target: &SimpleSelector,
        extend: &ExtendRule,
        media_context: &Option<Vec<CssMediaQuery>>,
        span: Option<Span>,
    ) {
        let selectors = self.selectors.get(target).cloned();
        let existing_extensions = self.extensions_by_extender.get(target).cloned();

        let mut new_extensions: Option<IndexMap<ComplexSelector, Extension>> = None;

        let mut sources = self
            .extensions
            .entry(target.clone())
            .or_insert_with(IndexMap::new)
            .clone();

        for complex in extender.components {
            let state = Extension {
                specificity: complex.max_specificity(),
                extender: complex.clone(),
                target: Some(target.clone()),
                span,
                media_context: media_context.clone(),
                is_optional: extend.is_optional,
                is_original: false,
                left: None,
                right: None,
            };

            if let Some(existing_state) = sources.get(&complex) {
                // If there's already an extend from `extender` to `target`, we don't need
                // to re-run the extension. We may need to mark the extension as
                // mandatory, though.
                let mut new_val = MergedExtension::merge(existing_state.clone(), state).unwrap();
                sources.get_mut(&complex).replace(&mut new_val);
                continue;
            }

            sources.insert(complex.clone(), state.clone());

            for component in complex.components.clone() {
                if let ComplexSelectorComponent::Compound(component) = component {
                    for simple in component.components {
                        self.extensions_by_extender
                            .entry(simple.clone())
                            .or_insert_with(Vec::new)
                            .push(state.clone());
                        // Only source specificity for the original selector is relevant.
                        // Selectors generated by `@extend` don't get new specificity.
                        self.source_specificity
                            .entry(simple.clone())
                            .or_insert_with(|| complex.max_specificity());
                    }
                }
            }

            if selectors.is_some() || existing_extensions.is_some() {
                new_extensions
                    .get_or_insert_with(IndexMap::new)
                    .insert(complex.clone(), state.clone());
            }

            let new_extensions = if let Some(new) = new_extensions.clone() {
                new
            } else {
                // TODO: HACK: we extend by sources here, but we should be able to mutate sources directly
                self.extensions
                    .get_mut(target)
                    .get_or_insert(&mut IndexMap::new())
                    .extend(sources);
                return;
            };

            let mut new_extensions_by_target = HashMap::new();
            new_extensions_by_target.insert(target.clone(), new_extensions);

            if let Some(existing_extensions) = existing_extensions.clone() {
                let additional_extensions =
                    self.extend_existing_extensions(existing_extensions, &new_extensions_by_target);
                if let Some(additional_extensions) = additional_extensions {
                    map_add_all_2(&mut new_extensions_by_target, additional_extensions);
                }
            }

            if let Some(selectors) = selectors.clone() {
                self.extend_existing_selectors(selectors, &new_extensions_by_target);
            }
        }

        // TODO: HACK: we extend by sources here, but we should be able to mutate sources directly
        self.extensions
            .get_mut(target)
            .get_or_insert(&mut IndexMap::new())
            .extend(sources);
    }

    /// Extend `extensions` using `new_extensions`.
    ///
    /// Note that this does duplicate some work done by
    /// `Extender::extend_existing_selectors`, but it's necessary to expand each extension's
    /// extender separately without reference to the full selector list, so that
    /// relevant results don't get trimmed too early.
    ///
    /// Returns extensions that should be added to `new_extensions` before
    /// extending selectors in order to properly handle extension loops such as:
    ///```foo
    ///     .c {x: y; @extend .a}
    ///     .x.y.a {@extend .b}
    ///     .z.b {@extend .c}
    ///```
    /// Returns `None` if there are no extensions to add.
    fn extend_existing_extensions(
        &mut self,
        extensions: Vec<Extension>,
        new_extensions: &HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>,
    ) -> Option<HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>> {
        let mut additional_extensions: Option<
            HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>,
        > = None;
        for extension in extensions {
            let mut sources = self
                .extensions
                .get(&extension.target.clone().unwrap())
                .unwrap()
                .clone();

            // `extend_existing_selectors` would have thrown already.
            let selectors: Vec<ComplexSelector> = if let Some(v) = self.extend_complex(
                extension.extender.clone(),
                new_extensions,
                &extension.media_context,
            ) {
                v
            } else {
                continue;
            };
            // todo: when we add error handling, this error is special
            /*
            } on SassException catch (error) {
                throw SassException(
                    "From ${extension.extenderSpan.message('')}\n"
                    "${error.message}",
                    error.span);
            }
            */

            let contains_extension = selectors.first() == Some(&extension.extender);

            let mut first = false;
            for complex in selectors {
                // If the output contains the original complex selector, there's no
                // need to recreate it.
                if contains_extension && first {
                    first = false;
                    continue;
                }

                let with_extender = extension.clone().with_extender(complex.clone());
                let existing_extension = sources.get(&complex);
                if let Some(existing_extension) = existing_extension.cloned() {
                    sources.get_mut(&complex).replace(
                        &mut MergedExtension::merge(existing_extension.clone(), with_extender)
                            .unwrap(),
                    );
                } else {
                    sources
                        .get_mut(&complex)
                        .replace(&mut with_extender.clone());

                    for component in complex.components.clone() {
                        if let ComplexSelectorComponent::Compound(component) = component {
                            for simple in component.components {
                                self.extensions_by_extender
                                    .entry(simple)
                                    .or_insert_with(Vec::new)
                                    .push(with_extender.clone());
                            }
                        }
                    }

                    if new_extensions.contains_key(&extension.target.clone().unwrap()) {
                        additional_extensions
                            .get_or_insert_with(HashMap::new)
                            .entry(extension.target.clone().unwrap())
                            .or_insert_with(IndexMap::new)
                            .insert(complex.clone(), with_extender.clone());
                    }
                }
            }
            // If `selectors` doesn't contain `extension.extender`, for example if it
            // was replaced due to :not() expansion, we must get rid of the old
            // version.
            if !contains_extension {
                sources.remove(&extension.extender);
            }
        }
        additional_extensions
    }

    /// Extend `extensions` using `new_extensions`.
    fn extend_existing_selectors(
        &mut self,
        selectors: HashSet<SelectorList>,
        new_extensions: &HashMap<SimpleSelector, IndexMap<ComplexSelector, Extension>>,
    ) {
        for mut selector in selectors {
            let old_value = selector.clone();
            selector = self.extend_list(
                old_value.clone(),
                new_extensions,
                &self.media_contexts.get(&selector).cloned(),
            );
            /*
            todo: error handling
            } on SassException catch (error) {
            throw SassException(
                "From ${selector.span.message('')}\n"
                "${error.message}",
                error.span);
            }

            */

            // If no extends actually happened (for example becaues unification
            // failed), we don't need to re-register the selector.
            if old_value == selector {
                continue;
            }
            self.register_selector(selector.clone(), &old_value);
        }
    }
}

/// Rotates the element in list from `start` (inclusive) to `end` (exclusive)
/// one index higher, looping the final element back to `start`.
fn rotate_slice<T: Clone>(list: &mut VecDeque<T>, start: usize, end: usize) {
    let mut element = list.get(end - 1).unwrap().clone();
    for i in start..end {
        let next = list.get(i).unwrap().clone();
        list[i] = element;
        element = next;
    }
}

/// Like `HashMap::extend`, but for two-layer maps.
///
/// This avoids copying inner maps from `source` if possible.
fn map_add_all_2<K1: Hash + Eq, K2: Hash + Eq, V>(
    destination: &mut HashMap<K1, IndexMap<K2, V>>,
    source: HashMap<K1, IndexMap<K2, V>>,
) {
    source.into_iter().for_each(|(key, mut inner)| {
        if destination.contains_key(&key) {
            destination
                .get_mut(&key)
                .get_or_insert(&mut IndexMap::new())
                .extend(inner);
        } else {
            destination.get_mut(&key).replace(&mut inner);
        }
    })
}
