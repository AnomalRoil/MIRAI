// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

use crate::abstract_value::AbstractValue;
use crate::environment::Environment;
use crate::expression::{Expression, ExpressionType};
use crate::path::{Path, PathEnum, PathSelector};
use crate::rustc_middle::ty::DefIdTree;
use crate::{type_visitor, utils};

use log_derive::*;
use mirai_annotations::*;
use rustc_hir::def_id::DefId;
use rustc_middle::mir;
use rustc_middle::ty::subst::{GenericArg, GenericArgKind, InternalSubsts, SubstsRef};
use rustc_middle::ty::{
    Const, ConstKind, ExistentialPredicate, ExistentialProjection, ExistentialTraitRef, FnSig,
    ParamTy, Ty, TyCtxt, TyKind, TypeAndMut,
};
use rustc_target::abi::VariantIdx;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter, Result};
use std::rc::Rc;

pub struct TypeVisitor<'tcx> {
    pub actual_argument_types: Vec<Ty<'tcx>>,
    pub def_id: DefId,
    pub generic_argument_map: Option<HashMap<rustc_span::Symbol, GenericArg<'tcx>>>,
    pub generic_arguments: Option<SubstsRef<'tcx>>,
    pub mir: &'tcx mir::Body<'tcx>,
    pub path_ty_cache: HashMap<Rc<Path>, Ty<'tcx>>,
    pub dummy_untagged_value_type: Ty<'tcx>,
    tcx: TyCtxt<'tcx>,
}

impl<'analysis, 'tcx> Debug for TypeVisitor<'tcx> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        "TypeVisitor".fmt(f)
    }
}

impl<'analysis, 'compilation, 'tcx> TypeVisitor<'tcx> {
    pub fn new(def_id: DefId, mir: &'tcx mir::Body<'tcx>, tcx: TyCtxt<'tcx>) -> TypeVisitor<'tcx> {
        let dummy_untagged_value_type = tcx.types.i8;
        TypeVisitor {
            actual_argument_types: Vec::new(),
            def_id,
            generic_argument_map: None,
            generic_arguments: None,
            mir,
            path_ty_cache: HashMap::new(),
            dummy_untagged_value_type,
            tcx,
        }
    }

    /// Restores the method only state to its initial state.
    #[logfn_inputs(TRACE)]
    pub fn reset_visitor_state(&mut self) {
        self.generic_arguments = None;
        self.path_ty_cache
            .retain(|p, _| p.is_rooted_by_non_local_structure());
    }

    //todo: what about generators?
    /// Parameters that are closures bring enclosed variables with them that are effectively
    /// additional parameters. We pre-populate the environment with entries for these because
    /// there is no convenient way to look up their types later on. I.e. unlike ordinary parameters
    /// whose types can be looked up in mir.local_decls, these extra parameters need their
    /// types extracted from the closure type definitions via the tricky logic below.
    #[logfn_inputs(DEBUG)]
    pub fn add_any_closure_fields_for(
        &mut self,
        mut path_ty: Ty<'tcx>,
        path: &Rc<Path>,
        first_state: &mut Environment,
    ) {
        let mut is_ref = false;
        if let TyKind::Ref(_, t, _) = path_ty.kind() {
            is_ref = true;
            path_ty = t;
        }
        match path_ty.kind() {
            TyKind::Closure(_, substs) => {
                for (i, ty) in substs.as_closure().upvar_tys().enumerate() {
                    let var_type: ExpressionType = ty.kind().into();
                    let mut qualifier = path.clone();
                    if is_ref {
                        qualifier = Path::new_deref(path.clone(), ExpressionType::NonPrimitive)
                    }
                    let closure_field_path = Path::new_field(qualifier, i);
                    self.path_ty_cache.insert(closure_field_path.clone(), ty);
                    let closure_field_val =
                        AbstractValue::make_typed_unknown(var_type, closure_field_path.clone());
                    first_state
                        .value_map
                        .insert_mut(closure_field_path, closure_field_val);
                }
            }
            TyKind::Opaque(def_id, substs) => {
                let map = self.get_generic_arguments_map(*def_id, substs, &[]);
                let path_ty =
                    self.specialize_generic_argument_type(self.tcx.type_of(*def_id), &map);
                self.add_any_closure_fields_for(path_ty, path, first_state);
            }
            TyKind::Dynamic(..) | TyKind::FnDef(..) | TyKind::FnPtr(..) => {}
            _ => {
                info!("unexpected closure type {:?}", path_ty.kind());
            }
        }
    }

    /// Returns the size in bytes (including padding) of an element of the given collection type.
    /// If the type is not a collection, it returns one.
    pub fn get_elem_type_size(&self, ty: Ty<'tcx>) -> u64 {
        match ty.kind() {
            TyKind::Array(ty, _) | TyKind::Slice(ty) => self.get_type_size(ty),
            TyKind::RawPtr(t) => self.get_type_size(t.ty),
            _ => 1,
        }
    }

    /// Path is required to be rooted in a temporary used to track a checked operation result.
    /// The result type of the local will be a tuple (t, bool).
    /// The result of this function is the t part.
    #[logfn_inputs(TRACE)]
    pub fn get_first_part_of_target_path_type_tuple(
        &self,
        path: &Rc<Path>,
        current_span: rustc_span::Span,
    ) -> ExpressionType {
        match self.get_path_rustc_type(path, current_span).kind() {
            TyKind::Tuple(types) => types[0].expect_ty().kind().into(),
            _ => assume_unreachable!(),
        }
    }

    // Path is required to be rooted in a temporary used to track an operation result.
    #[logfn_inputs(TRACE)]
    pub fn get_target_path_type(
        &self,
        path: &Rc<Path>,
        current_span: rustc_span::Span,
    ) -> ExpressionType {
        self.get_path_rustc_type(path, current_span).kind().into()
    }

    /// Returns a parameter environment for the current function.
    pub fn get_param_env(&self) -> rustc_middle::ty::ParamEnv<'tcx> {
        let env_def_id = if self.tcx.is_closure(self.def_id) {
            self.tcx.closure_base_def_id(self.def_id)
        } else {
            self.def_id
        };
        self.tcx.param_env(env_def_id)
    }

    /// This is a hacky and brittle way to navigate the Rust compiler's type system.
    /// Eventually it should be replaced with a comprehensive and principled mapping.
    #[logfn_inputs(TRACE)]
    pub fn get_path_rustc_type(&self, path: &Rc<Path>, current_span: rustc_span::Span) -> Ty<'tcx> {
        if let Some(ty) = self.path_ty_cache.get(path) {
            return *ty;
        }
        match &path.value {
            PathEnum::Computed { value } => match &value.expression {
                Expression::ConditionalExpression { consequent, .. } => {
                    self.get_path_rustc_type(&Path::get_as_path(consequent.clone()), current_span)
                }
                Expression::CompileTimeConstant(c) => c.get_rustc_type(self.tcx),
                Expression::Reference(path) => {
                    let target_type = self.get_path_rustc_type(path, current_span);
                    self.tcx
                        .mk_imm_ref(self.tcx.lifetimes.re_erased, target_type)
                }
                Expression::InitialParameterValue { path, .. }
                | Expression::Variable { path, .. }
                | Expression::WidenedJoin { path, .. } => {
                    self.get_path_rustc_type(path, current_span)
                }
                _ => value.expression.infer_type().as_rustc_type(self.tcx),
            },
            PathEnum::LocalVariable { ordinal } => {
                if *ordinal > 0 && *ordinal < self.mir.local_decls.len() {
                    self.specialize_generic_argument_type(
                        self.mir.local_decls[mir::Local::from(*ordinal)].ty,
                        &self.generic_argument_map,
                    )
                } else {
                    trace!(
                        "local var path.value is {:?} at {:?}",
                        path.value,
                        current_span
                    );
                    self.tcx.types.never
                }
            }
            PathEnum::HeapBlock { value } | PathEnum::Offset { value } => {
                value.expression.infer_type().as_rustc_type(self.tcx)
            }
            PathEnum::Parameter { ordinal } => {
                if *ordinal > 0 && *ordinal < self.mir.local_decls.len() {
                    self.get_loc_ty(mir::Local::from(*ordinal))
                } else {
                    info!(
                        "parameter path.value is {:?} at {:?}",
                        path.value, current_span
                    );
                    self.tcx.types.never
                }
            }
            PathEnum::PhantomData => self.tcx.types.never,
            PathEnum::Result => {
                if self.mir.local_decls.is_empty() {
                    info!("result type wanted from function without result local");
                    self.tcx.types.never
                } else {
                    self.specialize_generic_argument_type(
                        self.mir.local_decls[mir::Local::from(0usize)].ty,
                        &self.generic_argument_map,
                    )
                }
            }
            PathEnum::QualifiedPath {
                qualifier,
                selector,
                ..
            } => {
                let mut t = self.get_path_rustc_type(qualifier, current_span);
                if t.is_never() {
                    return t;
                }
                match t.kind() {
                    TyKind::Infer(..) => {
                        // The qualifier does not resolve to a useful rustc type.
                        // This can happen when the qualifier is a PathEnum::Computed where the value
                        // is TOP, or BOTTOM or a heap layout.
                        return self.tcx.types.never;
                    }
                    TyKind::Projection(..) => {
                        t = self.specialize_generic_argument_type(t, &self.generic_argument_map);
                    }
                    _ => {}
                }
                match &**selector {
                    PathSelector::ConstantSlice { .. } => {
                        return t;
                    }
                    PathSelector::UnionField {
                        case_index: ordinal,
                        ..
                    }
                    | PathSelector::Field(ordinal) => {
                        if let TyKind::Opaque(def_id, subs) = &t.kind() {
                            let map = self.get_generic_arguments_map(*def_id, subs, &[]);
                            t = self
                                .specialize_generic_argument_type(self.tcx.type_of(*def_id), &map);
                            trace!("opaque type_of {:?}", t.kind());
                            trace!("opaque type_of {:?}", t);
                        }
                        match t.kind() {
                            TyKind::Adt(def, substs) => {
                                // enum fields have qualifiers that cast onto the right variant.
                                // In this case, t ends up as a tuple so we won't get here.
                                // See the TyKind::Tuple and PathSelector::DownCast cases below.
                                checked_assume!(!def.is_enum());
                                let variants = &def.variants;
                                assume!(variants.len() == 1); // only enums have more than one variant
                                let variant = &variants[variants.last().unwrap()];
                                if *ordinal < variant.fields.len() {
                                    let field = &variant.fields[*ordinal];
                                    let ft = field.ty(self.tcx, substs);
                                    trace!("field {:?} type is {:?}", ordinal, ft);
                                    return ft;
                                }
                                //todo: assume_unreachable!("field ordinal should always be valid");
                            }
                            TyKind::Closure(def_id, subs) => {
                                return subs.as_closure().upvar_tys().nth(*ordinal).unwrap_or_else(
                                    || {
                                        info!("closure field not found {:?} {:?}", def_id, ordinal);
                                        self.tcx.types.never
                                    },
                                );
                            }
                            TyKind::Ref(_, t, _) if matches!(t.kind(), TyKind::Closure(..)) => {
                                // todo: this seems to work around a more fundamental bug.
                                // why would getting a field from a closure not need a deref
                                // before the field access? I.e. is a reference to a closure
                                // a sort of fat pointer?
                                if let TyKind::Closure(def_id, subs) = t.kind() {
                                    return subs
                                        .as_closure()
                                        .upvar_tys()
                                        .nth(*ordinal)
                                        .unwrap_or_else(|| {
                                            info!(
                                                "closure field not found {:?} {:?}",
                                                def_id, ordinal
                                            );
                                            self.tcx.types.never
                                        });
                                }
                                unreachable!("t.kind is a closure because of the guard");
                            }
                            TyKind::Tuple(types) => {
                                if let Some(gen_arg) = types.get(*ordinal as usize) {
                                    return gen_arg.expect_ty();
                                }
                                if types.is_empty() {
                                    return self.tcx.types.never;
                                }
                            }
                            _ => {
                                if is_slice_pointer(t.kind()) {
                                    match *ordinal {
                                        0 => {
                                            // Field 0 of a slice pointer is a raw pointer to the slice element type
                                            return self.tcx.mk_ptr(rustc_middle::ty::TypeAndMut {
                                                ty: type_visitor::get_element_type(t),
                                                mutbl: rustc_hir::Mutability::Mut,
                                            });
                                        }
                                        1 => {
                                            return self.tcx.types.usize;
                                        }
                                        _ => {}
                                    }
                                }
                                if *ordinal == 0
                                    && matches!(
                                        t.kind(),
                                        TyKind::FnPtr(..) | TyKind::RawPtr(..) | TyKind::Ref(..)
                                    )
                                {
                                    // If the qualifier is a thin pointer to something that
                                    // does not have a field 0, it could be that qualifier.0
                                    // is a bogus path that got constructed when transferring
                                    // a returned fat pointer into a thin pointer target
                                    // variable without an explicit cast.
                                    // It is hard to detect this case before calling this
                                    // routine, so we'll allow it to happen tell the caller
                                    // to look out for it by returning this:
                                    return self.tcx.types.never;
                                }
                            }
                        }
                    }
                    PathSelector::Deref => {
                        return self.get_dereferenced_type(t);
                    }
                    PathSelector::Discriminant => {
                        return self.tcx.types.i32;
                    }
                    PathSelector::Downcast(_, ordinal) => {
                        if let TyKind::Adt(def, substs) = t.kind() {
                            use rustc_index::vec::Idx;
                            if *ordinal >= def.variants.len() {
                                debug!(
                                    "illegally down casting to index {} of {:?} at {:?}",
                                    *ordinal, t, current_span
                                );
                                return self.tcx.types.never;
                            }
                            let variant = &def.variants[VariantIdx::new(*ordinal)];
                            let field_tys = variant.fields.iter().map(|fd| fd.ty(self.tcx, substs));
                            return self.tcx.mk_tup(field_tys);
                        }
                        return self.tcx.types.never;
                    }
                    PathSelector::Index(_) | PathSelector::ConstantIndex { .. } => {
                        return get_element_type(t);
                    }
                    PathSelector::Layout => {
                        return self.tcx.types.trait_object_dummy_self;
                    }
                    PathSelector::Slice(_) => {
                        return if type_visitor::is_slice_pointer(t.kind()) {
                            t
                        } else {
                            let slice_ty = self.tcx.mk_slice(type_visitor::get_element_type(t));
                            self.tcx.mk_mut_ref(self.tcx.lifetimes.re_static, slice_ty)
                        };
                    }
                    PathSelector::TagField => {
                        return self.dummy_untagged_value_type;
                    }
                    _ => {}
                }
                info!("current span is {:?}", current_span);
                info!(
                    "cache key is {:?}",
                    utils::summary_key_str(self.tcx, self.def_id)
                );
                info!("path is {:?}", path);
                info!("t is {:?}", t);
                info!("qualifier is {:?}", qualifier);
                info!("selector is {:?}", selector);
                self.tcx.types.never
            }
            PathEnum::StaticVariable { def_id, .. } => {
                if let Some(def_id) = def_id {
                    return self.tcx.type_of(*def_id);
                }
                info!(
                    "static variable path.value is {:?} at {:?}",
                    path.value, current_span
                );
                self.tcx.types.never
            }
            _ => {
                info!("path.value is {:?} at {:?}", path.value, current_span);
                info!("path_ty_cache {:?}", self.path_ty_cache);
                self.tcx.types.never
            }
        }
    }

    /// Returns the target type of a reference type.
    fn get_dereferenced_type(&self, ty: Ty<'tcx>) -> Ty<'tcx> {
        match ty.kind() {
            TyKind::RawPtr(ty_and_mut) => ty_and_mut.ty,
            TyKind::Ref(_, t, _) => *t,
            _ => {
                if ty.is_box() {
                    ty.boxed_ty()
                } else {
                    ty
                }
            }
        }
    }

    /// If Operand corresponds to a compile time constant function, return
    /// the generic parameter substitutions (type arguments) that are used by
    /// the call instruction whose operand this is.
    #[logfn_inputs(TRACE)]
    pub fn get_generic_arguments_map(
        &self,
        def_id: DefId,
        generic_args: SubstsRef<'tcx>,
        actual_argument_types: &[Ty<'tcx>],
    ) -> Option<HashMap<rustc_span::Symbol, GenericArg<'tcx>>> {
        let mut substitution_map = self.generic_argument_map.clone();
        let mut map: HashMap<rustc_span::Symbol, GenericArg<'tcx>> = HashMap::new();

        // This iterates over the callee's generic parameter definitions.
        // If the parent of the callee is generic, those definitions are iterated
        // as well. This applies recursively. Note that a child cannot mask the
        // generic parameters of its parent with one of its own, so each parameter
        // definition in this iteration will have a unique name.
        InternalSubsts::for_item(self.tcx, def_id, |param_def, _| {
            if let Some(gen_arg) = generic_args.get(param_def.index as usize) {
                let specialized_gen_arg =
                    self.specialize_generic_argument(*gen_arg, &substitution_map);
                if let Some(substitution_map) = &mut substitution_map {
                    substitution_map.insert(param_def.name, specialized_gen_arg);
                }
                map.insert(param_def.name, specialized_gen_arg);
            } else {
                debug!("unmapped generic param def");
            }
            self.tcx.mk_param_from_def(param_def) // not used
        });
        // Add "Self" -> actual_argument_types[0]
        if let Some(self_ty) = actual_argument_types.get(0) {
            let self_ty = if let TyKind::Ref(_, ty, _) = self_ty.kind() {
                *ty
            } else {
                self_ty
            };
            let self_sym = rustc_span::Symbol::intern("Self");
            map.entry(self_sym).or_insert_with(|| self_ty.into());
        }
        if map.is_empty() {
            None
        } else {
            Some(map)
        }
    }

    pub fn get_loc_ty(&self, local: mir::Local) -> Ty<'tcx> {
        let i = local.as_usize();
        if 0 < i && i <= self.mir.arg_count && i <= self.actual_argument_types.len() {
            self.actual_argument_types[i - 1]
        } else {
            self.specialize_generic_argument_type(
                self.mir.local_decls[local].ty,
                &self.generic_argument_map,
            )
        }
    }

    /// Returns an ExpressionType value corresponding to the Rustc type of the place.
    #[logfn_inputs(TRACE)]
    pub fn get_place_type(
        &mut self,
        place: &mir::Place<'tcx>,
        current_span: rustc_span::Span,
    ) -> ExpressionType {
        self.get_rustc_place_type(place, current_span).kind().into()
    }

    /// Returns the rustc Ty of the given place in memory.
    #[logfn_inputs(TRACE)]
    #[logfn(TRACE)]
    pub fn get_rustc_place_type(
        &self,
        place: &mir::Place<'tcx>,
        current_span: rustc_span::Span,
    ) -> Ty<'tcx> {
        let result = {
            let base_type = self.get_loc_ty(place.local);
            self.get_type_for_projection_element(current_span, base_type, &place.projection)
        };
        match result.kind() {
            TyKind::Param(t_par) => {
                if let Some(generic_args) = self.generic_arguments {
                    if let Some(gen_arg) = generic_args.as_ref().get(t_par.index as usize) {
                        return gen_arg.expect_ty();
                    }
                    if t_par.name.as_str() == "Self" && !self.actual_argument_types.is_empty() {
                        return self.actual_argument_types[0];
                    }
                }
            }
            TyKind::Ref(region, ty, mutbl) => {
                if let TyKind::Param(t_par) = ty.kind() {
                    if t_par.name.as_str() == "Self" && !self.actual_argument_types.is_empty() {
                        return self.tcx.mk_ref(
                            region,
                            rustc_middle::ty::TypeAndMut {
                                ty: self.actual_argument_types[0],
                                mutbl: *mutbl,
                            },
                        );
                    }
                }
            }
            _ => {}
        }
        result
    }

    /// Returns the rustc TyKind of the element selected by projection_elem.
    #[logfn_inputs(TRACE)]
    pub fn get_type_for_projection_element(
        &self,
        current_span: rustc_span::Span,
        base_ty: Ty<'tcx>,
        place_projection: &[rustc_middle::mir::PlaceElem<'tcx>],
    ) -> Ty<'tcx> {
        place_projection
            .iter()
            .fold(base_ty, |base_ty, projection_elem| match projection_elem {
                mir::ProjectionElem::Deref => match base_ty.kind() {
                    TyKind::Adt(..) => base_ty,
                    TyKind::RawPtr(ty_and_mut) => ty_and_mut.ty,
                    TyKind::Ref(_, ty, _) => *ty,
                    _ => {
                        debug!(
                            "span: {:?}\nelem: {:?} type: {:?}",
                            current_span, projection_elem, base_ty
                        );
                        assume_unreachable!();
                    }
                },
                mir::ProjectionElem::Field(_, ty) => {
                    self.specialize_generic_argument_type(ty, &self.generic_argument_map)
                }
                mir::ProjectionElem::Subslice { .. } => base_ty,
                mir::ProjectionElem::Index(_) | mir::ProjectionElem::ConstantIndex { .. } => {
                    match base_ty.kind() {
                        TyKind::Adt(..) => base_ty,
                        TyKind::Array(ty, _) => *ty,
                        TyKind::Ref(_, ty, _) => get_element_type(*ty),
                        TyKind::Slice(ty) => *ty,
                        _ => {
                            debug!(
                                "span: {:?}\nelem: {:?} type: {:?}",
                                current_span, projection_elem, base_ty
                            );
                            assume_unreachable!();
                        }
                    }
                }
                mir::ProjectionElem::Downcast(_, ordinal) => {
                    if let TyKind::Adt(def, substs) = base_ty.kind() {
                        if ordinal.index() >= def.variants.len() {
                            debug!(
                                "illegally down casting to index {} of {:?} at {:?}",
                                ordinal.index(),
                                base_ty,
                                current_span
                            );
                            let variant = &def.variants.iter().last().unwrap();
                            let field_tys = variant.fields.iter().map(|fd| fd.ty(self.tcx, substs));
                            return self.tcx.mk_tup(field_tys);
                        }
                        let variant = &def.variants[*ordinal];
                        let field_tys = variant.fields.iter().map(|fd| fd.ty(self.tcx, substs));
                        return self.tcx.mk_tup(field_tys);
                    } else {
                        info!("unexpected type for downcast {:?}", base_ty);
                    }
                    base_ty
                }
            })
    }

    /// Returns the size in bytes (including padding) of an instance of the given type.
    pub fn get_type_size(&self, ty: Ty<'tcx>) -> u64 {
        let param_env = self.get_param_env();
        if let Ok(ty_and_layout) = self.tcx.layout_of(param_env.and(ty)) {
            ty_and_layout.layout.size.bytes()
        } else {
            0
        }
    }

    /// Returns the size (including padding) and alignment, in bytes,  of an instance of the given type.
    pub fn get_type_size_and_alignment(&self, ty: Ty<'tcx>) -> (u128, u128) {
        let param_env = self.get_param_env();
        if let Ok(ty_and_layout) = self.tcx.layout_of(param_env.and(ty)) {
            (
                ty_and_layout.layout.size.bytes() as u128,
                ty_and_layout.align.pref.bytes() as u128,
            )
        } else {
            (0, 8)
        }
    }

    #[logfn_inputs(TRACE)]
    fn specialize_const(
        &self,
        constant: &'tcx Const<'tcx>,
        map: &Option<HashMap<rustc_span::Symbol, GenericArg<'tcx>>>,
    ) -> &'tcx Const<'tcx> {
        if let ConstKind::Param(param_const) = constant.val {
            if let Some(gen_arg) = map.as_ref().unwrap().get(&param_const.name) {
                return gen_arg.expect_const();
            }
        }
        constant
    }

    #[logfn_inputs(TRACE)]
    fn specialize_generic_argument(
        &self,
        gen_arg: GenericArg<'tcx>,
        map: &Option<HashMap<rustc_span::Symbol, GenericArg<'tcx>>>,
    ) -> GenericArg<'tcx> {
        match gen_arg.unpack() {
            GenericArgKind::Type(ty) => self.specialize_generic_argument_type(ty, map).into(),
            GenericArgKind::Const(c) => self.specialize_const(c, map).into(),
            _ => gen_arg,
        }
    }

    #[logfn_inputs(TRACE)]
    pub fn specialize_generic_argument_type(
        &self,
        gen_arg_type: Ty<'tcx>,
        map: &Option<HashMap<rustc_span::Symbol, GenericArg<'tcx>>>,
    ) -> Ty<'tcx> {
        match gen_arg_type.kind() {
            TyKind::Adt(def, substs) => self.tcx.mk_adt(def, self.specialize_substs(substs, map)),
            TyKind::Array(elem_ty, len) => {
                let specialized_elem_ty = self.specialize_generic_argument_type(elem_ty, map);
                let specialized_len = self.specialize_const(len, map);
                self.tcx
                    .mk_ty(TyKind::Array(specialized_elem_ty, specialized_len))
            }
            TyKind::Slice(elem_ty) => {
                let specialized_elem_ty = self.specialize_generic_argument_type(elem_ty, map);
                self.tcx.mk_slice(specialized_elem_ty)
            }
            TyKind::RawPtr(rustc_middle::ty::TypeAndMut { ty, mutbl }) => {
                let specialized_ty = self.specialize_generic_argument_type(ty, map);
                self.tcx.mk_ptr(rustc_middle::ty::TypeAndMut {
                    ty: specialized_ty,
                    mutbl: *mutbl,
                })
            }
            TyKind::Ref(region, ty, mutbl) => {
                let specialized_ty = self.specialize_generic_argument_type(ty, map);
                self.tcx.mk_ref(
                    region,
                    rustc_middle::ty::TypeAndMut {
                        ty: specialized_ty,
                        mutbl: *mutbl,
                    },
                )
            }
            TyKind::FnDef(def_id, substs) => self
                .tcx
                .mk_fn_def(*def_id, self.specialize_substs(substs, map)),
            TyKind::FnPtr(fn_sig) => {
                let map_fn_sig = |fn_sig: FnSig<'tcx>| {
                    let specialized_inputs_and_output = self.tcx.mk_type_list(
                        fn_sig
                            .inputs_and_output
                            .iter()
                            .map(|ty| self.specialize_generic_argument_type(ty, map)),
                    );
                    FnSig {
                        inputs_and_output: specialized_inputs_and_output,
                        c_variadic: fn_sig.c_variadic,
                        unsafety: fn_sig.unsafety,
                        abi: fn_sig.abi,
                    }
                };
                let specialized_fn_sig = fn_sig.map_bound(map_fn_sig);
                self.tcx.mk_fn_ptr(specialized_fn_sig)
            }
            TyKind::Dynamic(predicates, region) => {
                let specialized_predicates =
                    self.tcx
                        .mk_poly_existential_predicates(predicates.iter().map(
                            |pred: rustc_middle::ty::Binder<ExistentialPredicate<'tcx>>| {
                                rustc_middle::ty::Binder::bind(match pred.skip_binder() {
                                    ExistentialPredicate::Trait(ExistentialTraitRef {
                                        def_id,
                                        substs,
                                    }) => ExistentialPredicate::Trait(ExistentialTraitRef {
                                        def_id,
                                        substs: self.specialize_substs(substs, map),
                                    }),
                                    ExistentialPredicate::Projection(ExistentialProjection {
                                        item_def_id,
                                        substs,
                                        ty,
                                    }) => ExistentialPredicate::Projection(ExistentialProjection {
                                        item_def_id,
                                        substs: self.specialize_substs(substs, map),
                                        ty: self.specialize_generic_argument_type(ty, map),
                                    }),
                                    ExistentialPredicate::AutoTrait(_) => pred.skip_binder(),
                                })
                            },
                        ));
                self.tcx.mk_dynamic(specialized_predicates, region)
            }
            TyKind::Closure(def_id, substs) => self
                .tcx
                .mk_closure(*def_id, self.specialize_substs(substs, map)),
            TyKind::Generator(def_id, substs, movability) => {
                self.tcx
                    .mk_generator(*def_id, self.specialize_substs(substs, map), *movability)
            }
            TyKind::GeneratorWitness(bound_types) => {
                let map_types = |types: &rustc_middle::ty::List<Ty<'tcx>>| {
                    self.tcx.mk_type_list(
                        types
                            .iter()
                            .map(|ty| self.specialize_generic_argument_type(ty, map)),
                    )
                };
                let specialized_types = bound_types.map_bound(map_types);
                self.tcx.mk_generator_witness(specialized_types)
            }
            TyKind::Tuple(substs) => self.tcx.mk_tup(
                self.specialize_substs(substs, map)
                    .iter()
                    .map(|gen_arg| gen_arg.expect_ty()),
            ),
            // The projection of an associated type. For example,
            // `<T as Trait<..>>::N`.
            TyKind::Projection(projection) => {
                let specialized_substs = self.specialize_substs(projection.substs, map);
                let item_def_id = projection.item_def_id;
                if utils::are_concrete(specialized_substs) {
                    let param_env = self
                        .tcx
                        .param_env(self.tcx.associated_item(item_def_id).container.id());
                    if let Ok(Some(instance)) = rustc_middle::ty::Instance::resolve(
                        self.tcx,
                        param_env,
                        item_def_id,
                        specialized_substs,
                    ) {
                        let item_def_id = instance.def.def_id();
                        let item_type = self.tcx.type_of(item_def_id);
                        let map = self.get_generic_arguments_map(item_def_id, instance.substs, &[]);
                        if item_type == gen_arg_type && map.is_none() {
                            // Can happen if the projection just adds a life time
                            item_type
                        } else {
                            self.specialize_generic_argument_type(item_type, &map)
                        }
                    } else {
                        if specialized_substs.len() == 1
                            && self.tcx.parent(item_def_id)
                                == self.tcx.lang_items().discriminant_kind_trait()
                        {
                            let enum_arg = specialized_substs[0];
                            if let GenericArgKind::Type(enum_ty) = enum_arg.unpack() {
                                return enum_ty.discriminant_ty(self.tcx);
                            }
                        }
                        debug!("could not resolve an associated type with concrete type arguments");
                        gen_arg_type
                    }
                } else {
                    self.tcx
                        .mk_projection(projection.item_def_id, specialized_substs)
                }
            }
            TyKind::Opaque(def_id, substs) => self
                .tcx
                .mk_opaque(*def_id, self.specialize_substs(substs, map)),
            TyKind::Param(ParamTy { name, .. }) => {
                if let Some(map) = map {
                    if let Some(gen_arg) = map.get(&name) {
                        return gen_arg.expect_ty();
                    }
                }
                gen_arg_type
            }
            _ => gen_arg_type,
        }
    }

    #[logfn_inputs(TRACE)]
    pub fn specialize_substs(
        &self,
        substs: SubstsRef<'tcx>,
        map: &Option<HashMap<rustc_span::Symbol, GenericArg<'tcx>>>,
    ) -> SubstsRef<'tcx> {
        let specialized_generic_args: Vec<GenericArg<'_>> = substs
            .iter()
            .map(|gen_arg| self.specialize_generic_argument(gen_arg, &map))
            .collect();
        self.tcx.intern_substs(&specialized_generic_args)
    }
}

//todo: what about generators?
/// Extracts the def_id of a closure, given the type of a value that is known to be a closure.
/// Returns None otherwise.
#[logfn_inputs(TRACE)]
pub fn get_def_id_from_closure(closure_ty: Ty<'_>) -> Option<DefId> {
    match closure_ty.kind() {
        TyKind::Closure(def_id, _) | TyKind::Opaque(def_id, _) => {
            return Some(*def_id);
        }
        TyKind::Ref(_, ty, _) => {
            if let TyKind::Closure(def_id, _) = ty.kind() {
                return Some(*def_id);
            }
        }
        _ => {}
    }
    None
}

/// Returns the element type of an array or slice type.
#[logfn_inputs(TRACE)]
pub fn get_element_type(ty: Ty<'_>) -> Ty<'_> {
    match &ty.kind() {
        TyKind::Array(t, _) => *t,
        TyKind::RawPtr(TypeAndMut { ty: t, .. }) | TyKind::Ref(_, t, _) => match t.kind() {
            TyKind::Array(t, _) => *t,
            TyKind::Slice(t) => *t,
            _ => t,
        },
        TyKind::Slice(t) => *t,
        _ => ty,
    }
}

/// Returns the type of value that a pointer of ty points to.
/// If ty is not a pointer type, just return ty.
#[logfn_inputs(TRACE)]
pub fn get_target_type(ty: Ty<'_>) -> Ty<'_> {
    match ty.kind() {
        TyKind::RawPtr(TypeAndMut { ty: t, .. }) | TyKind::Ref(_, t, _) => t,
        _ => ty,
    }
}

/// Returns true if the given type is a reference (or raw pointer) that is not a slice pointer
#[logfn_inputs(TRACE)]
pub fn is_thin_pointer(ty_kind: &TyKind<'_>) -> bool {
    if let TyKind::RawPtr(TypeAndMut { ty: target, .. }) | TyKind::Ref(_, target, _) = ty_kind {
        !matches!(target.kind(), TyKind::Slice(..) | TyKind::Str)
    } else {
        false
    }
}

/// Returns true if the given type is a reference (or raw pointer) to a collection type, in which
/// case the reference/pointer independently tracks the length of the collection, thus effectively
/// tracking a slice of the underlying collection.
#[logfn_inputs(TRACE)]
#[logfn(TRACE)]
pub fn is_slice_pointer(ty_kind: &TyKind<'_>) -> bool {
    if let TyKind::RawPtr(TypeAndMut { ty: target, .. }) | TyKind::Ref(_, target, _) = ty_kind {
        trace!("target type {:?}", target.kind());
        // Pointers to sized arrays are thin pointers.
        matches!(target.kind(), TyKind::Slice(..) | TyKind::Str)
    } else {
        false
    }
}
