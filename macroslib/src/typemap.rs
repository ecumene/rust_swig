pub mod ast;
mod parse;
pub mod ty;
pub mod utils;

use std::{cell::RefCell, fmt, mem, rc::Rc};

use log::{debug, log_enabled, trace, warn};
use petgraph::{
    graph::{EdgeIndex, NodeIndex},
    Graph,
};
use proc_macro2::{Span, TokenStream};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use smol_str::SmolStr;
use syn::{parse_quote, spanned::Spanned, Ident, Type};

use crate::{
    error::{DiagnosticError, Result},
    typemap::{
        ast::{
            check_if_smart_pointer_return_inner_type, get_trait_bounds, normalize_ty_lifetimes,
            DisplayToTokens, GenericTypeConv,
        },
        ty::{RustType, RustTypeS},
    },
    ForeignEnumInfo, ForeignerClassInfo,
};

pub(crate) static TO_VAR_TEMPLATE: &str = "{to_var}";
pub(crate) static FROM_VAR_TEMPLATE: &str = "{from_var}";
pub(in crate::typemap) static TO_VAR_TYPE_TEMPLATE: &str = "{to_var_type}";
pub(in crate::typemap) static FUNCTION_RETURN_TYPE_TEMPLATE: &str = "{function_ret_type}";
const MAX_TRY_BUILD_PATH_STEPS: usize = 7;

#[derive(Debug, Clone)]
pub(crate) struct TypeConvEdge {
    code_template: String,
    dependency: Rc<RefCell<Option<TokenStream>>>,
}

impl From<String> for TypeConvEdge {
    fn from(x: String) -> Self {
        TypeConvEdge {
            code_template: x,
            dependency: Rc::new(RefCell::new(None)),
        }
    }
}

impl TypeConvEdge {
    fn new(code_template: String, dependency: Option<TokenStream>) -> TypeConvEdge {
        TypeConvEdge {
            code_template,
            dependency: Rc::new(RefCell::new(dependency)),
        }
    }
}

pub(crate) type TypeGraphIdx = u32;
pub(crate) type TypesConvGraph = Graph<RustType, TypeConvEdge, petgraph::Directed, TypeGraphIdx>;

pub(crate) trait ForeignMethodSignature {
    type FI: AsRef<ForeignTypeInfo>;
    fn output(&self) -> &ForeignTypeInfo;
    fn input(&self) -> &[Self::FI];
}

type RustTypeNameToGraphIdx = FxHashMap<SmolStr, NodeIndex<TypeGraphIdx>>;

#[derive(Debug)]
pub(crate) struct TypeMap {
    conv_graph: TypesConvGraph,
    foreign_names_map: FxHashMap<SmolStr, NodeIndex<TypeGraphIdx>>,
    rust_names_map: RustTypeNameToGraphIdx,
    utils_code: Vec<syn::Item>,
    generic_edges: Vec<GenericTypeConv>,
    rust_to_foreign_cache: FxHashMap<SmolStr, SmolStr>,
    foreign_classes: Vec<ForeignerClassInfo>,
    exported_enums: FxHashMap<SmolStr, ForeignEnumInfo>,
    traits_usage_code: FxHashMap<Ident, String>,
}

impl Default for TypeMap {
    fn default() -> Self {
        let generic_params: syn::Generics = parse_quote! { <T> };
        let default_rules = vec![
            GenericTypeConv {
                code_template: "let mut {to_var}: {to_var_type} = &{from_var};".into(),
                ..GenericTypeConv::simple_new(
                    parse_type! { T },
                    parse_type! { &T },
                    generic_params.clone(),
                )
            },
            GenericTypeConv {
                code_template: "let mut {to_var}: {to_var_type} = &mut {from_var};".into(),
                ..GenericTypeConv::simple_new(
                    parse_type! { T },
                    parse_type! { &mut T },
                    generic_params.clone(),
                )
            },
            GenericTypeConv {
                code_template: "let mut {to_var}: {to_var_type} = {from_var};".into(),
                ..GenericTypeConv::simple_new(
                    parse_type! { &mut T },
                    parse_type! { &T },
                    generic_params.clone(),
                )
            },
            GenericTypeConv {
                code_template: "let mut {to_var}: {to_var_type} = {from_var}.as_ref();".into(),
                ..GenericTypeConv::simple_new(
                    parse_type! { & Box<T> },
                    parse_type! { &T },
                    generic_params.clone(),
                )
            },
            GenericTypeConv {
                code_template: "let mut {to_var}: {to_var_type} = {from_var}.as_mut();".into(),
                ..GenericTypeConv::simple_new(
                    parse_type! { & mut Box<T> },
                    parse_type! { &mut T },
                    generic_params,
                )
            },
        ];
        TypeMap {
            conv_graph: TypesConvGraph::new(),
            foreign_names_map: FxHashMap::default(),
            rust_names_map: FxHashMap::default(),
            utils_code: Vec::new(),
            generic_edges: default_rules,
            rust_to_foreign_cache: FxHashMap::default(),
            foreign_classes: Vec::new(),
            exported_enums: FxHashMap::default(),
            traits_usage_code: FxHashMap::default(),
        }
    }
}

struct DisplayTypesConvGraph<'a>(&'a TypesConvGraph);

impl<'a> fmt::Display for DisplayTypesConvGraph<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> std::result::Result<(), fmt::Error> {
        let conv_graph = self.0;
        writeln!(f, "conversation graph begin").unwrap();
        for node in conv_graph.node_indices() {
            write!(f, "node {}: ", conv_graph[node].normalized_name)?;
            let mut edges = conv_graph
                .neighbors_directed(node, petgraph::Outgoing)
                .detach();
            while let Some((_, target)) = edges.next(conv_graph) {
                write!(f, "->{}, ", conv_graph[target].normalized_name)?;
            }
            writeln!(f)?;
        }
        writeln!(f, "conversation graph end").unwrap();
        Ok(())
    }
}

#[derive(Debug)]
struct PossiblePath {
    path_len: usize,
    new_edges: Vec<(RustType, RustType, TypeConvEdge)>,
}

impl PossiblePath {
    fn len(&self) -> usize {
        self.path_len
    }
    fn new(graph_snapshot: TypeGraphSnapshot, path: Vec<EdgeIndex<TypeGraphIdx>>) -> Self {
        let mut new_edges = Vec::with_capacity(path.len());
        for edge in &path {
            if graph_snapshot.new_edges.iter().any(|x| *x == *edge) {
                let (from, to) = graph_snapshot
                    .conv_graph
                    .edge_endpoints(*edge)
                    .expect("Internal error: PossiblePath::new no edge");
                let conv_rule = graph_snapshot
                    .conv_graph
                    .edge_weight(*edge)
                    .expect("Internal error: PossiblePath::new no edge")
                    .clone();
                new_edges.push((
                    graph_snapshot.conv_graph[from].clone(),
                    graph_snapshot.conv_graph[to].clone(),
                    conv_rule,
                ));
            }
        }
        PossiblePath {
            path_len: path.len(),
            new_edges,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ForeignTypeInfo {
    pub name: SmolStr,
    pub correspoding_rust_type: RustType,
}

impl AsRef<ForeignTypeInfo> for ForeignTypeInfo {
    fn as_ref(&self) -> &ForeignTypeInfo {
        self
    }
}

struct TypeGraphSnapshot<'a> {
    conv_graph: &'a mut TypesConvGraph,
    rust_names_map: &'a RustTypeNameToGraphIdx,
    new_nodes_names_map: RustTypeNameToGraphIdx,
    new_nodes: SmallVec<[NodeIndex<TypeGraphIdx>; 32]>,
    new_edges: SmallVec<[EdgeIndex<TypeGraphIdx>; 32]>,
}

impl<'a> TypeGraphSnapshot<'a> {
    fn new(conv_graph: &'a mut TypesConvGraph, rust_names_map: &'a RustTypeNameToGraphIdx) -> Self {
        TypeGraphSnapshot {
            conv_graph,
            rust_names_map,
            new_nodes: SmallVec::new(),
            new_nodes_names_map: RustTypeNameToGraphIdx::default(),
            new_edges: SmallVec::new(),
        }
    }

    fn node_for_ty(&mut self, (ty, ty_name): (syn::Type, SmolStr)) -> NodeIndex<TypeGraphIdx> {
        let graph = &mut self.conv_graph;
        let mut new_node = false;
        let idx = if let Some(idx) = self.rust_names_map.get(&ty_name) {
            *idx
        } else {
            let names_to_graph_map = &mut self.new_nodes_names_map;
            *names_to_graph_map
                .entry(ty_name.clone())
                .or_insert_with(|| {
                    new_node = true;
                    let idx =
                        graph.add_node(Rc::new(RustTypeS::new_without_graph_idx(ty, ty_name)));
                    Rc::get_mut(&mut graph[idx])
                        .expect("Internal error: can not modify Rc")
                        .graph_idx = idx;
                    idx
                })
        };
        if new_node {
            self.new_nodes.push(idx);
        }
        idx
    }

    fn find_type_by_name(&self, type_name: &str) -> Option<&RustType> {
        self.rust_names_map
            .get(type_name)
            .map(|i| &self.conv_graph[*i])
            .or_else(|| {
                self.new_nodes_names_map
                    .get(type_name)
                    .map(|i| &self.conv_graph[*i])
            })
    }

    fn add_edge(
        &mut self,
        from: NodeIndex<TypeGraphIdx>,
        to: NodeIndex<TypeGraphIdx>,
        edge: TypeConvEdge,
    ) {
        if self.conv_graph.find_edge(from, to).is_none() {
            let edge_idx = self.conv_graph.add_edge(from, to, edge);
            self.new_edges.push(edge_idx);
        }
    }
}

impl<'a> Drop for TypeGraphSnapshot<'a> {
    fn drop(&mut self) {
        for edge in self.new_edges.iter().rev() {
            self.conv_graph.remove_edge(*edge);
        }
        for idx in self.new_nodes.iter().rev() {
            self.conv_graph.remove_node(*idx);
        }
    }
}

impl TypeMap {
    pub(crate) fn is_empty(&self) -> bool {
        self.conv_graph.node_count() == 0
    }

    pub(crate) fn take_utils_code(&mut self) -> Vec<syn::Item> {
        let mut ret = Vec::new();
        ret.append(&mut self.utils_code);
        ret
    }

    pub(crate) fn add_foreign(&mut self, correspoding_rty: RustType, foreign_name: SmolStr) {
        self.foreign_names_map
            .insert(foreign_name, correspoding_rty.graph_idx);
    }
    pub(crate) fn find_foreign_type_info_by_name(
        &self,
        foreign_name: &str,
    ) -> Option<ForeignTypeInfo> {
        self.foreign_names_map
            .get(foreign_name)
            .map(|x| ForeignTypeInfo {
                name: foreign_name.into(),
                correspoding_rust_type: self.conv_graph[*x].clone(),
            })
    }

    pub(crate) fn cache_rust_to_foreign_conv(&mut self, from: &RustType, to: ForeignTypeInfo) {
        let to_id = to.correspoding_rust_type.graph_idx;
        self.rust_to_foreign_cache
            .insert(from.normalized_name.clone(), to.name.clone());
        self.foreign_names_map.insert(to.name, to_id);
    }

    pub(crate) fn convert_to_heap_pointer(
        &mut self,
        from: &RustType,
        var_name: &str,
    ) -> (RustType, String) {
        for smart_pointer in &["Box", "Rc", "Arc"] {
            if let Some(inner_ty) = check_if_smart_pointer_return_inner_type(from, *smart_pointer) {
                let inner_ty: RustType = self.find_or_alloc_rust_type(&inner_ty);
                let code = format!(
                    r#"
    let {var_name}: *const {inner_ty} = {smart_pointer}::into_raw({var_name});
"#,
                    var_name = var_name,
                    inner_ty = inner_ty.normalized_name,
                    smart_pointer = *smart_pointer,
                );
                return (inner_ty, code);
            }
        }

        let inner_ty = from.clone();
        let inner_ty_str = normalize_ty_lifetimes(&inner_ty.ty);
        (
            inner_ty,
            format!(
                r#"
    let {var_name}: Box<{inner_ty}> = Box::new({var_name});
    let {var_name}: *mut {inner_ty} = Box::into_raw({var_name});
"#,
                var_name = var_name,
                inner_ty = inner_ty_str
            ),
        )
    }

    pub(crate) fn unpack_from_heap_pointer(
        from: &RustType,
        var_name: &str,
        unbox_if_boxed: bool,
    ) -> String {
        for smart_pointer in &["Box", "Rc", "Arc"] {
            if check_if_smart_pointer_return_inner_type(from, *smart_pointer).is_some() {
                return format!(
                    r#"
    let {var_name}: {rc_type}  = unsafe {{ {smart_pointer}::from_raw({var_name}) }};
"#,
                    var_name = var_name,
                    rc_type = from.normalized_name,
                    smart_pointer = *smart_pointer,
                );
            }
        }
        let unbox_code = if unbox_if_boxed {
            format!(
                r#"
    let {var_name}: {inside_box_type} = *{var_name};
"#,
                var_name = var_name,
                inside_box_type = from.normalized_name
            )
        } else {
            String::new()
        };
        format!(
            r#"
    let {var_name}: Box<{inside_box_type}> = unsafe {{ Box::from_raw({var_name}) }};
{unbox_code}
"#,
            var_name = var_name,
            inside_box_type = from.normalized_name,
            unbox_code = unbox_code
        )
    }

    pub(crate) fn is_ty_implements(&self, ty: &RustType, trait_name: &str) -> Option<RustType> {
        if ty.implements.contains(trait_name) {
            Some(ty.clone())
        } else {
            if let syn::Type::Reference(syn::TypeReference { ref elem, .. }) = ty.ty {
                let ty_name = normalize_ty_lifetimes(&*elem);
                self.rust_names_map.get(ty_name).and_then(|idx| {
                    if self.conv_graph[*idx].implements.contains(trait_name) {
                        Some(self.conv_graph[*idx].clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        }
    }

    pub(crate) fn find_foreigner_class_with_such_self_type(
        &self,
        may_be_self_ty: &RustType,
        if_ref_search_reftype: bool,
    ) -> Option<&ForeignerClassInfo> {
        let type_name = match may_be_self_ty.ty {
            syn::Type::Reference(syn::TypeReference { ref elem, .. }) if if_ref_search_reftype => {
                normalize_ty_lifetimes(&*elem)
            }
            _ => may_be_self_ty.normalized_name.as_str(),
        };

        trace!("find self type: possible name {}", type_name);
        for fc in &self.foreign_classes {
            let self_rust_ty = self
                .ty_to_rust_type(&fc.self_type_as_ty())
                .unwrap_or_else(|| {
                    panic!(
                        "Internal error: self_type ({}) not registered",
                        DisplayToTokens(&fc.self_type_as_ty())
                    )
                });
            trace!("self_type {}", self_rust_ty);
            if self_rust_ty.normalized_name == type_name {
                return Some(fc);
            }
        }
        None
    }

    pub(crate) fn add_conversation_rule(
        &mut self,
        from: RustType,
        to: RustType,
        rule: TypeConvEdge,
    ) {
        debug!(
            "TypesConvMap::add_conversation_rule {} -> {}: {:?}",
            from, to, rule
        );
        let from = from.graph_idx;
        let to = to.graph_idx;
        self.conv_graph.update_edge(from, to, rule);
    }

    pub(crate) fn register_exported_enum(&mut self, enum_info: &ForeignEnumInfo) {
        self.exported_enums
            .insert(enum_info.name.to_string().into(), enum_info.clone());
    }

    pub(crate) fn is_this_exported_enum(&self, ty: &RustType) -> Option<&ForeignEnumInfo> {
        self.exported_enums.get(&ty.normalized_name)
    }

    pub(crate) fn is_generated_foreign_type(&self, foreign_name: &str) -> bool {
        if self.exported_enums.contains_key(foreign_name) {
            return true;
        }
        self.foreign_classes
            .iter()
            .any(|fc| fc.name == foreign_name)
    }

    pub(crate) fn convert_rust_types(
        &mut self,
        from: &RustType,
        to: &RustType,
        var_name: &str,
        function_ret_type: &str,
        build_for_sp: Span,
    ) -> Result<(Vec<TokenStream>, String)> {
        let path = match self.find_path(from, to, build_for_sp) {
            Ok(x) => x,
            Err(_err) => {
                debug!("convert_rust_types: no path, trying to build it");
                self.build_path_if_possible(from, to, build_for_sp);
                self.find_path(from, to, build_for_sp)?
            }
        };
        let mut ret_code = String::new();
        let mut code_deps = Vec::<TokenStream>::new();

        for edge in path {
            let (_, target) = self.conv_graph.edge_endpoints(edge).unwrap();
            let target_type = self.conv_graph[target].normalized_name.clone();
            let edge = &mut self.conv_graph[edge];
            if let Some(dep) = edge.dependency.borrow_mut().take() {
                code_deps.push(dep);
            }
            let code = apply_code_template(
                &edge.code_template,
                var_name,
                var_name,
                &unpack_unique_typename(&target_type),
                function_ret_type,
            );
            ret_code.push_str(&code);
        }
        Ok((code_deps, ret_code))
    }

    fn find_path(
        &self,
        from: &RustType,
        to: &RustType,
        build_for_sp: Span,
    ) -> Result<Vec<EdgeIndex<TypeGraphIdx>>> {
        debug!("find_path: begin {} -> {}", from, to);
        if from.normalized_name == to.normalized_name {
            return Ok(vec![]);
        }
        find_conversation_path(&self.conv_graph, from.graph_idx, to.graph_idx, build_for_sp)
    }

    fn build_path_if_possible(
        &mut self,
        start_from: &RustType,
        goal_to: &RustType,
        build_for_sp: Span,
    ) {
        debug!(
            "build_path_if_possible begin {}\n {} -> {}",
            DisplayTypesConvGraph(&self.conv_graph),
            start_from,
            goal_to
        );
        if let Some(path) = try_build_path(
            start_from,
            goal_to,
            build_for_sp,
            &mut self.conv_graph,
            &self.rust_names_map,
            &self.generic_edges,
            MAX_TRY_BUILD_PATH_STEPS,
        ) {
            merge_path_to_conv_map(path, self);
        }
    }

    pub(crate) fn merge(
        &mut self,
        id_of_code: &str,
        code: &str,
        target_pointer_width: usize,
    ) -> Result<()> {
        debug!("merging {} with our rules", id_of_code);
        let mut was_traits_usage_code = FxHashMap::default();
        mem::swap(&mut was_traits_usage_code, &mut self.traits_usage_code);
        let mut new_data = parse::parse(
            id_of_code,
            code,
            target_pointer_width,
            was_traits_usage_code,
        )?;
        mem::swap(&mut new_data.traits_usage_code, &mut self.traits_usage_code);

        fn get_graph_node_idx(
            node_new_data_idx: NodeIndex<TypeGraphIdx>,
            new_data: &TypeMap,
            data: &mut TypeMap,
        ) -> NodeIndex<TypeGraphIdx> {
            let node = &new_data.conv_graph[node_new_data_idx];
            debug!("handling new node {:?}", node);
            let node2 = node.clone();
            let data_rust_names_map = &mut data.rust_names_map;
            let data_conv_graph = &mut data.conv_graph;
            let idx = *data_rust_names_map
                .entry(node.normalized_name.clone())
                .or_insert_with(|| {
                    let idx = data_conv_graph.add_node(node2);
                    Rc::make_mut(&mut data_conv_graph[idx]).graph_idx = idx;
                    idx
                });

            Rc::make_mut(&mut data_conv_graph[idx]).merge(node);

            if let Some((foreign_name, _)) = new_data
                .foreign_names_map
                .iter()
                .find(|x| *x.1 == node_new_data_idx)
            {
                data.foreign_names_map.insert(foreign_name.clone(), idx);
            }
            idx
        }

        fn process_new_node(
            new_data_idx: NodeIndex<TypeGraphIdx>,
            new_data: &TypeMap,
            data: &mut TypeMap,
        ) {
            let self_src = get_graph_node_idx(new_data_idx, new_data, data);
            let mut edges = new_data
                .conv_graph
                .neighbors_directed(new_data_idx, petgraph::Outgoing)
                .detach();
            while let Some((edge, target)) = edges.next(&new_data.conv_graph) {
                let self_target = get_graph_node_idx(target, new_data, data);

                if let Some(existing_edge) = data.conv_graph.find_edge(self_src, self_target) {
                    warn!(
                        "typemap merge: Converstation {:?} from {:?} to {:?} ignored, we use {:?} instead",
                        new_data.conv_graph[edge],
                        self_src,
                        self_target,
                        data.conv_graph[existing_edge]
                    );
                } else {
                    data.conv_graph.add_edge(
                        self_src,
                        self_target,
                        new_data.conv_graph[edge].clone(),
                    );
                }
            }
        }

        for node in new_data.conv_graph.node_indices() {
            process_new_node(node, &new_data, self);
        }
        self.utils_code.append(&mut new_data.utils_code);
        //TODO: more intellect to process new generics
        self.generic_edges.append(&mut new_data.generic_edges);
        Ok(())
    }

    /// find correspoint to rust foreign type
    pub(crate) fn map_through_conversation_to_foreign<
        F: Fn(&TypeMap, &ForeignerClassInfo) -> Option<Type>,
    >(
        &mut self,
        rust_ty: &RustType,
        direction: petgraph::Direction,
        build_for_sp: Span,
        calc_this_type_for_method: F,
    ) -> Option<ForeignTypeInfo> {
        debug!("map foreign: {} {:?}", rust_ty, direction);

        if direction == petgraph::Direction::Outgoing {
            if let Some(foreign_name) = self.rust_to_foreign_cache.get(&rust_ty.normalized_name) {
                if let Some(to) = self.foreign_names_map.get(foreign_name) {
                    let to = &self.conv_graph[*to];
                    return Some(ForeignTypeInfo {
                        name: foreign_name.clone(),
                        correspoding_rust_type: to.clone(),
                    });
                }
            }
        }

        {
            debug!(
                "map foreign: graph node {:?}",
                self.conv_graph[rust_ty.graph_idx]
            );
            let find_path = |from, to| match find_conversation_path(
                &self.conv_graph,
                from,
                to,
                Span::call_site(),
            ) {
                Ok(x) => Some(x),
                Err(_) => None,
            };
            let mut min_path: Option<(usize, NodeIndex, SmolStr)> = None;
            for (foreign_name, graph_idx) in &self.foreign_names_map {
                let path = match direction {
                    petgraph::Direction::Outgoing => find_path(rust_ty.graph_idx, *graph_idx),
                    petgraph::Direction::Incoming => find_path(*graph_idx, rust_ty.graph_idx),
                };
                if let Some(path) = path {
                    trace!(
                        "map foreign: we find path {} <-> {}",
                        foreign_name,
                        self.conv_graph[*graph_idx]
                    );
                    let cur: (usize, NodeIndex, SmolStr) =
                        (path.len(), *graph_idx, foreign_name.clone());
                    min_path = Some(if let Some(x) = min_path {
                        if cur.0 < x.0 {
                            cur
                        } else {
                            x
                        }
                    } else {
                        cur
                    });
                }
            }
            if let Some(min_path) = min_path {
                let node = &self.conv_graph[min_path.1];

                debug!(
                    "map foreign: we found min path {} <-> {}",
                    rust_ty, min_path.2
                );

                return Some(ForeignTypeInfo {
                    name: min_path.2,
                    correspoding_rust_type: node.clone(),
                });
            }
        }

        debug!(
            "map foreign: No paths exists, may be we can create one for '{}' {:?}?",
            rust_ty, direction
        );

        let mut new_foreign_types = FxHashSet::default();
        for edge in &self.generic_edges {
            if let Some(ref to_foreigner_hint) = edge.to_foreigner_hint {
                let trait_bounds = get_trait_bounds(&edge.generic_params);
                for graph_idx in self.rust_names_map.values() {
                    for trait_bound in &trait_bounds {
                        let rust_ty = &self.conv_graph[*graph_idx];
                        if rust_ty.implements.contains_subset(&trait_bound.trait_names) {
                            if let Some(class) = self.find_foreigner_class_with_such_this_type(
                                &rust_ty.ty,
                                &calc_this_type_for_method,
                            ) {
                                let ty_param_name = trait_bound.ty_param.as_ref().to_string();
                                let suffix = to_foreigner_hint
                                    .as_str()
                                    .replace(&ty_param_name, &*rust_ty.normalized_name.as_str());
                                let foreign_name = to_foreigner_hint
                                    .as_str()
                                    .replace(&ty_param_name, &*class.name.to_string());
                                new_foreign_types.insert((
                                    edge.to_ty.clone(),
                                    suffix,
                                    foreign_name,
                                ));
                            } else {
                                warn!("No foreign_class for type '{}'", rust_ty.normalized_name);
                            }
                        }
                    }
                }
            }
        }
        for (ty, suffix, foreign_name) in new_foreign_types {
            debug!(
                "map foreign: add possible type {} {} <-> {}",
                DisplayToTokens(&ty),
                suffix,
                foreign_name
            );
            let rust_ty = self.find_or_alloc_rust_type_with_suffix(&ty, &suffix);
            self.foreign_names_map
                .insert(foreign_name.into(), rust_ty.graph_idx);
        }

        let from: RustType = rust_ty.clone().into();
        let mut possible_paths = Vec::<(PossiblePath, SmolStr, NodeIndex)>::new();
        for max_steps in 1..=MAX_TRY_BUILD_PATH_STEPS {
            for (foreign_name, graph_idx) in &self.foreign_names_map {
                let other = self.conv_graph[*graph_idx].clone();
                let path = match direction {
                    petgraph::Direction::Outgoing => try_build_path(
                        &from,
                        &other,
                        build_for_sp,
                        &mut self.conv_graph,
                        &self.rust_names_map,
                        &self.generic_edges,
                        max_steps,
                    ),
                    petgraph::Direction::Incoming => try_build_path(
                        &other,
                        &from,
                        build_for_sp,
                        &mut self.conv_graph,
                        &self.rust_names_map,
                        &self.generic_edges,
                        max_steps,
                    ),
                };
                if let Some(path) = path {
                    possible_paths.push((path, foreign_name.clone(), *graph_idx));
                }
            }
            if !possible_paths.is_empty() {
                break;
            }
        }
        let ret = possible_paths.into_iter().min_by_key(|pp| pp.0.len()).map(
            |(pp, foreign_name, graph_idx)| {
                merge_path_to_conv_map(pp, self);
                let node = &self.conv_graph[graph_idx];
                ForeignTypeInfo {
                    name: foreign_name,
                    correspoding_rust_type: node.clone(),
                }
            },
        );
        if ret.is_none() {
            debug!(
                "map to foreign failed, foreign_map {:?}\n conv_graph: {}",
                self.foreign_names_map,
                DisplayTypesConvGraph(&self.conv_graph),
            );
        }
        ret
    }

    pub(crate) fn find_foreigner_class_with_such_this_type<
        F: Fn(&TypeMap, &ForeignerClassInfo) -> Option<Type>,
    >(
        &self,
        this_ty: &Type,
        get_this_type: F,
    ) -> Option<&ForeignerClassInfo> {
        let this_name = normalize_ty_lifetimes(this_ty);
        for fc in &self.foreign_classes {
            if let Some(this_type_for_method) = get_this_type(self, fc) {
                let cur_this = normalize_ty_lifetimes(&this_type_for_method);
                if cur_this == this_name {
                    return Some(fc);
                }
            }
        }
        None
    }

    pub(crate) fn register_foreigner_class(&mut self, class: &ForeignerClassInfo) {
        self.foreign_classes.push(class.clone());
    }

    fn add_node<F: FnOnce() -> RustTypeS>(
        &mut self,
        key: SmolStr,
        init_without_graph_idx: F,
    ) -> NodeIndex {
        let rust_names_map = &mut self.rust_names_map;
        let conv_graph = &mut self.conv_graph;
        *rust_names_map.entry(key).or_insert_with(|| {
            let idx = conv_graph.add_node(Rc::new(init_without_graph_idx()));
            Rc::get_mut(&mut conv_graph[idx])
                .expect("Internal error: can not modify Rc")
                .graph_idx = idx;
            idx
        })
    }

    pub(crate) fn find_or_alloc_rust_type(&mut self, ty: &Type) -> RustType {
        let name = normalize_ty_lifetimes(ty);
        let idx = self.add_node(name.into(), || {
            RustTypeS::new_without_graph_idx(ty.clone(), name)
        });
        self.conv_graph[idx].clone()
    }

    pub(crate) fn find_or_alloc_rust_type_that_implements(
        &mut self,
        ty: &Type,
        trait_name: &str,
    ) -> RustType {
        let name = normalize_ty_lifetimes(ty);
        let idx = self.add_node(name.into(), || {
            RustTypeS::new_without_graph_idx(ty.clone(), name).implements(trait_name)
        });
        self.conv_graph[idx].clone()
    }

    pub(crate) fn find_or_alloc_rust_type_with_suffix(
        &mut self,
        ty: &Type,
        suffix: &str,
    ) -> RustType {
        let name: SmolStr = make_unique_rust_typename(normalize_ty_lifetimes(ty), suffix).into();
        let idx = self.add_node(name.clone(), || {
            RustTypeS::new_without_graph_idx(ty.clone(), name)
        });
        self.conv_graph[idx].clone()
    }

    pub(crate) fn find_or_alloc_rust_type_with_may_be_suffix(
        &mut self,
        ty: &Type,
        suffix: Option<String>,
    ) -> RustType {
        if let Some(suffix) = suffix {
            self.find_or_alloc_rust_type_with_suffix(ty, &suffix)
        } else {
            self.find_or_alloc_rust_type(ty)
        }
    }

    pub(crate) fn ty_to_rust_type(&self, ty: &Type) -> Option<RustType> {
        let name = normalize_ty_lifetimes(ty);
        self.rust_names_map
            .get(name)
            .map(|idx| self.conv_graph[*idx].clone())
    }

    pub(crate) fn find_rust_type_with_suffix(&self, ty: &Type, suffix: &str) -> Option<RustType> {
        let name: SmolStr = make_unique_rust_typename(normalize_ty_lifetimes(ty), suffix).into();
        self.rust_names_map
            .get(&name)
            .map(|idx| self.conv_graph[*idx].clone())
    }
}

pub(in crate::typemap) fn validate_code_template(sp: Span, code: &str) -> Result<()> {
    if code.contains(TO_VAR_TEMPLATE)
        && code.contains(FROM_VAR_TEMPLATE)
        && code.contains(TO_VAR_TYPE_TEMPLATE)
    {
        Ok(())
    } else {
        Err(DiagnosticError::new(
            sp,
            format!(
                "{} not contains one of {}, {}, {}",
                code, TO_VAR_TEMPLATE, FROM_VAR_TEMPLATE, TO_VAR_TYPE_TEMPLATE
            ),
        ))
    }
}

pub(crate) fn make_unique_rust_typename(
    not_unique_name: &str,
    suffix_to_make_unique: &str,
) -> String {
    format!("{}{}{}", not_unique_name, 0 as char, suffix_to_make_unique)
}

pub(crate) fn make_unique_rust_typename_if_need(
    rust_typename: String,
    suffix: Option<String>,
) -> String {
    match suffix {
        Some(s) => make_unique_rust_typename(&rust_typename, &s),
        None => rust_typename,
    }
}

pub(crate) fn unpack_unique_typename(name: &str) -> &str {
    match name.find('\0') {
        Some(pos) => &name[0..pos],
        None => name,
    }
}

fn apply_code_template(
    code_temlate: &str,
    to_name: &str,
    from_name: &str,
    to_typename: &str,
    func_ret_type: &str,
) -> String {
    let mut ret = String::new();
    ret.push_str("    ");
    ret.push_str(code_temlate);
    ret.push('\n');
    ret.replace(TO_VAR_TEMPLATE, to_name)
        .replace(FROM_VAR_TEMPLATE, from_name)
        .replace(TO_VAR_TYPE_TEMPLATE, to_typename)
        .replace(FUNCTION_RETURN_TYPE_TEMPLATE, func_ret_type)
}

fn find_conversation_path(
    conv_graph: &TypesConvGraph,
    from: NodeIndex<TypeGraphIdx>,
    to: NodeIndex<TypeGraphIdx>,
    build_for_sp: Span,
) -> Result<Vec<EdgeIndex<TypeGraphIdx>>> {
    trace!(
        "find_conversation_path: begin {} -> {}",
        conv_graph[from],
        conv_graph[to]
    );

    if let Some((_, nodes_path)) = petgraph::algo::astar(
        conv_graph,
        from,
        |idx| idx == to,
        |_| 1,
        |idx| if idx != from { 1 } else { 0 },
    ) {
        let mut edges = Vec::with_capacity(nodes_path.len());
        for (cur_node, next_node) in nodes_path.iter().zip(nodes_path.iter().skip(1)) {
            edges.push(
                conv_graph
                    .find_edge(*cur_node, *next_node)
                    .expect("Internal error: find_conversation_path no edge"),
            );
        }
        Ok(edges)
    } else {
        let mut err = DiagnosticError::new(
            conv_graph[from].ty.span(),
            format!("Can not find conversation from type '{}'", conv_graph[from]),
        );
        err.span_note(
            conv_graph[to].ty.span(),
            format!("to type '{}'", conv_graph[to]),
        );
        err.span_note(build_for_sp, "In this context");
        Err(err)
    }
}

fn merge_path_to_conv_map(path: PossiblePath, conv_map: &mut TypeMap) {
    let PossiblePath {
        new_edges,
        path_len: _,
    } = path;

    for (from, to, conv_rule) in new_edges {
        let from_idx = conv_map.add_node(from.normalized_name.clone(), || (*from).clone());
        let to_idx = conv_map.add_node(to.normalized_name.clone(), || (*to).clone());
        assert!(conv_map.conv_graph.find_edge(from_idx, to_idx).is_none());
        conv_map.conv_graph.add_edge(from_idx, to_idx, conv_rule);
    }
}

fn try_build_path(
    start_from: &RustType,
    goal_to: &RustType,
    build_for_sp: Span,
    conv_graph: &mut TypesConvGraph,
    rust_names_map: &RustTypeNameToGraphIdx,
    generic_edges: &[GenericTypeConv],
    max_steps: usize,
) -> Option<PossiblePath> {
    debug!(
        "try_build_path: from {} to {}, ty names len {}, graph nodes {}, edges {}",
        start_from,
        goal_to,
        rust_names_map.len(),
        conv_graph.node_count(),
        conv_graph.edge_count()
    );
    let mut ty_graph = TypeGraphSnapshot::new(conv_graph, &rust_names_map);

    let start_from_idx = start_from.graph_idx;

    let goal_to_idx = goal_to.graph_idx;

    let mut cur_step = FxHashSet::default();
    cur_step.insert(start_from_idx);
    let mut next_step = FxHashSet::default();

    for step in 0..max_steps {
        debug!("try_build_path: do step {}", step);
        if cur_step.is_empty() {
            break;
        }
        if log_enabled!(log::Level::Debug) {
            use std::fmt::Write;
            let mut step_types = String::new();
            for from_ty in &cur_step {
                write!(step_types, "{} ", ty_graph.conv_graph[*from_ty]).unwrap();
            }
            debug!("try_build_path: cur_step {}", step_types);
        }
        for from_ty in &cur_step {
            let from: RustType = ty_graph.conv_graph[*from_ty].clone();
            for neighbor in ty_graph
                .conv_graph
                .neighbors_directed(*from_ty, petgraph::Outgoing)
            {
                next_step.insert(neighbor);
            }
            for edge in generic_edges {
                trace!(
                    "try_build_path: we check edge({:?} -> {:?}) for {}",
                    edge.from_ty,
                    edge.to_ty,
                    from
                );
                if let Some((to_ty, to_ty_name)) =
                    edge.is_conv_possible(&from, Some(goal_to), |name| {
                        ty_graph.find_type_by_name(name)
                    })
                {
                    if from.normalized_name == to_ty_name {
                        continue;
                    }
                    let to = ty_graph.node_for_ty((to_ty, to_ty_name));
                    ty_graph.add_edge(
                        *from_ty,
                        to,
                        TypeConvEdge {
                            code_template: edge.code_template.clone(),
                            dependency: edge.dependency.clone(),
                        },
                    );

                    if petgraph::algo::has_path_connecting(
                        &*ty_graph.conv_graph,
                        to,
                        goal_to_idx,
                        None,
                    ) {
                        debug!("try_build_path: NEW ALGO: we found PATH!!!!");
                        let path = find_conversation_path(
                            &ty_graph.conv_graph,
                            start_from_idx,
                            goal_to_idx,
                            build_for_sp,
                        )
                        .expect("path must exists");
                        if log_enabled!(log::Level::Debug) {
                            for edge in &path {
                                if let Some((from, to)) = ty_graph.conv_graph.edge_endpoints(*edge)
                                {
                                    debug!(
                                        "try_build_path: path: {} -> {}",
                                        ty_graph.conv_graph[from], ty_graph.conv_graph[to]
                                    );
                                }
                            }
                        }

                        return Some(PossiblePath::new(ty_graph, path));
                    }
                    next_step.insert(to);
                }
            }
        }
        mem::swap(&mut cur_step, &mut next_step);
        next_step.clear();
    }
    debug!("try_build_path: No results");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge() {
        let mut types_map = TypeMap::default();
        types_map
            .merge(
                "test_merge",
                r#"
mod swig_foreign_types_map {
    #![swig_foreigner_type="boolean"]
    #![swig_rust_type="jboolean"]
    #![swig_foreigner_type="int"]
    #![swig_rust_type="jint"]
}

fn helper1() {
}

#[swig_code = "let mut {to_var}: {to_var_type} = {from_var}.swig_into(env);"]
trait SwigInto<T> {
    fn swig_into(self, env: *mut JNIEnv) -> T;
}

#[swig_code = "let mut {to_var}: {to_var_type} = <{to_var_type}>::swig_from({from_var}, env);"]
trait SwigFrom<T> {
    fn swig_from(T, env: *mut JNIEnv) -> Self;
}

impl SwigInto<bool> for jboolean {
    fn swig_into(self, _: *mut JNIEnv) -> bool {
        self != 0
    }
}

fn helper2() {
}

impl SwigFrom<bool> for jboolean {
    fn swig_from(x: bool, _: *mut JNIEnv) -> Self {
        if x { 1 as jboolean } else { 0 as jboolean }
    }
}
impl SwigFrom<i32> for jint {
    fn swig_from(x: i32, _: *mut JNIEnv) -> Self {
        x
    }
}

fn helper3() {
}
"#,
                64,
            )
            .unwrap();
        assert_eq!(
            {
                let mut set = FxHashSet::default();
                for k in types_map.foreign_names_map.keys() {
                    set.insert(k.clone());
                }
                set
            },
            {
                let mut set = FxHashSet::default();
                set.insert("boolean".into());
                set.insert("int".into());
                set
            }
        );
        let ty_i32 = types_map.find_or_alloc_rust_type(&parse_type! { i32 });
        assert_eq!(
            types_map
                .map_through_conversation_to_foreign(
                    &ty_i32,
                    petgraph::Direction::Outgoing,
                    Span::call_site(),
                    |_, fc| fc.constructor_ret_type.clone(),
                )
                .unwrap()
                .name,
            "int"
        );
        assert_eq!(
            "let mut {to_var}: {to_var_type} = {from_var}.swig_into(env);",
            {
                let from = types_map.rust_names_map["jboolean"];
                let to = types_map.rust_names_map["bool"];
                let conv = &types_map.conv_graph[types_map.conv_graph.find_edge(from, to).unwrap()];
                conv.code_template.clone()
            },
        );

        let from = types_map.rust_names_map["jboolean"];
        let to = types_map.rust_names_map["bool"];
        assert_eq!(
            find_conversation_path(&types_map.conv_graph, from, to, Span::call_site()).unwrap(),
            vec![types_map.conv_graph.find_edge(from, to).unwrap()]
        );

        let from = types_map.rust_names_map["bool"];
        let to = types_map.rust_names_map["jboolean"];
        assert_eq!(
            find_conversation_path(&types_map.conv_graph, from, to, Span::call_site()).unwrap(),
            vec![types_map.conv_graph.find_edge(from, to).unwrap()]
        );
        assert_eq!(
            types_map
                .utils_code
                .iter()
                .filter_map(|v| {
                    let item: syn::Item = v.clone();
                    match item {
                        syn::Item::Fn(ref fun) => Some(fun.ident.to_string()),
                        syn::Item::Trait(ref trait_) => Some(trait_.ident.to_string()),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>(),
            vec!["helper1", "SwigInto", "SwigFrom", "helper2", "helper3"]
        );
    }

    #[test]
    fn test_try_build_path() {
        let _ = env_logger::try_init();
        let mut types_map = TypeMap::default();
        types_map
            .merge(
                "test_try_build_path",
                include_str!("java_jni/jni-include.rs"),
                64,
            )
            .unwrap();

        let foo_rt: RustType = types_map
            .find_or_alloc_rust_type_that_implements(&parse_type! { Foo }, "SwigForeignClass");
        types_map.register_foreigner_class(&ForeignerClassInfo {
            name: Ident::new("Foo", Span::call_site()),
            methods: vec![],
            self_type: None,
            foreigner_code: String::new(),
            constructor_ret_type: Some(foo_rt.ty.clone()),
            doc_comments: vec![],
            copy_derived: false,
        });

        let rc_refcell_foo_ty =
            types_map.find_or_alloc_rust_type(&parse_type! { &mut Rc<RefCell<Foo>> });
        let foo_ref_ty = types_map.find_or_alloc_rust_type(&parse_type! { &mut Foo });

        assert_eq!(
            r#"    let mut a0: & Rc < RefCell < Foo > > = a0;
    let mut a0: & RefCell < Foo > = a0.swig_deref();
    let mut a0: RefMut < Foo > = <RefMut < Foo >>::swig_from(a0, env);
    let mut a0: & mut Foo = a0.swig_deref_mut();
"#,
            types_map
                .convert_rust_types(
                    &rc_refcell_foo_ty,
                    &foo_ref_ty,
                    "a0",
                    "jlong",
                    Span::call_site()
                )
                .expect("path from &mut Rc<RefCell<Foo>> to &mut Foo NOT exists")
                .1,
        );

        let rc_refcell_foo_ty = types_map.find_or_alloc_rust_type(&parse_type! { &RefCell<Foo> });
        let foo_ref_ty = types_map.find_or_alloc_rust_type(&parse_type! { &Foo });

        assert_eq!(
            r#"    let mut a0: Ref < Foo > = <Ref < Foo >>::swig_from(a0, env);
    let mut a0: & Foo = a0.swig_deref();
"#,
            types_map
                .convert_rust_types(
                    &rc_refcell_foo_ty,
                    &foo_ref_ty,
                    "a0",
                    "jlong",
                    Span::call_site()
                )
                .expect("path from &RefCell<Foo> to &Foo NOT exists")
                .1
        );

        let vec_foo_ty = types_map.find_or_alloc_rust_type(&parse_type! { Vec<Foo> });

        assert_eq!(
            "Foo []",
            types_map
                .map_through_conversation_to_foreign(
                    &vec_foo_ty,
                    petgraph::Direction::Outgoing,
                    Span::call_site(),
                    |_, fc| fc.constructor_ret_type.clone(),
                )
                .unwrap()
                .name
        );

        assert!(try_build_path(
            &types_map.find_or_alloc_rust_type(&parse_type! { Vec<i32> }),
            &types_map.find_or_alloc_rust_type(&parse_type! { jlong }),
            Span::call_site(),
            &mut types_map.conv_graph,
            &mut types_map.rust_names_map,
            &types_map.generic_edges,
            MAX_TRY_BUILD_PATH_STEPS,
        )
        .is_none());
    }
}
