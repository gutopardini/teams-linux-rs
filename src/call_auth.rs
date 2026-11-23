//! Native adapter for the Trouter authentication callback contract.
//!
//! The service supplies its supported token types on each request. Keep their
//! order intact and pass the challenge/refresh signals to the session provider.

use crate::auth::TeamsSession;
use crate::auth::calling::{
    CallingToken, CallingTokenError, CallingTokenRequest, CallingTokenSource, CallingTokenType,
};

pub(crate) struct TrouterAuthRequest<'a> {
    /// Wire names supplied by Trouter, in the order it prefers.
    pub supported_token_types: &'a [&'a str],
    /// Opaque WWW-Authenticate header. The session provider currently fails
    /// closed until claims-aware acquisition is implemented.
    pub www_authenticate_header: Option<&'a str>,
    pub need_fresh: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TrouterAuthError {
    UnsupportedTokenType,
    Provider(CallingTokenError),
}

impl std::fmt::Display for TrouterAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedTokenType => {
                f.write_str("O Trouter solicitou um tipo de token não suportado.")
            }
            Self::Provider(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for TrouterAuthError {}

#[allow(dead_code)] // Returned to the native Trouter callback when transport wiring lands.
pub(crate) struct TrouterAuthToken {
    pub token_type: &'static str,
    token: CallingToken,
}

impl TrouterAuthToken {
    #[allow(dead_code)]
    pub(crate) fn expose_to_transport(&self) -> &str {
        self.token.expose_to_transport()
    }
}

impl std::fmt::Debug for TrouterAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrouterAuthToken")
            .field("token_type", &self.token_type)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

fn map_supported_types(wire_names: &[&str]) -> Result<Vec<CallingTokenType>, TrouterAuthError> {
    wire_names
        .iter()
        .map(|wire_name| match *wire_name {
            "aad" => Ok(CallingTokenType::Aad),
            "cae" => Ok(CallingTokenType::Cae),
            "skype" => Ok(CallingTokenType::Skype),
            _ => Err(TrouterAuthError::UnsupportedTokenType),
        })
        .collect()
}

/// Resolve credentials for Trouter without substituting Graph or changing the
/// preference order supplied by its transport callback.
#[allow(dead_code)] // Used when the native Trouter transport exposes authTokenProvider.
pub(crate) fn resolve_trouter_token(
    session: &TeamsSession,
    request: TrouterAuthRequest<'_>,
) -> Result<TrouterAuthToken, TrouterAuthError> {
    let supported_types = map_supported_types(request.supported_token_types)?;

    let token = session
        .calling_token(CallingTokenRequest {
            source: CallingTokenSource::TrouterCallingWeb,
            supported_types: &supported_types,
            challenge: request.www_authenticate_header,
            force_renew: request.need_fresh,
        })
        .map_err(TrouterAuthError::Provider)?;

    Ok(TrouterAuthToken {
        token_type: token.trouter_type_name(),
        token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_exact_wire_names_and_preserves_server_order_and_duplicates() {
        let names = ["cae", "skype", "aad", "skype"];

        assert_eq!(
            map_supported_types(&names).unwrap(),
            vec![
                CallingTokenType::Cae,
                CallingTokenType::Skype,
                CallingTokenType::Aad,
                CallingTokenType::Skype,
            ]
        );
    }

    #[test]
    fn unknown_wire_name_fails_closed_instead_of_being_dropped() {
        let names = ["aad", "Skype"];

        assert_eq!(
            map_supported_types(&names),
            Err(TrouterAuthError::UnsupportedTokenType)
        );
    }
}
