//! Authentication module for Microsoft Teams
//!
//! Implements OAuth2 device code flow for Azure AD authentication,
//! then exchanges the AAD token for a Skype token.

pub mod oauth;
pub mod skype;
pub mod tokens;

pub use oauth::{login, logout, status};
pub use tokens::{StoredToken, TokenStore};

fn configured_work_client_id() -> &'static str {
    static CLIENT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CLIENT_ID.get_or_init(|| {
        ["TEAMS_LINUX_WORK_CLIENT_ID", "TEAMS_LINUX_CLIENT_ID"]
            .into_iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.trim().is_empty())
            .unwrap_or_default()
    })
}

/// Azure AD client configuration for Teams
pub struct AuthConfig {
    /// OAuth2 client ID (public client)
    pub client_id: &'static str,
    /// OAuth2 redirect URI
    pub redirect_uri: &'static str,
    /// Azure AD tenant (common for multi-tenant)
    pub tenant: &'static str,
    /// Primary resource scope
    pub scope: &'static str,
}

impl AuthConfig {
    /// Config for work/school accounts (client ID supplied by the Linux host).
    pub fn work() -> Self {
        Self {
            client_id: configured_work_client_id(),
            redirect_uri: "https://login.microsoftonline.com/common/oauth2/nativeclient",
            tenant: "common",
            scope: "https://api.spaces.skype.com/.default offline_access",
        }
    }

}

impl Default for AuthConfig {
    fn default() -> Self {
        Self::work()
    }
}
