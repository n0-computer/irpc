use std::collections::{BTreeMap, HashSet};

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{quote, ToTokens};
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input,
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
            pub fn parent_span(&self) -> tracing::Span {
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

/// Generates From implementations for cases with rpc attributes
fn generate_case_from_impls(
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

/// Generate From implementations for message enum variants
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
        let type_name = format!("{}{}", variant_name, suffix);
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

/// Processes an RPC request enum and generates channel implementations.
///
/// This macro takes a protocol enum where each variant represents a different RPC request type
/// and generates the necessary channel implementations for each request.
///
/// # Macro Arguments
///
/// * First positional argument (required): The service type that will handle these requests
/// * `message` (optional): Generate an extended enum wrapping each type in `WithChannels<T, Service>`
/// * `alias` (optional): Generate type aliases with the given suffix for each `WithChannels<T, Service>`
///
/// # Variant Attributes
///
/// Individual enum variants can be annotated with the `#[rpc(...)]` attribute to specify channel types:
///
/// * `#[rpc(tx=SomeType)]`: Specify the transmitter/sender channel type (required)
/// * `#[rpc(tx=SomeType, rx=OtherType)]`: Also specify a receiver channel type (optional)
///
/// If `rx` is not specified, it defaults to `NoReceiver`.
///
/// # Examples
///
/// Basic usage:
/// ```
/// #[rpc_requests(ComputeService)]
/// enum ComputeProtocol {
///     #[rpc(tx=oneshot::Sender<u128>)]
///     Sqr(Sqr),
///     #[rpc(tx=oneshot::Sender<i64>)]
///     Sum(Sum),
/// }
/// ```
///
/// With a message enum:
/// ```
/// #[rpc_requests(ComputeService, message = ComputeMessage)]
/// enum ComputeProtocol {
///     #[rpc(tx=oneshot::Sender<u128>)]
///     Sqr(Sqr),
///     #[rpc(tx=oneshot::Sender<i64>)]
///     Sum(Sum),
/// }
/// ```
///
/// With type aliases:
/// ```
/// #[rpc_requests(ComputeService, alias = "Msg")]
/// enum ComputeProtocol {
///     #[rpc(tx=oneshot::Sender<u128>)]
///     Sqr(Sqr), // Generates type SqrMsg = WithChannels<Sqr, ComputeService>
///     #[rpc(tx=oneshot::Sender<i64>)]
///     Sum(Sum), // Generates type SumMsg = WithChannels<Sum, ComputeService>
/// }
/// ```
#[proc_macro_attribute]
pub fn rpc_requests(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input = parse_macro_input!(item as DeriveInput);
    let args = parse_macro_input!(attr as MacroArgs);

    let service_name = args.service_name;
    let message_enum_name = args.message_enum_name;
    let alias_suffix = args.alias_suffix;

    let enum_name = &input.ident;
    let input_span = input.span();

    let data_enum = match &mut input.data {
        Data::Enum(data_enum) => data_enum,
        _ => return error_tokens(input.span(), "RpcRequests can only be applied to enums"),
    };

    // Collect trait implementations
    let mut channel_impls = Vec::new();
    // Types to check for uniqueness
    let mut types = HashSet::new();
    // All variant names and types
    let mut all_variants = Vec::new();
    // Variants with rpc attributes (for From implementations)
    let mut variants_with_attr = Vec::new();

    for variant in &mut data_enum.variants {
        // Check field structure for every variant
        let request_type = match &variant.fields {
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => &fields.unnamed[0].ty,
            _ => {
                return error_tokens(
                    variant.span(),
                    "Each variant must have exactly one unnamed field",
                )
            }
        };
        all_variants.push((variant.ident.clone(), request_type.clone()));

        if !types.insert(request_type.to_token_stream().to_string()) {
            return error_tokens(input_span, "Each variant must have a unique request type");
        }

        // Find and remove the rpc attribute
        let mut rpc_attr = None;
        let mut multiple_rpc_attrs = false;

        variant.attrs.retain(|attr| {
            if attr.path.is_ident(ATTR_NAME) {
                if rpc_attr.is_some() {
                    multiple_rpc_attrs = true;
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
        if multiple_rpc_attrs {
            return error_tokens(
                variant.span(),
                "Each variant can only have one rpc attribute",
            );
        }

        // Process variants with rpc attributes
        if let Some(attr) = rpc_attr {
            variants_with_attr.push((variant.ident.clone(), request_type.clone()));

            let args = match attr.parse_args::<NamedTypeArgs>() {
                Ok(info) => info,
                Err(e) => return e.to_compile_error().into(),
            };

            match generate_channels_impl(args, &service_name, request_type, attr.span()) {
                Ok(impls) => channel_impls.push(impls),
                Err(e) => return e.to_compile_error().into(),
            }
        }
    }

    // Generate From implementations for the original enum (only for variants with rpc attributes)
    let original_from_impls = generate_case_from_impls(enum_name, &variants_with_attr);

    // Generate type aliases if requested
    let type_aliases = if let Some(suffix) = alias_suffix {
        // Use all variants for type aliases, not just those with rpc attributes
        generate_type_aliases(&all_variants, &service_name, &suffix)
    } else {
        quote! {}
    };

    // Generate the extended message enum if requested
    let extended_enum_code = if let Some(message_enum_name) = message_enum_name {
        let message_variants = all_variants
            .iter()
            .map(|(variant_name, inner_type)| {
                quote! {
                    #[allow(missing_docs)]
                    #variant_name(::irpc::WithChannels<#inner_type, #service_name>)
                }
            })
            .collect::<Vec<_>>();

        // Extract variant names for the parent_span implementation
        let variant_names: Vec<&Ident> = all_variants.iter().map(|(name, _)| name).collect();

        // Create the message enum definition
        let message_enum = quote! {
            #[allow(missing_docs)]
            #[derive(Debug)]
            pub enum #message_enum_name {
                #(#message_variants),*
            }
        };

        // Generate parent_span method
        let parent_span_impl = generate_parent_span_impl(&message_enum_name, &variant_names);

        // Generate From implementations for the message enum (only for variants with rpc attributes)
        let message_from_impls = generate_message_enum_from_impls(
            &message_enum_name,
            &variants_with_attr,
            &service_name,
        );

        quote! {
            #message_enum
            #parent_span_impl
            #message_from_impls
        }
    } else {
        // If no message_enum_name is provided, don't generate the extended enum
        quote! {}
    };

    // Combine everything
    let output = quote! {
        #input

        // Channel implementations
        #(#channel_impls)*

        // From implementations for the original enum
        #original_from_impls

        // Type aliases for WithChannels
        #type_aliases

        // Extended enum and its implementations
        #extended_enum_code
    };

    output.into()
}

// Parse arguments for the macro
struct MacroArgs {
    service_name: Ident,
    message_enum_name: Option<Ident>,
    alias_suffix: Option<String>,
}

impl Parse for MacroArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        // First argument must be the service name (positional)
        let service_name: Ident = input.parse()?;

        // Initialize optional parameters
        let mut message_enum_name = None;
        let mut alias_suffix = None;

        // Parse any additional named parameters
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            let param_name: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            match param_name.to_string().as_str() {
                "message" => {
                    message_enum_name = Some(input.parse()?);
                }
                "alias" => {
                    let lit: LitStr = input.parse()?;
                    alias_suffix = Some(lit.value());
                }
                _ => {
                    return Err(syn::Error::new(
                        param_name.span(),
                        format!("Unknown parameter: {}", param_name),
                    ));
                }
            }
        }

        Ok(MacroArgs {
            service_name,
            message_enum_name,
            alias_suffix,
        })
    }
}

struct NamedTypeArgs {
    types: BTreeMap<String, Type>,
}

impl NamedTypeArgs {
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

        loop {
            if input.is_empty() {
                break;
            }

            let key: Ident = input.parse()?;
            let _: Token![=] = input.parse()?;
            let value: Type = input.parse()?;

            types.insert(key.to_string(), value);

            if !input.peek(Token![,]) {
                break;
            }
            let _: Token![,] = input.parse()?;
        }

        Ok(NamedTypeArgs { types })
    }
}
