mod java_code;
mod map_type;
mod rust_code;

use std::fmt;

use log::debug;
use petgraph::Direction;
use proc_macro2::TokenStream;
use smol_str::SmolStr;
use syn::{parse_quote, spanned::Spanned, Type};

use self::map_type::map_type;
use crate::{
    error::{DiagnosticError, Result},
    source_registry::SourceId,
    typemap::ast::{
        fn_arg_type, if_result_return_ok_err_types, if_ty_result_return_ok_type,
        parse_ty_with_given_span_checked, DisplayToTokens, TypeName,
    },
    typemap::{
        ty::RustType, ForeignMethodSignature, ForeignTypeInfo, FROM_VAR_TEMPLATE, TO_VAR_TEMPLATE,
    },
    types::{
        ForeignEnumInfo, ForeignInterface, ForeignerClassInfo, ForeignerMethod, MethodVariant,
    },
    JavaConfig, LanguageGenerator, SourceCode, TypeMap,
};

#[derive(Clone, Copy)]
enum NullAnnotation {
    NonNull,
    Nullable,
}

struct JavaForeignTypeInfo {
    pub base: ForeignTypeInfo,
    pub java_converter: Option<JavaConverter>,
    annotation: Option<NullAnnotation>,
}

struct JavaConverter {
    java_transition_type: SmolStr,
    converter: String,
}

impl AsRef<ForeignTypeInfo> for JavaForeignTypeInfo {
    fn as_ref(&self) -> &ForeignTypeInfo {
        &self.base
    }
}

impl JavaForeignTypeInfo {
    fn java_convert<NameArg>(&self, name_arg: NameArg) -> Option<String>
    where
        NameArg: Fn() -> (String, String),
    {
        if let Some(code) = self.java_converter.as_ref().map(|x| &x.converter) {
            let (from_name, to_name) = name_arg();
            Some(
                code.replace(TO_VAR_TEMPLATE, &to_name)
                    .replace(FROM_VAR_TEMPLATE, &from_name),
            )
        } else {
            None
        }
    }
}

impl From<ForeignTypeInfo> for JavaForeignTypeInfo {
    fn from(x: ForeignTypeInfo) -> Self {
        JavaForeignTypeInfo {
            base: ForeignTypeInfo {
                name: x.name,
                correspoding_rust_type: x.correspoding_rust_type,
            },
            java_converter: None,
            annotation: None,
        }
    }
}

struct JniForeignMethodSignature {
    output: ForeignTypeInfo,
    input: Vec<JavaForeignTypeInfo>,
}

impl ForeignMethodSignature for JniForeignMethodSignature {
    type FI = JavaForeignTypeInfo;
    fn output(&self) -> &ForeignTypeInfo {
        &self.output
    }
    fn input(&self) -> &[JavaForeignTypeInfo] {
        &self.input[..]
    }
}

impl LanguageGenerator for JavaConfig {
    fn init(
        &self,
        conv_map: &mut TypeMap,
        _code: &[SourceCode],
    ) -> std::result::Result<(), String> {
        conv_map.find_or_alloc_rust_type_no_src_id(&parse_type! { jint });
        conv_map.find_or_alloc_rust_type_no_src_id(&parse_type! { jlong });
        Ok(())
    }
    fn register_class(&self, conv_map: &mut TypeMap, class: &ForeignerClassInfo) -> Result<()> {
        class
            .validate_class()
            .map_err(|err| DiagnosticError::new(class.src_id, class.span(), &err))?;
        if let Some(constructor_ret_type) = class.constructor_ret_type.as_ref() {
            let this_type_for_method = if_ty_result_return_ok_type(constructor_ret_type)
                .unwrap_or_else(|| constructor_ret_type.clone());

            let this_type: RustType = conv_map.find_or_alloc_rust_type_that_implements(
                &this_type_for_method,
                "SwigForeignClass",
                class.src_id,
            );
            debug!(
                "register_class: add implements SwigForeignClass for {}",
                this_type
            );

            let my_jobj_ti = conv_map.find_or_alloc_rust_type_with_suffix(
                &parse_type! { jobject },
                &this_type.normalized_name,
                SourceId::none(),
            );

            conv_map.cache_rust_to_foreign_conv(
                &this_type,
                ForeignTypeInfo {
                    correspoding_rust_type: my_jobj_ti,
                    name: class.name.to_string().into(),
                },
            )?;

            conv_map.find_or_alloc_rust_type(constructor_ret_type, class.src_id);

            let (this_type_for_method, _code_box_this) =
                conv_map.convert_to_heap_pointer(&this_type, "this");

            let jlong_ti: RustType =
                conv_map.find_or_alloc_rust_type_no_src_id(&parse_type! { jlong });
            let this_type_for_method_ty = &this_type_for_method.ty;
            let code = format!("& {}", DisplayToTokens(this_type_for_method_ty));
            let gen_ty = parse_ty_with_given_span_checked(&code, this_type_for_method_ty.span());
            let this_type_ref =
                conv_map.find_or_alloc_rust_type(&gen_ty, this_type_for_method.src_id);
            //handle foreigner_class as input arg
            conv_map.add_conversation_rule(
                jlong_ti.clone(),
                this_type_ref,
                format!(
                    r#"
        let {to_var}: &{this_type} = unsafe {{
            jlong_to_pointer::<{this_type}>({from_var}).as_mut().unwrap()
        }};
    "#,
                    to_var = TO_VAR_TEMPLATE,
                    from_var = FROM_VAR_TEMPLATE,
                    this_type = this_type_for_method.normalized_name,
                )
                .into(),
            );
            let code = format!("&mut {}", DisplayToTokens(this_type_for_method_ty));
            let gen_ty = parse_ty_with_given_span_checked(&code, this_type_for_method_ty.span());
            let this_type_mut_ref =
                conv_map.find_or_alloc_rust_type(&gen_ty, this_type_for_method.src_id);
            //handle foreigner_class as input arg
            conv_map.add_conversation_rule(
                jlong_ti.clone(),
                this_type_mut_ref,
                format!(
                    r#"
        let {to_var}: &mut {this_type} = unsafe {{
            jlong_to_pointer::<{this_type}>({from_var}).as_mut().unwrap()
        }};
    "#,
                    to_var = TO_VAR_TEMPLATE,
                    from_var = FROM_VAR_TEMPLATE,
                    this_type = this_type_for_method.normalized_name,
                )
                .into(),
            );

            let unpack_code =
                TypeMap::unpack_from_heap_pointer(&this_type_for_method, TO_VAR_TEMPLATE, true);
            conv_map.add_conversation_rule(
                jlong_ti,
                this_type,
                format!(
                    r#"
        let {to_var}: *mut {this_type} = unsafe {{
            jlong_to_pointer::<{this_type}>({from_var}).as_mut().unwrap()
        }};
    {unpack_code}
    "#,
                    to_var = TO_VAR_TEMPLATE,
                    from_var = FROM_VAR_TEMPLATE,
                    this_type = this_type_for_method.normalized_name,
                    unpack_code = unpack_code,
                )
                .into(),
            );
        }

        let _ = conv_map.find_or_alloc_rust_type(&class.self_type_as_ty(), class.src_id);

        Ok(())
    }

    fn generate(
        &self,
        conv_map: &mut TypeMap,
        _: usize,
        class: &ForeignerClassInfo,
    ) -> Result<Vec<TokenStream>> {
        debug!(
            "generate: begin for {}, this_type_for_method {:?}",
            class.name, class.constructor_ret_type
        );

        let f_methods_sign = find_suitable_foreign_types_for_methods(conv_map, class)?;
        java_code::generate_java_code(
            conv_map,
            &self.output_dir,
            &self.package_name,
            class,
            &f_methods_sign,
            self.null_annotation_package.as_ref().map(String::as_str),
        )
        .map_err(|err| DiagnosticError::new(class.src_id, class.span(), err))?;
        debug!("generate: java code done");
        let ast_items =
            rust_code::generate_rust_code(conv_map, &self.package_name, class, &f_methods_sign)?;

        Ok(ast_items)
    }

    fn generate_enum(
        &self,
        conv_map: &mut TypeMap,
        pointer_target_width: usize,
        enum_info: &ForeignEnumInfo,
    ) -> Result<Vec<TokenStream>> {
        if (enum_info.items.len() as u64) >= (i32::max_value() as u64) {
            return Err(DiagnosticError::new(
                enum_info.src_id,
                enum_info.span(),
                "Too many items in enum",
            ));
        }

        java_code::generate_java_code_for_enum(&self.output_dir, &self.package_name, enum_info)
            .map_err(|err| DiagnosticError::new(enum_info.src_id, enum_info.span(), &err))?;

        rust_code::generate_rust_code_for_enum(
            &self.package_name,
            conv_map,
            pointer_target_width,
            enum_info,
        )
    }

    fn generate_interface(
        &self,
        conv_map: &mut TypeMap,
        pointer_target_width: usize,
        interface: &ForeignInterface,
    ) -> Result<Vec<TokenStream>> {
        let f_methods = find_suitable_ftypes_for_interace_methods(conv_map, interface)?;
        java_code::generate_java_code_for_interface(
            &self.output_dir,
            &self.package_name,
            interface,
            &f_methods,
            self.null_annotation_package.as_ref().map(String::as_str),
        )
        .map_err(|err| DiagnosticError::new(interface.src_id, interface.span(), err))?;
        let items = rust_code::generate_interface(
            &self.package_name,
            conv_map,
            pointer_target_width,
            interface,
            &f_methods,
        )?;

        let my_jobj_ti = conv_map.find_or_alloc_rust_type_with_suffix(
            &parse_type! { jobject },
            &interface.name.to_string(),
            SourceId::none(),
        );
        conv_map.add_foreign(
            my_jobj_ti,
            TypeName::from_ident(&interface.name, interface.src_id),
        )?;
        Ok(items)
    }
}

fn method_name(method: &ForeignerMethod, f_method: &JniForeignMethodSignature) -> String {
    let need_conv = f_method.input.iter().any(|v| v.java_converter.is_some());
    match method.variant {
        MethodVariant::StaticMethod if !need_conv => method.short_name().as_str().to_string(),
        MethodVariant::Method(_) | MethodVariant::StaticMethod => {
            format!("do_{}", method.short_name())
        }
        MethodVariant::Constructor => "init".into(),
    }
}

fn find_suitable_ftypes_for_interace_methods(
    conv_map: &mut TypeMap,
    interace: &ForeignInterface,
) -> Result<Vec<JniForeignMethodSignature>> {
    let void_sym = "void";
    let dummy_ty = parse_type! { () };
    let dummy_rust_ty = conv_map.find_or_alloc_rust_type_no_src_id(&dummy_ty);
    let mut f_methods = vec![];

    for method in &interace.items {
        let mut input = Vec::<JavaForeignTypeInfo>::with_capacity(method.fn_decl.inputs.len() - 1);
        for arg in method.fn_decl.inputs.iter().skip(1) {
            let arg_rust_ty = conv_map.find_or_alloc_rust_type(fn_arg_type(arg), interace.src_id);
            let f_arg_type = map_type(
                conv_map,
                &arg_rust_ty,
                Direction::Outgoing,
                (interace.src_id, fn_arg_type(arg).span()),
            )?;

            input.push(f_arg_type);
        }
        let output = match method.fn_decl.output {
            syn::ReturnType::Default => ForeignTypeInfo {
                name: void_sym.into(),
                correspoding_rust_type: dummy_rust_ty.clone(),
            },
            _ => unimplemented!(),
        };
        f_methods.push(JniForeignMethodSignature { output, input });
    }
    Ok(f_methods)
}

fn find_suitable_foreign_types_for_methods(
    conv_map: &mut TypeMap,
    class: &ForeignerClassInfo,
) -> Result<Vec<JniForeignMethodSignature>> {
    let mut ret = Vec::<JniForeignMethodSignature>::with_capacity(class.methods.len());
    let empty_symbol = "";
    let dummy_ty = parse_type! { () };
    let dummy_rust_ty = conv_map.find_or_alloc_rust_type_no_src_id(&dummy_ty);

    for method in &class.methods {
        //skip self argument
        let skip_n = match method.variant {
            MethodVariant::Method(_) => 1,
            _ => 0,
        };
        assert!(method.fn_decl.inputs.len() >= skip_n);
        let mut input =
            Vec::<JavaForeignTypeInfo>::with_capacity(method.fn_decl.inputs.len() - skip_n);
        for arg in method.fn_decl.inputs.iter().skip(skip_n) {
            let arg_rust_ty = conv_map.find_or_alloc_rust_type(fn_arg_type(arg), class.src_id);

            let fti = map_type(
                conv_map,
                &arg_rust_ty,
                Direction::Incoming,
                (class.src_id, fn_arg_type(arg).span()),
            )?;
            input.push(fti);
        }
        let output = match method.variant {
            MethodVariant::Constructor => ForeignTypeInfo {
                name: empty_symbol.into(),
                correspoding_rust_type: dummy_rust_ty.clone(),
            },
            _ => match method.fn_decl.output {
                syn::ReturnType::Default => ForeignTypeInfo {
                    name: "void".into(),
                    correspoding_rust_type: dummy_rust_ty.clone(),
                },
                syn::ReturnType::Type(_, ref rt) => {
                    let ret_rust_ty = conv_map.find_or_alloc_rust_type(rt, class.src_id);
                    let fti = map_type(
                        conv_map,
                        &ret_rust_ty,
                        Direction::Outgoing,
                        (class.src_id, rt.span()),
                    )?;
                    fti.base
                }
            },
        };
        ret.push(JniForeignMethodSignature { output, input });
    }
    Ok(ret)
}

fn fmt_write_err_map(err: fmt::Error) -> String {
    format!("fmt write error: {}", err)
}

fn java_class_full_name(package_name: &str, class_name: &str) -> String {
    let mut ret: String = package_name.into();
    ret.push('.');
    ret.push_str(class_name);
    ret
}

fn java_class_name_to_jni(full_name: &str) -> String {
    full_name.replace(".", "/")
}

fn calc_this_type_for_method(tm: &TypeMap, class: &ForeignerClassInfo) -> Option<Type> {
    if let Some(constructor_ret_type) = class.constructor_ret_type.as_ref() {
        Some(
            if_result_return_ok_err_types(
                &tm.ty_to_rust_type_checked(constructor_ret_type)
                    .unwrap_or_else(|| {
                        panic!(
                            "Internal error: constructor type {} for class {} unknown",
                            DisplayToTokens(constructor_ret_type),
                            class.name
                        );
                    }),
            )
            .map(|(ok_ty, _err_ty)| ok_ty)
            .unwrap_or_else(|| constructor_ret_type.clone()),
        )
    } else {
        None
    }
}
