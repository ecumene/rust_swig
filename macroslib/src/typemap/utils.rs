use proc_macro2::TokenStream;
use syn::{spanned::Spanned, Type};

use crate::{
    error::{DiagnosticError, Result},
    source_registry::SourceId,
    typemap::{
        ast::{fn_arg_type, parse_ty_with_given_span_checked, DisplayToTokens},
        ty::RustType,
        ForeignMethodSignature, ForeignTypeInfo, TypeMap,
    },
    types::{
        ForeignInterfaceMethod, ForeignerClassInfo, ForeignerMethod, MethodVariant, SelfTypeVariant, ValidFnArg
    },
};

pub(crate) fn foreign_from_rust_convert_method_output(
    conv_map: &mut TypeMap,
    src_id: SourceId,
    rust_ret_ty: &syn::ReturnType,
    f_output: &ForeignTypeInfo,
    var_name: &str,
    func_ret_type: &str,
) -> Result<(Vec<TokenStream>, String)> {
    let rust_ret_ty: Type = match *rust_ret_ty {
        syn::ReturnType::Default => {
            if f_output.name != "void" {
                return Err(DiagnosticError::new(
                    src_id,
                    rust_ret_ty.span(),
                    format!("Rust type `()` mapped to not void ({})", f_output.name),
                ));
            } else {
                return Ok((Vec::new(), String::new()));
            }
        }
        syn::ReturnType::Type(_, ref p_ty) => (**p_ty).clone(),
    };
    let context_span = rust_ret_ty.span();
    let rust_ret_ty = conv_map.find_or_alloc_rust_type(&rust_ret_ty, src_id);
    conv_map.convert_rust_types(
        &rust_ret_ty,
        &f_output.correspoding_rust_type,
        var_name,
        func_ret_type,
        (src_id, context_span),
    )
}

pub(crate) fn foreign_to_rust_convert_method_inputs<
    FTI: AsRef<ForeignTypeInfo>,
    GI: Iterator<Item = String>,
>(
    conv_map: &mut TypeMap,
    src_id: SourceId,
    method: &ForeignerMethod<syn::FnArg>,
    f_method: &ForeignMethodSignature<FI = FTI>,
    arg_names: GI,
    func_ret_type: &str,
) -> Result<(Vec<TokenStream>, String)> {
    let mut code_deps = Vec::new();
    let mut ret_code = String::new();

    //skip self
    let skip_n = match method.variant {
        MethodVariant::Method(_) => 1,
        _ => 0,
    };
    for ((to_type, f_from), arg_name) in method
        .fn_decl
        .inputs
        .iter()
        .skip(skip_n)
        .zip(f_method.input().iter())
        .zip(arg_names)
    {
        let to: RustType = conv_map.find_or_alloc_rust_type(fn_arg_type(to_type), src_id);
        let (mut cur_deps, cur_code) = conv_map.convert_rust_types(
            &f_from.as_ref().correspoding_rust_type,
            &to,
            &arg_name,
            func_ret_type,
            (src_id, to_type.span()),
        )?;
        code_deps.append(&mut cur_deps);
        ret_code.push_str(&cur_code);
    }
    Ok((code_deps, ret_code))
}

pub(crate) fn create_suitable_types_for_constructor_and_self(
    self_variant: SelfTypeVariant,
    class: &ForeignerClassInfo<ValidFnArg>,
    constructor_real_type: &Type,
) -> (Type, Type) {
    match self_variant {
        SelfTypeVariant::Default => {
            unimplemented!();
        }
        SelfTypeVariant::Mut => {
            unimplemented!();
        }
        SelfTypeVariant::Rptr | SelfTypeVariant::RptrMut => {
            let self_type = class.self_type_as_ty();
            if self_variant == SelfTypeVariant::Rptr {
                (
                    parse_ty_with_given_span_checked(
                        &format!("& {}", DisplayToTokens(constructor_real_type)),
                        constructor_real_type.span(),
                    ),
                    parse_ty_with_given_span_checked(
                        &format!("& {}", DisplayToTokens(&self_type)),
                        self_type.span(),
                    ),
                )
            } else {
                (
                    parse_ty_with_given_span_checked(
                        &format!("&mut {}", DisplayToTokens(constructor_real_type)),
                        constructor_real_type.span(),
                    ),
                    parse_ty_with_given_span_checked(
                        &format!("&mut {}", DisplayToTokens(&self_type)),
                        self_type.span(),
                    ),
                )
            }
        }
    }
}

pub(crate) fn rust_to_foreign_convert_method_inputs<
    GI: Iterator<Item = String>,
    FTI: AsRef<ForeignTypeInfo>,
>(
    conv_map: &mut TypeMap,
    src_id: SourceId,
    method: &ForeignInterfaceMethod<syn::FnArg>,
    f_method: &ForeignMethodSignature<FI = FTI>,
    arg_names: GI,
    func_ret_type: &str,
) -> Result<(Vec<TokenStream>, String)> {
    let mut code_deps = Vec::new();
    let mut ret_code = String::new();

    for ((from_ty, to_f), arg_name) in method
        .fn_decl
        .inputs
        .iter()
        .skip(1) //skip self
        .zip(f_method.input().iter())
        .zip(arg_names)
    {
        let from: RustType = conv_map.find_or_alloc_rust_type(fn_arg_type(from_ty), src_id);
        let (mut cur_deps, cur_code) = conv_map.convert_rust_types(
            &from,
            &to_f.as_ref().correspoding_rust_type,
            &arg_name,
            func_ret_type,
            (src_id, from_ty.span()),
        )?;
        code_deps.append(&mut cur_deps);
        ret_code.push_str(&cur_code);
    }
    Ok((code_deps, ret_code))
}
