use std::collections::HashSet;

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{quote, ToTokens};
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
    spanned::Spanned,
    token::Comma,
    Attribute, Data, DeriveInput, Error, Fields, Ident, LitStr, Token, Type, Visibility,
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

/// Processes an RPC request enum and generates trait implementations for use with `irpc`.
///
/// This attribute macro may be applied to an enum where each variant represents
/// a different RPC request type. Each variant of the enum must contain a single unnamed field
/// of a distinct type (unless the `wrap` attribute is used on a variant, see below).
///
/// Basic usage example:
/// ```no_compile
/// use serde::{Serialize, Deserialize};
/// use irpc::{rpc_requests, channel::{oneshot, mpsc}};
///
/// #[rpc_requests(message = ComputeMessage)]
/// #[derive(Debug, Serialize, Deserialize)]
/// enum ComputeProtocol {
///     /// Multiply two numbers, return the result over a oneshot channel.
///     #[rpc(tx=oneshot::Sender<i64>)]
///     Multiply(Multiply),
///     /// Sum all numbers received via the `rx` stream,
///     /// reply with the updating sum over the `tx` stream.
///     #[rpc(tx=mpsc::Sender<i64>, rx=mpsc::Receiver<i64>)]
///     Sum(Sum),
/// }
///
/// #[derive(Debug, Serialize, Deserialize)]
/// struct Multiply(i64, i64);
///
/// #[derive(Debug, Serialize, Deserialize)]
/// struct Sum;
/// ```
///
/// ## Generated code
///
/// If no further arguments are set, the macro generates:
///
/// * A `Channels<S>` implementation for each request type (i.e. the type of the variant's
///   single unnamed field).
///   The `Tx` and `Rx` types are set to the types provided via the variant's `rpc` attribute.
/// * A `From` implementation to convert from each request type to the protocol enum.
///
/// When the `message` argument is set, the macro will also create a message enum and implement the
/// `Service` and `RemoteService` traits for the protocol enum. This is recommended for the
/// typical use of the macro.
///
/// ## Macro arguments
///
/// * `message = <name>` *(optional but recommended)*:
///     * Generates an extended enum wrapping each type in `irpc::WithChannels<T, Service>`.
///       The attribute value is the name of the message enum type.
///     * Generates a `irpc::Service` implementation for the protocol enum, with the `Message`
///       type set to the message enum.
///     * Generates a `irpc::rpc::RemoteService` implementation for the protocol enum.
/// * `alias = "<suffix>"` *(optional)*: Generate type aliases with the given suffix for each `WithChannels<T, Service>`.
/// * `rpc_feature = "<feature>"` *(optional)*: If set, the `RemoteService` implementation will be feature-flagged
///   with this feature. Set this if your crate only optionally enables the `rpc` feature
///   of `irpc`.
/// * `no_rpc` *(optional, no value)*: If set, no implementation of `RemoteService` will be generated and the generated
///   code works without the `rpc` feature of `irpc`.
/// * `no_spans` *(optional, no value)*: If set, the generated code works without the `spans` feature of `irpc`.
///
/// ## Variant attributes
///
/// #### `#[rpc]` attribute
///
/// Individual enum variants are annotated with the `#[rpc(...)]` attribute to specify channel types.
/// The `rpc` attribute contains two optional arguments:
///
/// * `tx = SomeType`: Set the kind of channel for sending responses from the server to the client.
///   Must be a `Sender` type from the `irpc::channel` module.
///   If `tx` is not set, it defaults to `irpc::channel::none::NoSender`.
/// * `rx = OtherType`: Set the kind of channel for receiving updates from the client at the server.
///   Must be a `Receiver` type from the `irpc::channel` module.
///   If `rx` is not set, it defaults to `irpc::channel::none::NoReceiver`.
///
/// #### `#[wrap]` attribute
///
/// The attribute has the syntax `#[wrap(TypeName, derive(Foo, Bar))]`
///
/// If set, a struct `TypeName` will be generated from the variant's fields, and the variant
/// will be changed to have a single, unnamed field of `TypeName`.
///
/// * `TypeName` is the name of the generated type.
///   By default it will inherit the visibility of the protocol enum. You can set a different
///   visibility by prefixing it with the visibility (e.g. `pub(crate) TypeName`).
/// * `derive(Foo, Bar)` is optional and allows to set additional derives for the generated struct.
///   By default, the struct will get `Serialize`, `Deserialize`, and `Debug` derives.
///
/// ## Examples
///
/// With `wrap`:
/// ```no_compile
/// use serde::{Serialize, Deserialize};
/// use irpc::{rpc_requests, channel::{oneshot, mpsc}, Client};
///
/// #[rpc_requests(message = StoreMessage)]
/// #[derive(Debug, Serialize, Deserialize)]
/// enum StoreProtocol {
///     /// Doc comment for `GetRequest`.
///     #[rpc(tx=oneshot::Sender<String>)]
///     #[wrap(GetRequest, derive(Clone))]
///     Get(String),
///
///     /// Doc comment for `SetRequest`.
///     #[rpc(tx=oneshot::Sender<()>)]
///     #[wrap(SetRequest)]
///     Set { key: String, value: String }
/// }
///
/// async fn client_usage(client: Client<StoreProtocol>) -> anyhow::Result<()> {
///     client.rpc(SetRequest { key: "foo".to_string(), value: "bar".to_string() }).await?;
///     let value = client.rpc(GetRequest("foo".to_string())).await?;
///     Ok(())
/// }
/// ```
///
/// With type aliases:
/// ```no_compile
/// #[rpc_requests(message = ComputeMessage, alias = "Msg")]
/// enum ComputeProtocol {
///     #[rpc(tx=oneshot::Sender<u128>)]
///     Sqr(Sqr), // Generates type SqrMsg = WithChannels<Sqr, ComputeProtocol>
///     #[rpc(tx=mpsc::Sender<i64>)]
///     Sum(Sum), // Generates type SumMsg = WithChannels<Sum, ComputeProtocol>
/// }
/// ```
#[proc_macro_attribute]
pub fn rpc_requests(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input = parse_macro_input!(item as DeriveInput);
    let args = parse_macro_input!(attr as MacroArgs);

    let enum_name = &input.ident;
    let vis = &input.vis;

    let data_enum = match &mut input.data {
        Data::Enum(data_enum) => data_enum,
        _ => {
            return error_tokens(
                input.span(),
                "The rpc_requests macro can only be applied to enums",
            )
        }
    };

    let cfg_feature_rpc = match args.rpc_feature.as_ref() {
        None => quote!(),
        Some(feature) => quote!(#[cfg(feature = #feature)]),
    };

    // Collect trait implementations
    let mut channel_impls = TokenStream2::new();
    // Types to check for uniqueness
    let mut types = HashSet::new();
    // All variant names and types
    let mut all_variants = Vec::new();
    // Variants with rpc attributes (for From implementations)
    let mut variants_with_attr = Vec::new();
    // Wrapper types (via wrap attribute)
    let mut wrapper_types = TokenStream2::new();

    for variant in &mut data_enum.variants {
        let rpc_attr = match VariantRpcArgs::from_attrs(&mut variant.attrs) {
            Ok(args) => args,
            Err(err) => return err.into_compile_error().into(),
        };

        let request_type = match rpc_attr.wrap {
            None => match &mut variant.fields {
                Fields::Unnamed(ref mut fields) if fields.unnamed.len() == 1 => {
                    fields.unnamed[0].ty.clone()
                }
                _ => return error_tokens(
                    variant.span(),
                    "Each variant must either have exactly one unnamed field, or use the `wrap` argument in the `rpc` attribute.",
                ),
            },
            Some(WrapArgs { ident, derive, vis }) => {
                let vis = vis.as_ref().unwrap_or(&input.vis).clone();
                let ty = type_from_ident(&ident);
                let struc = struct_from_variant_fields(ident, variant.fields.clone(), variant.attrs.clone(), vis);
                wrapper_types.extend(quote! {
                    #[derive(::std::fmt::Debug, ::serde::Serialize, ::serde::Deserialize, #(#derive),* )]
                    #struc
                });
                variant.fields = single_unnamed_field(ty.clone());
                ty
            }
        };

        all_variants.push((variant.ident.clone(), request_type.clone()));

        if !types.insert(request_type.to_token_stream().to_string()) {
            return error_tokens(
                variant.span(),
                "Each variant must have a unique request type",
            );
        }

        if let Some(args) = rpc_attr.rpc {
            variants_with_attr.push((variant.ident.clone(), request_type.clone()));
            channel_impls.extend(generate_channels_impl(args, enum_name, &request_type))
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
                    #variant_name(::irpc::WithChannels<#inner_type, #enum_name>)
                }
            })
            .collect::<Vec<_>>();

        // Extract variant names for the parent_span implementation
        let variant_names: Vec<&Ident> = all_variants.iter().map(|(name, _)| name).collect();

        // Create the message enum definition
        let doc = format!("Message enum for [`{enum_name}`]");
        let message_enum = quote! {
            #[doc = #doc]
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
        #wrapper_types

        // Channel implementations
        #channel_impls

        // From implementations for the original enum
        #protocol_enum_from_impls

        // Type aliases for WithChannels
        #type_aliases

        // Extended enum and its implementations
        #extended_enum_code
    };

    output.into()
}

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
    args: RpcArgs,
    service_name: &Ident,
    request_type: &Type,
) -> TokenStream2 {
    let rx = args.rx.unwrap_or_else(|| {
        // We can safely unwrap here because this is a known valid type
        syn::parse_str::<Type>(DEFAULT_RX_TYPE).expect("Failed to parse default rx type")
    });
    let tx = args.tx.unwrap_or_else(|| {
        // We can safely unwrap here because this is a known valid type
        syn::parse_str::<Type>(DEFAULT_TX_TYPE).expect("Failed to parse default tx type")
    });

    quote! {
        impl ::irpc::Channels<#service_name> for #request_type {
            type Tx = #tx;
            type Rx = #rx;
        }
    }
}

/// Generates `From` impls for protocol enum variants with an rpc attribute.
fn generate_protocol_enum_from_impls(
    enum_name: &Ident,
    variants_with_attr: &[(Ident, Type)],
) -> TokenStream2 {
    variants_with_attr
        .iter()
        .map(|(variant_name, inner_type)| {
            quote! {
                impl From<#inner_type> for #enum_name {
                    fn from(value: #inner_type) -> Self {
                        #enum_name::#variant_name(value)
                    }
                }
            }
        })
        .collect()
}

/// Generate `From<WithChannels<T, Service>>` impls for message enum variants.
fn generate_message_enum_from_impls(
    message_enum_name: &Ident,
    variants_with_attr: &[(Ident, Type)],
    service_name: &Ident,
) -> TokenStream2 {
    variants_with_attr
        .iter()
        .map(|(variant_name, inner_type)| {
            quote! {
                impl From<::irpc::WithChannels<#inner_type, #service_name>> for #message_enum_name {
                    fn from(value: ::irpc::WithChannels<#inner_type, #service_name>) -> Self {
                        #message_enum_name::#variant_name(value)
                    }
                }
            }
        })
        .collect()
}

/// Generate `RemoteService` impl for message enums.
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

/// Generate type aliases for `WithChannels<T, Service>`
fn generate_type_aliases(
    variants: &[(Ident, Type)],
    service_name: &Ident,
    suffix: &str,
) -> TokenStream2 {
    variants
        .iter()
        .map(|(variant_name, inner_type)| {
            // Create a type name using the variant name + suffix
            // For example: Sum + "Msg" = SumMsg
            let type_name = format!("{variant_name}{suffix}");
            let type_ident = Ident::new(&type_name, variant_name.span());
            quote! {
                /// Type alias for WithChannels<#inner_type, #service_name>
                pub type #type_ident = ::irpc::WithChannels<#inner_type, #service_name>;
            }
        })
        .collect()
}

// Parse arguments for the macro
#[derive(Default)]
struct MacroArgs {
    message_enum_name: Option<Ident>,
    alias_suffix: Option<String>,
    rpc_feature: Option<String>,
    no_rpc: bool,
    no_spans: bool,
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
                "no_spans" => {
                    this.no_spans = true;
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
            let ident = attr.path.get_ident().map(|ident| ident.to_string());
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
                    let types: Punctuated<Type, Comma> = content.parse_terminated(Type::parse)?;
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
    };
    Fields::Unnamed(syn::FieldsUnnamed {
        paren_token: syn::token::Paren(Span::call_site()),
        unnamed: Punctuated::from_iter([field]),
    })
}

fn set_fields_vis(fields: &mut Fields, vis: &Visibility) {
    let inner = match fields {
        Fields::Named(ref mut named) => named.named.iter_mut(),
        Fields::Unnamed(ref mut unnamed) => unnamed.unnamed.iter_mut(),
        Fields::Unit => return,
    };
    for field in inner {
        field.vis = vis.clone();
    }
}

// Helper function for error reporting
fn error_tokens(span: Span, message: &str) -> TokenStream {
    Error::new(span, message).to_compile_error().into()
}

fn syn_err<T>(span: Span, message: impl std::fmt::Display) -> syn::Result<T> {
    Err(Error::new(span, message))
}
