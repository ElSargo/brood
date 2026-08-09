use crate::policy_provider::{
    PolicyProviderDeclaration, PolicyProviderKind, is_valid_declaration_name,
};

pub const POLICY_PROVIDER_EXPORT_PREFIX: &str = "brood_policy_provider_";

pub fn parse_policy_provider_export_tail(tail: &str) -> Option<(PolicyProviderKind, String)> {
    for (prefix, kind) in [
        ("key_", PolicyProviderKind::Key),
        ("pointer_", PolicyProviderKind::Pointer),
        ("map_", PolicyProviderKind::Map),
        ("focus_", PolicyProviderKind::Focus),
    ] {
        let Some(name) = tail.strip_prefix(prefix) else {
            continue;
        };
        if is_valid_declaration_name(name) {
            return Some((kind, name.to_string()));
        }
    }
    None
}

pub fn parse_policy_provider_export(export_name: &str) -> Option<PolicyProviderDeclaration> {
    let tail = export_name.strip_prefix(POLICY_PROVIDER_EXPORT_PREFIX)?;
    let (kind, name) = parse_policy_provider_export_tail(tail)?;
    Some(PolicyProviderDeclaration {
        name,
        kind,
        export_name: export_name.to_string(),
    })
}
