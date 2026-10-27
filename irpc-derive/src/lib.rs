use std::collections::HashSet;

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{ToTokens, quote};
use syn::{
    Attribute, Data, DeriveInput, Error, Fields, Ident, LitStr, Meta, Token, Type, Visibility,
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
    spanned::Spanned,
    token::Comma,
};

/// Attribute on protocol enums and variants
const RPC_ATTR_NAME: &str = "rpc";
/// Attribute on variants to wrap in generated struct
const WRAP_ATTR_NAME: &str = "wrap";
/// The tx type name
const TX_ATTR: &str = "tx";
/// The rx type name
const RX_ATTR: &str = "rx";
/// Fully qualified path to the default rx type
const DEFAULT_RX_TYPE: &str = "::irpc::channel::none::NoReceiver";
/// Fully qualified path to the default tx type
const DEFAULT_TX_TYPE: &str = "::irpc::channel::none::NoSender";

// See `irpc::rpc_requests` for docs.
#[proc_macro_attribute]
pub fn rpc_requests(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let args = parse_macro_input!(attr as MacroArgs);
    match expand_rpc_requests(input, args) {
        Ok(output) => output.into(),
        Err(err) => err.into_compile_error().into(),
    }
}

#[derive(Clone)]
struct RequestVariant {
    ident: Ident,
    ty: Type,
    /// Conditions for both the protocol and this variant.
    cfg_attrs: Vec<Attribute>,
}

fn expand_rpc_requests(mut input: DeriveInput, args: MacroArgs) -> syn::Result<TokenStream2> {
    let enum_name = &input.ident;
    let vis = &input.vis;
    let enum_cfg_attrs = filtered_attributes(&input.attrs, &["cfg"])?;
    let message_attrs = filtered_attributes(&input.attrs, &["cfg", "non_exhaustive"])?;

    let data_enum = match &mut input.data {
        Data::Enum(data_enum) => data_enum,
        _ => {
            return Err(Error::new(
                input.span(),
                "The rpc_requests macro can only be applied to enums",
            ));
        }
    };

    let cfg_feature_rpc = match args.rpc_feature.as_ref() {
        None => quote!(),
        Some(feature) => quote!(#[cfg(feature = #feature)]),
    };

    let mut channel_impls = TokenStream2::new();
    let mut unconditional_types = HashSet::new();
    let mut all_variants = Vec::new();
    let mut variants_with_attr = Vec::new();
    let mut wrapper_types = TokenStream2::new();

    for variant in &mut data_enum.variants {
        let rpc_attr = VariantRpcArgs::from_attrs(&mut variant.attrs)?;
        let variant_cfg_attrs = filtered_attributes(&variant.attrs, &["cfg"])?;
        let cfg_attrs: Vec<_> = enum_cfg_attrs
            .iter()
            .chain(&variant_cfg_attrs)
            .cloned()
            .collect();

        let request_type = match rpc_attr.wrap {
            None => match &mut variant.fields {
                Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                    fields.unnamed[0].ty.clone()
                }
                _ => {
                    return Err(Error::new(
                        variant.span(),
                        "Each variant must either have exactly one unnamed field, or use the `wrap` argument in the `rpc` attribute.",
                    ));
                }
            },
            Some(WrapArgs { ident, derive, vis }) => {
                let vis = vis.as_ref().unwrap_or(&input.vis).clone();
                let ty = type_from_ident(&ident);
                let struc = struct_from_variant_fields(
                    ident,
                    variant.fields.clone(),
                    variant.attrs.clone(),
                    vis,
                );
                wrapper_types.extend(quote! {
                    #(#enum_cfg_attrs)*
                    #[derive(
                        ::std::fmt::Debug,
                        ::irpc::__macro_exports::serde::Serialize,
                        ::irpc::__macro_exports::serde::Deserialize,
                        #(#derive),*
                    )]
                    #[serde(crate = "::irpc::__macro_exports::serde")]
                    #struc
                });
                variant.fields = single_unnamed_field(ty.clone());
                ty
            }
        };

        // Conditional variants may use the same type under mutually exclusive
        // conditions. Rust checks conflicting impls if both are enabled.
        if variant_cfg_attrs.is_empty()
            && !unconditional_types.insert(request_type.to_token_stream().to_string())
        {
            return Err(Error::new(
                variant.span(),
                "Each variant must have a unique request type",
            ));
        }

        let info = RequestVariant {
            ident: variant.ident.clone(),
            ty: request_type,
            cfg_attrs,
        };
        if let Some(args) = rpc_attr.rpc {
            channel_impls.extend(generate_channels_impl(args, enum_name, &info));
            variants_with_attr.push(info.clone());
        }
        all_variants.push(info);
    }

    let protocol_enum_from_impls =
        generate_protocol_enum_from_impls(enum_name, &variants_with_attr);
    let type_aliases = if let Some(suffix) = args.alias_suffix {
        generate_type_aliases(&all_variants, enum_name, &suffix, vis)
    } else {
        quote! {}
    };

    let extended_enum_code = if let Some(message_enum_name) = args.message_enum_name.as_ref() {
        let message_variants = all_variants.iter().map(|variant| {
            let RequestVariant {
                ident,
                ty,
                cfg_attrs,
            } = variant;
            quote! {
                #(#cfg_attrs)*
                #ident(::irpc::WithChannels<#ty, #enum_name>)
            }
        });

        let doc = format!("Message enum for [`{enum_name}`]");
        let message_enum = quote! {
            #(#message_attrs)*
            #[doc = #doc]
            #[allow(missing_docs)]
            #[derive(::std::fmt::Debug)]
            #vis enum #message_enum_name {
                #(#message_variants),*
            }
        };

        let span_impl = generate_span_impl(message_enum_name, &all_variants, &enum_cfg_attrs);
        let message_from_impls =
            generate_message_enum_from_impls(message_enum_name, &variants_with_attr, enum_name);

        let span_propagation = args.span_propagation;
        let span_arms = all_variants.iter().map(|variant| {
            let RequestVariant { ident, cfg_attrs, .. } = variant;
            let name = ident.to_string();
            quote! {
                #(#cfg_attrs)*
                Self::#ident { .. } => ::irpc::__macro_exports::tracing::debug_span!(#name, req = req_id)
            }
        });
        let service_impl = quote! {
            #(#enum_cfg_attrs)*
            impl ::irpc::Service for #enum_name {
                type Message = #message_enum_name;
                const SPAN_PROPAGATION: bool = #span_propagation;

                fn new_request_span(
                    &self,
                    req_id: u64,
                ) -> ::irpc::__macro_exports::tracing::Span {
                    match *self {
                        #(#span_arms),*
                    }
                }
            }
        };

        let remote_service_impl = if !args.no_rpc {
            let block = generate_remote_service_impl(
                message_enum_name,
                enum_name,
                &variants_with_attr,
                &enum_cfg_attrs,
            );
            quote! {
                #cfg_feature_rpc
                #block
            }
        } else {
            quote! {}
        };

        quote! {
            #message_enum
            #service_impl
            #remote_service_impl
            #span_impl
            #message_from_impls
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        #input
        #wrapper_types
        #channel_impls
        #protocol_enum_from_impls
        #type_aliases
        #extended_enum_code
    })
}

/// Retains only the requested attributes, including inside nested `cfg_attr`.
///
/// Copying `cfg_attr` verbatim would also put unrelated attributes (for example
/// `serde` or `derive`) onto generated impls and match arms.
fn filtered_attributes(attrs: &[Attribute], names: &[&str]) -> syn::Result<Vec<Attribute>> {
    attrs
        .iter()
        .filter_map(|attr| match filtered_meta(&attr.meta, names) {
            Ok(Some(meta)) => Some(Ok(Attribute {
                meta,
                ..attr.clone()
            })),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        })
        .collect()
}

fn filtered_meta(meta: &Meta, names: &[&str]) -> syn::Result<Option<Meta>> {
    if names.iter().any(|name| meta.path().is_ident(name)) {
        return Ok(Some(meta.clone()));
    }
    if !meta.path().is_ident("cfg_attr") {
        return Ok(None);
    }
    let Meta::List(list) = meta else {
        return Err(Error::new_spanned(
            meta,
            "expected cfg_attr(predicate, attribute)",
        ));
    };
    let mut args = list
        .parse_args_with(Punctuated::<Meta, Comma>::parse_terminated)?
        .into_iter();
    let condition = args
        .next()
        .ok_or_else(|| Error::new_spanned(meta, "expected cfg_attr(predicate, attribute)"))?;
    let attrs = args
        .filter_map(|meta| filtered_meta(&meta, names).transpose())
        .collect::<syn::Result<Vec<_>>>()?;
    if attrs.is_empty() {
        Ok(None)
    } else {
        Ok(Some(syn::parse_quote!(cfg_attr(#condition, #(#attrs),*))))
    }
}

/// Generates the `span` method of the message enum.
fn generate_span_impl(
    enum_name: &Ident,
    variants: &[RequestVariant],
    enum_cfg_attrs: &[Attribute],
) -> TokenStream2 {
    let arms = variants.iter().map(|variant| {
        let RequestVariant {
            ident, cfg_attrs, ..
        } = variant;
        quote! {
            #(#cfg_attrs)*
            #enum_name::#ident(ref inner) => &inner.span
        }
    });
    quote! {
        #(#enum_cfg_attrs)*
        impl #enum_name {
            /// Returns the tracing span in which the message was created.
            pub fn span(&self) -> &::irpc::__macro_exports::tracing::Span {
                match *self {
                    #(#arms),*
                }
            }
        }
    }
}

fn generate_channels_impl(
    args: RpcArgs,
    service_name: &Ident,
    variant: &RequestVariant,
) -> TokenStream2 {
    let rx = args.rx.unwrap_or_else(|| {
        syn::parse_str::<Type>(DEFAULT_RX_TYPE).expect("Failed to parse default rx type")
    });
    let tx = args.tx.unwrap_or_else(|| {
        syn::parse_str::<Type>(DEFAULT_TX_TYPE).expect("Failed to parse default tx type")
    });
    let RequestVariant { ty, cfg_attrs, .. } = variant;
    quote! {
        #(#cfg_attrs)*
        impl ::irpc::Channels<#service_name> for #ty {
            type Tx = #tx;
            type Rx = #rx;
        }
    }
}

/// Generates `From` impls for protocol enum variants with an rpc attribute.
fn generate_protocol_enum_from_impls(
    enum_name: &Ident,
    variants: &[RequestVariant],
) -> TokenStream2 {
    let impls = variants.iter().map(|variant| {
        let RequestVariant {
            ident,
            ty,
            cfg_attrs,
        } = variant;
        quote! {
            #(#cfg_attrs)*
            impl From<#ty> for #enum_name {
                fn from(value: #ty) -> Self {
                    #enum_name::#ident(value)
                }
            }
        }
    });
    quote!(#(#impls)*)
}

/// Generate `From<WithChannels<T, Service>>` impls for message enum variants.
fn generate_message_enum_from_impls(
    message_enum_name: &Ident,
    variants: &[RequestVariant],
    service_name: &Ident,
) -> TokenStream2 {
    let impls = variants.iter().map(|variant| {
        let RequestVariant {
            ident,
            ty,
            cfg_attrs,
        } = variant;
        quote! {
            #(#cfg_attrs)*
            impl From<::irpc::WithChannels<#ty, #service_name>> for #message_enum_name {
                fn from(value: ::irpc::WithChannels<#ty, #service_name>) -> Self {
                    #message_enum_name::#ident(value)
                }
            }
        }
    });
    quote!(#(#impls)*)
}

/// Generate `RemoteService` impl for message enums.
fn generate_remote_service_impl(
    message_enum_name: &Ident,
    proto_enum_name: &Ident,
    variants: &[RequestVariant],
    enum_cfg_attrs: &[Attribute],
) -> TokenStream2 {
    let arms = variants.iter().map(|variant| {
        let RequestVariant {
            ident, cfg_attrs, ..
        } = variant;
        quote! {
            #(#cfg_attrs)*
            #proto_enum_name::#ident(msg) => {
                #message_enum_name::from(::irpc::WithChannels::from((msg, tx, rx)))
            }
        }
    });
    quote! {
        #(#enum_cfg_attrs)*
        impl ::irpc::rpc::RemoteService for #proto_enum_name {
            fn with_remote_channels(
                self,
                rx: ::irpc::__macro_exports::RecvStream,
                tx: ::irpc::__macro_exports::SendStream
            ) -> Self::Message {
                match self {
                    #(#arms),*
                }
            }
        }
    }
}

/// Generate type aliases for `WithChannels<T, Service>`.
fn generate_type_aliases(
    variants: &[RequestVariant],
    service_name: &Ident,
    suffix: &str,
    vis: &Visibility,
) -> TokenStream2 {
    let aliases = variants.iter().map(|variant| {
        let RequestVariant {
            ident,
            ty,
            cfg_attrs,
        } = variant;
        let type_ident = Ident::new(&format!("{ident}{suffix}"), ident.span());
        quote! {
            #(#cfg_attrs)*
            /// Type alias for WithChannels<#ty, #service_name>.
            #vis type #type_ident = ::irpc::WithChannels<#ty, #service_name>;
        }
    });
    quote!(#(#aliases)*)
}

// Parse arguments for the macro
#[derive(Default)]
struct MacroArgs {
    message_enum_name: Option<Ident>,
    alias_suffix: Option<String>,
    rpc_feature: Option<String>,
    no_rpc: bool,
    /// When true, includes span context in the wire format and enables span propagation.
    span_propagation: bool,
}

impl Parse for MacroArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut this = Self::default();
        loop {
            let arg: Ident = input.parse()?;
            match arg.to_string().as_str() {
                "message" => {
                    input.parse::<Token![=]>()?;
                    let value: Ident = input.parse()?;
                    this.message_enum_name = Some(value);
                }
                "alias" => {
                    input.parse::<Token![=]>()?;
                    let value: LitStr = input.parse()?;
                    this.alias_suffix = Some(value.value());
                }
                "rpc_feature" => {
                    input.parse::<Token![=]>()?;
                    if this.no_rpc {
                        return syn_err(arg.span(), "rpc_feature is incompatible with no_rpc");
                    }
                    let value: LitStr = input.parse()?;
                    this.rpc_feature = Some(value.value());
                }
                "no_rpc" => {
                    if this.rpc_feature.is_some() {
                        return syn_err(arg.span(), "rpc_feature is incompatible with no_rpc");
                    }
                    this.no_rpc = true;
                }
                "span_propagation" => {
                    this.span_propagation = true;
                }
                _ => {
                    return syn_err(arg.span(), format!("Unknown parameter: {arg}"));
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            } else {
                break;
            }
        }

        Ok(this)
    }
}

#[derive(Default)]
struct VariantRpcArgs {
    wrap: Option<WrapArgs>,
    rpc: Option<RpcArgs>,
}

impl VariantRpcArgs {
    fn from_attrs(attrs: &mut Vec<syn::Attribute>) -> syn::Result<Self> {
        let mut this = Self::default();
        let mut remaining_attrs = Vec::new();
        for attr in attrs.drain(..) {
            let ident = attr.path().get_ident().map(|ident| ident.to_string());
            match ident.as_deref() {
                Some(RPC_ATTR_NAME) => {
                    if this.rpc.is_some() {
                        syn_err(attr.span(), "Each variant can have only one rpc attribute")?;
                    }
                    this.rpc = Some(attr.parse_args()?);
                }
                Some(WRAP_ATTR_NAME) => {
                    if this.wrap.is_some() {
                        syn_err(attr.span(), "Each variant can have only one wrap attribute")?;
                    }
                    this.wrap = Some(attr.parse_args()?);
                }
                _ => remaining_attrs.push(attr),
            }
        }
        *attrs = remaining_attrs;
        Ok(this)
    }
}

#[derive(Default)]
struct RpcArgs {
    rx: Option<Type>,
    tx: Option<Type>,
}

/// Parse the rpc args as a comma separated list of name=type pairs
impl Parse for RpcArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut this = Self::default();
        while !input.is_empty() {
            let arg: Ident = input.parse()?;
            let _: Token![=] = input.parse()?;
            let value: Type = input.parse()?;
            if arg == RX_ATTR {
                this.rx = Some(value);
            } else if arg == TX_ATTR {
                this.tx = Some(value);
            } else {
                syn_err(arg.span(), "Unexpected argument in rpc attribute")?;
            }
            if !input.peek(Token![,]) {
                break;
            } else {
                let _: Token![,] = input.parse()?;
            }
        }

        Ok(this)
    }
}

struct WrapArgs {
    vis: Option<Visibility>,
    ident: Ident,
    derive: Vec<Type>,
}

impl Parse for WrapArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let vis = match input.parse::<Visibility>()? {
            Visibility::Inherited => None,
            vis => Some(vis),
        };
        let ident: Ident = input.parse()?;
        let mut this = Self {
            ident,
            derive: Default::default(),
            vis,
        };
        while input.peek(Token![,]) {
            let _: Token![,] = input.parse()?;
            let arg: Ident = input.parse()?;
            match arg.to_string().as_str() {
                "derive" => {
                    let content;
                    syn::parenthesized!(content in input);
                    let types: Punctuated<Type, Comma> = Punctuated::parse_terminated(&content)?;
                    this.derive = types.into_iter().collect();
                }
                _ => syn_err(arg.span(), "Unexpected argument in wrap argument")?,
            }
        }
        if !input.is_empty() {
            syn_err(input.span(), "Unexpected tokens in wrap argument")?;
        }
        Ok(this)
    }
}

fn type_from_ident(ident: &Ident) -> Type {
    Type::Path(syn::TypePath {
        attrs: Default::default(),
        qself: None,
        path: syn::Path {
            leading_colon: None,
            segments: Punctuated::from_iter([syn::PathSegment::from(ident.clone())]),
        },
    })
}

fn struct_from_variant_fields(
    ident: Ident,
    mut fields: Fields,
    attrs: Vec<Attribute>,
    vis: Visibility,
) -> syn::ItemStruct {
    set_fields_vis(&mut fields, &vis);
    let span = ident.span();
    syn::ItemStruct {
        attrs,
        vis,
        struct_token: Token![struct](span),
        ident,
        generics: Default::default(),
        semi_token: match &fields {
            Fields::Unit => Some(Token![;](span)),
            Fields::Unnamed(_) => Some(Token![;](span)),
            Fields::Named(_) => None,
        },
        fields,
    }
}

fn single_unnamed_field(ty: Type) -> Fields {
    let field = syn::Field {
        attrs: vec![],
        vis: Visibility::Inherited,
        ident: None,
        colon_token: None,
        ty,
        modifiers: Default::default(),
        default: None,
    };
    Fields::Unnamed(syn::FieldsUnnamed {
        paren_token: syn::token::Paren(Span::call_site()),
        unnamed: Punctuated::from_iter([field]),
    })
}

fn set_fields_vis(fields: &mut Fields, vis: &Visibility) {
    let inner = match fields {
        Fields::Named(named) => named.named.iter_mut(),
        Fields::Unnamed(unnamed) => unnamed.unnamed.iter_mut(),
        Fields::Unit => return,
    };
    for field in inner {
        field.vis = vis.clone();
    }
}

fn syn_err<T>(span: Span, message: impl std::fmt::Display) -> syn::Result<T> {
    Err(Error::new(span, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(input: TokenStream2) -> syn::File {
        let input = syn::parse2(input).unwrap();
        let args = syn::parse_quote!(message = Message, alias = "Msg", no_rpc);
        syn::parse2(expand_rpc_requests(input, args).unwrap()).unwrap()
    }

    #[test]
    fn message_inherits_non_exhaustive_attributes() {
        for attr in [
            quote!(#[non_exhaustive]),
            quote!(#[cfg_attr(feature = "extensible", non_exhaustive)]),
            quote!(#[cfg_attr(all(), cfg_attr(all(), non_exhaustive))]),
        ] {
            let file = expand(quote! {
                #attr
                enum Protocol {
                    #[rpc()]
                    Request(Request),
                }
            });
            let message = file
                .items
                .iter()
                .find_map(|item| match item {
                    syn::Item::Enum(item) if item.ident == "Message" => Some(item),
                    _ => None,
                })
                .unwrap();
            let actual = filtered_attributes(&message.attrs, &["non_exhaustive"]).unwrap();
            assert_eq!(quote!(#(#actual)*).to_string(), attr.to_string());
        }
        let file = expand(quote! {
            enum Protocol {
                #[rpc()]
                Request(Request),
            }
        });
        let message = file
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Enum(item) if item.ident == "Message" => Some(item),
                _ => None,
            })
            .unwrap();
        assert!(
            filtered_attributes(&message.attrs, &["non_exhaustive"])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn aliases_inherit_protocol_visibility() {
        for vis in [
            quote!(),
            quote!(pub),
            quote!(pub(crate)),
            quote!(pub(super)),
        ] {
            let file = expand(quote! {
                #vis enum Protocol {
                    #[rpc()]
                    Request(Request),
                }
            });
            let alias = file
                .items
                .iter()
                .find_map(|item| match item {
                    syn::Item::Type(item) if item.ident == "RequestMsg" => Some(item),
                    _ => None,
                })
                .unwrap();
            let actual = &alias.vis;
            assert_eq!(quote!(#actual).to_string(), vis.to_string());
        }
    }

    #[test]
    fn cfg_attr_filters_unrelated_nested_attributes() {
        let input: DeriveInput = syn::parse_quote! {
            #[cfg_attr(all(), doc = "docs", cfg_attr(any(), cfg(unix), derive(Clone)), non_exhaustive)]
            enum Protocol {}
        };
        let attrs = filtered_attributes(&input.attrs, &["cfg"]).unwrap();
        let expected = quote!(#[cfg_attr(all(), cfg_attr(any(), cfg(unix)))]);
        assert_eq!(quote!(#(#attrs)*).to_string(), expected.to_string());
    }
}
