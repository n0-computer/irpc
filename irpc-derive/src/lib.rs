use std::collections::{BTreeMap, HashSet};

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{quote, ToTokens};
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
    spanned::Spanned,
    Data, DeriveInput, Fields, Ident, LitStr, Token, Type,
};

// Helper function for error reporting
fn error_tokens(span: Span, message: &str) -> TokenStream {
    syn::Error::new(span, message).to_compile_error().into()
}

/// The only attribute we care about
const ATTR_NAME: &str = "rpc";
/// the tx type name
const TX_ATTR: &str = "tx";
/// the rx type name
const RX_ATTR: &str = "rx";
/// Fully qualified path to the default rx type
const DEFAULT_RX_TYPE: &str = "::irpc::channel::none::NoReceiver";

/// Generate parent span method for an enum
fn generate_parent_span_impl(enum_name: &Ident, variant_names: &[&Ident]) -> TokenStream2 {
    quote! {
        impl #enum_name {
            /// Get the parent span of the message
            pub fn parent_span(&self) -> ::tracing::Span {
                let span = match self {
                    #(#enum_name::#variant_names(inner) => inner.parent_span_opt()),*
                };
                span.cloned().unwrap_or_else(|| ::tracing::Span::current())
            }
        }
    }
}

fn generate_channels_impl(
    mut args: NamedTypeArgs,
    service_name: &Ident,
    request_type: &Type,
    attr_span: Span,
) -> syn::Result<TokenStream2> {
    // Try to get rx, default to NoReceiver if not present
    // Use unwrap_or_else for a cleaner default
    let rx = args.types.remove(RX_ATTR).unwrap_or_else(|| {
        // We can safely unwrap here because this is a known valid type
        syn::parse_str::<Type>(DEFAULT_RX_TYPE).expect("Failed to parse default rx type")
    });
    let tx = args.get(TX_ATTR, attr_span)?;

    let res = quote! {
        impl ::irpc::Channels<#service_name> for #request_type {
            type Tx = #tx;
            type Rx = #rx;
        }
    };

    args.check_empty(attr_span)?;
    Ok(res)
}

/// Generates From implementations for protocol enum variants.
fn generate_protocol_enum_from_impls(
    enum_name: &Ident,
    variants_with_attr: &[(Ident, Type)],
) -> TokenStream2 {
    let mut impls = quote! {};

    // Generate From implementations for each case that has an rpc attribute
    for (variant_name, inner_type) in variants_with_attr {
        let impl_tokens = quote! {
            impl From<#inner_type> for #enum_name {
                fn from(value: #inner_type) -> Self {
                    #enum_name::#variant_name(value)
                }
            }
        };

        impls = quote! {
            #impls
            #impl_tokens
        };
    }

    impls
}

/// Generates From implementations for message enum variants.
fn generate_message_enum_from_impls(
    message_enum_name: &Ident,
    variants_with_attr: &[(Ident, Type)],
    service_name: &Ident,
) -> TokenStream2 {
    let mut impls = quote! {};

    // Generate From<WithChannels<T, Service>> implementations for each case with an rpc attribute
    for (variant_name, inner_type) in variants_with_attr {
        let impl_tokens = quote! {
            impl From<::irpc::WithChannels<#inner_type, #service_name>> for #message_enum_name {
                fn from(value: ::irpc::WithChannels<#inner_type, #service_name>) -> Self {
                    #message_enum_name::#variant_name(value)
                }
            }
        };

        impls = quote! {
            #impls
            #impl_tokens
        };
    }

    impls
}

/// Generate Message::from_quic_streams impl
fn generate_remote_service_impl(
    message_enum_name: &Ident,
    proto_enum_name: &Ident,
    variants_with_attr: &[(Ident, Type)],
) -> TokenStream2 {
    let variants = variants_with_attr
        .iter()
        .map(|(variant_name, _inner_type)| {
            quote! {
                #proto_enum_name::#variant_name(msg) => {
                    #message_enum_name::from(::irpc::WithChannels::from((msg, tx, rx)))
                }
            }
        });

    quote! {
        impl ::irpc::rpc::RemoteService for #proto_enum_name {
            fn with_remote_channels(
                self,
                rx: ::irpc::rpc::quinn::RecvStream,
                tx: ::irpc::rpc::quinn::SendStream
            ) -> Self::Message {
                match self {
                    #(#variants),*
                }
            }
        }
    }
}

/// Generate type aliases for WithChannels<T, Service>
fn generate_type_aliases(
    variants: &[(Ident, Type)],
    service_name: &Ident,
    suffix: &str,
) -> TokenStream2 {
    let mut aliases = quote! {};

    for (variant_name, inner_type) in variants {
        // Create a type name using the variant name + suffix
        // For example: Sum + "Msg" = SumMsg
        let type_name = format!("{variant_name}{suffix}");
        let type_ident = Ident::new(&type_name, variant_name.span());

        let alias = quote! {
            /// Type alias for WithChannels<#inner_type, #service_name>
            pub type #type_ident = ::irpc::WithChannels<#inner_type, #service_name>;
        };

        aliases = quote! {
            #aliases
            #alias
        };
    }

    aliases
}

#[proc_macro_attribute]
pub fn rpc_requests(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input = parse_macro_input!(item as DeriveInput);
    let args = parse_macro_input!(attr as MacroArgs);

    let enum_name = &input.ident;
    let vis = &input.vis;
    let input_span = input.span();
    let cfg_feature_rpc = match args.rpc_feature.as_ref() {
        None => quote!(),
        Some(feature) => quote!(#[cfg(feature = #feature)]),
    };

    let data_enum = match &mut input.data {
        Data::Enum(data_enum) => data_enum,
        _ => {
            return error_tokens(
                input.span(),
                "The rpc_requests macro can only be applied to enums",
            )
        }
    };

    // Collect trait implementations
    let mut channel_impls = Vec::new();
    // Types to check for uniqueness
    let mut types = HashSet::new();
    // All variant names and types
    let mut all_variants = Vec::new();
    // Variants with rpc attributes (for From implementations)
    let mut variants_with_attr = Vec::new();

    let mut wrapper_types = Vec::new();

    for variant in &mut data_enum.variants {
        let rpc_attr = match NamedTypeArgs::from_attrs(&mut variant.attrs) {
            Ok(args) => args,
            Err(err) => return err,
        };

        let wrap = rpc_attr
            .as_ref()
            .map(|(args, _)| args.wrap.as_ref())
            .flatten()
            .map(|name| name.clone().unwrap_or_else(|| variant.ident.clone()));

        let request_type = match wrap {
            None => match &mut variant.fields {
                Fields::Unnamed(ref mut fields) if fields.unnamed.len() == 1 => {
                    fields.unnamed[0].ty.clone()
                }
                _ => return error_tokens(
                    variant.span(),
                    "Each variant must either have exactly one unnamed field, or use the `wrap` argument in the `rpc` attribute.",
                ),
            },
            Some(name) => {
                let struc = struct_from_variant_fields(name.clone(), variant.fields.clone());
                wrapper_types.push(quote! {
                    #[derive(::std::fmt::Debug, ::serde::Serialize, ::serde::Deserialize)]
                    #struc
                });
                let ty = type_from_ident(name);
                variant.fields = single_unnamed_field(ty.clone());
                ty
            }
        };

        all_variants.push((variant.ident.clone(), request_type.clone()));

        if !types.insert(request_type.to_token_stream().to_string()) {
            return error_tokens(input_span, "Each variant must have a unique request type");
        }

        if let Some((args, attr)) = rpc_attr {
            variants_with_attr.push((variant.ident.clone(), request_type.clone()));

            match generate_channels_impl(args, enum_name, &request_type, attr.span()) {
                Ok(impls) => channel_impls.push(impls),
                Err(e) => return e.to_compile_error().into(),
            }
        }
    }

    // Generate From implementations for the original enum (only for variants with rpc attributes)
    let protocol_enum_from_impls =
        generate_protocol_enum_from_impls(enum_name, &variants_with_attr);

    // Generate type aliases if requested
    let type_aliases = if let Some(suffix) = args.alias_suffix {
        // Use all variants for type aliases, not just those with rpc attributes
        generate_type_aliases(&all_variants, enum_name, &suffix)
    } else {
        quote! {}
    };

    // Generate the extended message enum if requested
    let extended_enum_code = if let Some(message_enum_name) = args.message_enum_name.as_ref() {
        let message_variants = all_variants
            .iter()
            .map(|(variant_name, inner_type)| {
                quote! {
                    #[allow(missing_docs)]
                    #variant_name(::irpc::WithChannels<#inner_type, #enum_name>)
                }
            })
            .collect::<Vec<_>>();

        // Extract variant names for the parent_span implementation
        let variant_names: Vec<&Ident> = all_variants.iter().map(|(name, _)| name).collect();

        // Create the message enum definition
        let message_enum = quote! {
            #[allow(missing_docs)]
            #[derive(::std::fmt::Debug)]
            #vis enum #message_enum_name {
                #(#message_variants),*
            }
        };

        // Generate parent_span method
        let parent_span_impl = if !args.no_spans {
            generate_parent_span_impl(message_enum_name, &variant_names)
        } else {
            quote! {}
        };

        // Generate From implementations for the message enum (only for variants with rpc attributes)
        let message_from_impls =
            generate_message_enum_from_impls(message_enum_name, &variants_with_attr, enum_name);

        let service_impl = quote! {
            impl ::irpc::Service for #enum_name {
                type Message = #message_enum_name;
            }
        };

        let remote_service_impl = if !args.no_rpc {
            let block =
                generate_remote_service_impl(message_enum_name, enum_name, &variants_with_attr);
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
            #parent_span_impl
            #message_from_impls
        }
    } else {
        quote! {}
    };

    // Combine everything
    let output = quote! {
        #input

        // Wrapper types
        #(#wrapper_types)*

        // Channel implementations
        #(#channel_impls)*

        // From implementations for the original enum
        #protocol_enum_from_impls

        // Type aliases for WithChannels
        #type_aliases

        // Extended enum and its implementations
        #extended_enum_code
    };

    output.into()
}

// Parse arguments for the macro
struct MacroArgs {
    message_enum_name: Option<Ident>,
    alias_suffix: Option<String>,
    rpc_feature: Option<String>,
    no_rpc: bool,
    no_spans: bool,
}

impl Parse for MacroArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        // Initialize optional parameters
        let mut message_enum_name = None;
        let mut alias_suffix = None;
        let mut rpc_feature = None;
        let mut no_rpc = false;
        let mut no_spans = false;

        // Parse names parameters.
        loop {
            let param_name: Ident = input.parse()?;

            match param_name.to_string().as_str() {
                "message" => {
                    input.parse::<Token![=]>()?;
                    let ident: Ident = input.parse()?;
                    message_enum_name = Some(ident);
                }
                "alias" => {
                    input.parse::<Token![=]>()?;
                    let lit: LitStr = input.parse()?;
                    alias_suffix = Some(lit.value());
                }
                "rpc_feature" => {
                    input.parse::<Token![=]>()?;
                    if no_rpc {
                        return Err(syn::Error::new(
                            param_name.span(),
                            "rpc_feature is incompatible with no_rpc",
                        ));
                    }
                    let lit: LitStr = input.parse()?;
                    rpc_feature = Some(lit.value());
                }
                "no_rpc" => {
                    if rpc_feature.is_some() {
                        return Err(syn::Error::new(
                            param_name.span(),
                            "rpc_feature is incompatible with no_rpc",
                        ));
                    }
                    no_rpc = true;
                }
                "no_spans" => {
                    no_spans = true;
                }
                _ => {
                    return Err(syn::Error::new(
                        param_name.span(),
                        format!("Unknown parameter: {param_name}"),
                    ));
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            } else {
                break;
            }
        }

        Ok(MacroArgs {
            message_enum_name,
            alias_suffix,
            no_rpc,
            no_spans,
            rpc_feature,
        })
    }
}

struct NamedTypeArgs {
    wrap: Option<Option<Ident>>,
    types: BTreeMap<String, Type>,
}

impl NamedTypeArgs {
    fn from_attrs(
        attrs: &mut Vec<syn::Attribute>,
    ) -> Result<Option<(Self, syn::Attribute)>, TokenStream> {
        let mut rpc_attr = None;
        let mut multiple_rpc_attrs = None;

        attrs.retain(|attr| {
            if attr.path.is_ident(ATTR_NAME) {
                if rpc_attr.is_some() {
                    multiple_rpc_attrs = Some(attr.span());
                    true // Keep this duplicate attribute
                } else {
                    rpc_attr = Some(attr.clone());
                    false // Remove this attribute
                }
            } else {
                true // Keep other attributes
            }
        });

        // Check for multiple rpc attributes
        if let Some(span) = multiple_rpc_attrs {
            return Err(error_tokens(
                span,
                "Each variant can only have one rpc attribute",
            ));
        }

        // Process variants with rpc attributes
        if let Some(attr) = rpc_attr {
            let args = match attr.parse_args::<NamedTypeArgs>() {
                Ok(info) => info,
                Err(e) => return Err(e.to_compile_error().into()),
            };
            Ok(Some((args, attr)))
        } else {
            Ok(None)
        }
    }
    /// Get and remove a type from the map, failing if it doesn't exist
    fn get(&mut self, key: &str, span: Span) -> syn::Result<Type> {
        self.types
            .remove(key)
            .ok_or_else(|| syn::Error::new(span, format!("rpc requires a {key} type")))
    }

    /// Fail if there are any unknown arguments remaining
    fn check_empty(&self, span: Span) -> syn::Result<()> {
        if self.types.is_empty() {
            Ok(())
        } else {
            Err(syn::Error::new(
                span,
                format!(
                    "Unknown arguments provided: {:?}",
                    self.types.keys().collect::<Vec<_>>()
                ),
            ))
        }
    }
}

/// Parse the rpc args as a comma separated list of name=type pairs
impl Parse for NamedTypeArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut types = BTreeMap::new();
        let mut wrap = None;

        loop {
            if input.is_empty() {
                break;
            }

            let key: Ident = input.parse()?;
            if key == "wrap" {
                let value = if input.peek(Token![=]) {
                    let _: Token![=] = input.parse()?;
                    let value: Ident = input.parse()?;
                    Some(value)
                } else {
                    None
                };
                wrap = Some(value);
            } else {
                let _: Token![=] = input.parse()?;
                let value: Type = input.parse()?;
                types.insert(key.to_string(), value);
            }

            if !input.peek(Token![,]) {
                break;
            }
            let _: Token![,] = input.parse()?;
        }

        Ok(NamedTypeArgs { types, wrap })
    }
}

fn type_from_ident(ident: Ident) -> Type {
    Type::Path(syn::TypePath {
        qself: None,
        path: syn::Path {
            leading_colon: None,
            segments: {
                let mut segments = syn::punctuated::Punctuated::new();
                segments.push(syn::PathSegment {
                    ident,
                    arguments: syn::PathArguments::None,
                });
                segments
            },
        },
    })
}
fn struct_from_variant_fields(name: Ident, mut fields: Fields) -> syn::ItemStruct {
    make_fields_public(&mut fields);
    let span = name.span();
    syn::ItemStruct {
        attrs: vec![],
        vis: vis_pub(),
        struct_token: Token![struct](span),
        ident: name.clone(),
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
        vis: syn::Visibility::Inherited,
        ident: None,
        colon_token: None,
        ty,
    };
    Fields::Unnamed(syn::FieldsUnnamed {
        paren_token: syn::token::Paren(Span::call_site()),
        unnamed: Punctuated::from_iter([field]),
    })
}

fn make_fields_public(fields: &mut Fields) {
    let inner = match fields {
        Fields::Named(ref mut named) => named.named.iter_mut(),
        Fields::Unnamed(ref mut unnamed) => unnamed.unnamed.iter_mut(),
        Fields::Unit => return,
    };
    for field in inner {
        field.vis = vis_pub();
    }
}

fn vis_pub() -> syn::Visibility {
    syn::Visibility::Public(syn::VisPublic {
        pub_token: syn::token::Pub {
            span: proc_macro2::Span::call_site(),
        },
    })
}
