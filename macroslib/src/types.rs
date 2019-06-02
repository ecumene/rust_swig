use proc_macro2::{Ident, Span};

use syn::{parse_quote, spanned::Spanned, Token, Type};

use smol_str::SmolStr;

use crate::{
    error::{DiagnosticError, Result, SourceIdSpan},
    source_registry::SourceId,
};

#[derive(Debug, Clone)]
pub(crate) struct ForeignerClassInfo<T> {
    pub(crate) src_id: SourceId,
    pub(crate) name: Ident,
    pub(crate) methods: Vec<ForeignerMethod<T>>,
    pub(crate) self_type: Option<Type>,
    pub(crate) foreigner_code: String,
    /// For example if we have `fn new(x: X) -> Result<Y, Z>`, then Result<Y, Z>
    pub(crate) constructor_ret_type: Option<Type>,
    pub(crate) doc_comments: Vec<String>,
    pub(crate) copy_derived: bool,
}

impl <T> ForeignerClassInfo<T> {
    pub(crate) fn span(&self) -> Span {
        self.name.span()
    }
    pub(crate) fn self_type_as_ty(&self) -> Type {
        self.self_type
            .as_ref()
            .cloned()
            .unwrap_or_else(|| parse_quote! { () })
    }
    /// common for several language binding generator code
    pub(crate) fn validate_class(&self) -> Result<()> {
        let mut has_constructor = false;
        let mut has_methods = false;
        let mut has_static_methods = false;
        for x in &self.methods {
            match x.variant {
                MethodVariant::Constructor => has_constructor = true,
                MethodVariant::Method(_) => has_methods = true,
                MethodVariant::StaticMethod => has_static_methods = true,
            }
        }
        if self.self_type.is_none() && has_constructor {
            Err(DiagnosticError::new(
                self.src_id,
                self.span(),
                format!(
                    "class {} has constructor, but no self_type defined",
                    self.name
                ),
            ))
        } else if self.self_type.is_none() && has_methods {
            Err(DiagnosticError::new(
                self.src_id,
                self.span(),
                format!("class {} has methods, but no self_type defined", self.name),
            ))
        } else if self.self_type.is_some()
            && !has_static_methods
            && !has_constructor
            && !has_methods
        {
            Err(DiagnosticError::new(
                self.src_id,
                self.span(),
                format!(
                    "class {} has only self_type, but no methods or constructors",
                    self.name
                ),
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ForeignerMethod<T> {
    pub(crate) variant: MethodVariant,
    pub(crate) rust_id: syn::Path,
    pub(crate) fn_decl: FnDecl<T>,
    pub(crate) name_alias: Option<Ident>,
    pub(crate) access: MethodAccess,
    pub(crate) doc_comments: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct FnDecl<T> {
    pub(crate) span: Span,
    pub(crate) inputs: Vec<T>,
    pub(crate) output: syn::ReturnType,
}

impl <T: From<syn::FnArg>> From<syn::FnDecl> for crate::types::FnDecl<T> {
    fn from(x: syn::FnDecl) -> Self {
        crate::types::FnDecl {
            span: x.fn_token.span(),
            inputs: x.inputs.iter().map(|x| T::from(*x)).collect(),
            output: x.output,
        }
    }
}

impl <T> ForeignerMethod<T> {
    pub(crate) fn short_name(&self) -> String {
        if let Some(ref name) = self.name_alias {
            name.to_string()
        } else {
            match self.rust_id.segments.len() {
                0 => String::new(),
                n => self.rust_id.segments[n - 1].ident.to_string(),
            }
        }
    }

    pub(crate) fn span(&self) -> Span {
        self.rust_id.span()
    }

    pub(crate) fn is_dummy_constructor(&self) -> bool {
        self.rust_id.segments.is_empty()
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub(crate) enum MethodAccess {
    Private,
    Public,
    Protected,
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub(crate) enum MethodVariant {
    Constructor,
    Method(SelfTypeVariant),
    StaticMethod,
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub(crate) enum SelfTypeVariant {
    RptrMut,
    Rptr,
    Mut,
    Default,
}

impl SelfTypeVariant {
    pub(crate) fn is_read_only(self) -> bool {
        match self {
            SelfTypeVariant::RptrMut | SelfTypeVariant::Mut => false,
            SelfTypeVariant::Default | SelfTypeVariant::Rptr => true,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ForeignEnumInfo {
    pub(crate) src_id: SourceId,
    pub(crate) name: Ident,
    pub(crate) items: Vec<ForeignEnumItem>,
    pub(crate) doc_comments: Vec<String>,
}

impl ForeignEnumInfo {
    pub(crate) fn rust_enum_name(&self) -> String {
        self.name.to_string()
    }
    pub(crate) fn span(&self) -> Span {
        self.name.span()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ForeignEnumItem {
    pub(crate) name: Ident,
    pub(crate) rust_name: syn::Path,
    pub(crate) doc_comments: Vec<String>,
}

pub(crate) struct ForeignInterface<T> {
    pub(crate) src_id: SourceId,
    pub(crate) name: Ident,
    pub(crate) self_type: syn::Path,
    pub(crate) doc_comments: Vec<String>,
    pub(crate) items: Vec<ForeignInterfaceMethod<T>>,
}

impl <T> ForeignInterface<T>  {
    pub(crate) fn span(&self) -> Span {
        self.name.span()
    }
    pub(crate) fn src_id_span(&self) -> SourceIdSpan {
        (self.src_id, self.name.span())
    }
}

pub(crate) struct ForeignInterfaceMethod<T> {
    pub(crate) name: Ident,
    pub(crate) rust_name: syn::Path,
    pub(crate) fn_decl: FnDecl<T>,
    pub(crate) doc_comments: Vec<String>,
}

pub(crate) struct ValidFnArg((SmolStr, Span), Type);