use super::{AccessToken, TeamsSession};

/// Call services negotiate a token type for a specific source. Keep these
/// separate from Graph so a Graph grant can never satisfy a call request.
#[allow(dead_code)] // The native call transport consumes this port in CALL-02.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CallingTokenSource {
    TrouterCallingWeb,
    CallStore,
    MediaRelay,
}

#[allow(dead_code)] // CAE remains explicit even though this session cannot mint it yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CallingTokenType {
    Aad,
    Cae,
    Skype,
}

#[allow(dead_code)] // Constructed by the call transport from its source negotiation.
pub(crate) struct CallingTokenRequest<'a> {
    pub source: CallingTokenSource,
    /// Ordered by the call service; the first available credential wins.
    pub supported_types: &'a [CallingTokenType],
    /// Opaque WWW-Authenticate value. Never logged or included in errors.
    pub challenge: Option<&'a str>,
    pub force_renew: bool,
}

#[allow(dead_code)] // Returned to the native call transport; Debug redacts the secret.
pub(crate) struct CallingToken {
    pub source: CallingTokenSource,
    pub token_type: CallingTokenType,
    value: String,
    pub expires_in_secs: u64,
}

impl CallingToken {
    #[allow(dead_code)]
    pub(crate) fn expose_to_transport(&self) -> &str {
        &self.value
    }

    pub(crate) fn trouter_type_name(&self) -> &'static str {
        match self.token_type {
            CallingTokenType::Aad => "aad",
            CallingTokenType::Cae => "cae",
            CallingTokenType::Skype => "skype",
        }
    }
}

impl std::fmt::Debug for CallingToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallingToken")
            .field("source", &self.source)
            .field("token_type", &self.token_type)
            .field("value", &"[REDACTED]")
            .field("expires_in_secs", &self.expires_in_secs)
            .finish()
    }
}

#[allow(dead_code)] // Surfaced by CALL-02 when challenge-aware renewal is added.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CallingTokenError {
    SessionUnavailable,
    ClaimsChallengeUnsupported,
    NoSupportedCredential,
    RenewalFailed(String),
}

impl std::fmt::Display for CallingTokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionUnavailable => f.write_str("Não foi possível acessar a sessão."),
            Self::ClaimsChallengeUnsupported => {
                f.write_str("Este desafio de chamada ainda não pode ser renovado.")
            }
            Self::NoSupportedCredential => {
                f.write_str("A sessão não tem um tipo de token aceito por esta chamada.")
            }
            Self::RenewalFailed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CallingTokenError {}

impl TeamsSession {
    /// Pinned commercial work/school profile observed in the original config.
    /// Other authentication clouds never inherit a public-cloud media service.
    pub(crate) fn native_stun_service(&self)->Option<String> {
        (self.config.authz_url=="https://teams.microsoft.com/api/authsvc/v1.0/authz")
            .then(||crate::call_relay::STUN_SERVICE.into())
    }

    pub(crate) fn native_relay_request(&self)->Result<crate::call_relay::RelayRequest,crate::call_relay::RelayError>{
        let token=self.calling_token(CallingTokenRequest{source:CallingTokenSource::MediaRelay,supported_types:&[CallingTokenType::Skype],challenge:None,force_renew:false}).map_err(|_|crate::call_relay::RelayError::Credential)?;
        crate::call_relay::RelayRequest::commercial(&self.config.authz_url,token)
    }

    /// Reuse this authenticated Teams session and its regional discovery only.
    /// No Graph grant, browser, hardcoded cloud or application identity.
    pub(crate) fn native_meeting_transport(
        &self,
    ) -> Result<
        (
            crate::call_join::CallRoute,
            crate::call_session::CallTransport,
        ),
        String,
    > {
        let token = self
            .calling_token(CallingTokenRequest {
                source: CallingTokenSource::TrouterCallingWeb,
                supported_types: &[CallingTokenType::Skype],
                challenge: None,
                force_renew: false,
            })
            .map_err(|e| e.to_string())?;
        let table = self
            .tokens
            .lock()
            .map_err(|_| "Sessão de chamada indisponível.")?
            .region_gtms
            .clone()
            .ok_or("A sessão não retornou a rota regional de chamadas.")?;
        let route = crate::call_join::CallRoute::from_discovery(&table)
            .map_err(|_| "Rota regional de chamada inválida.")?;
        let transport = route
            .transport(token)
            .map_err(|_| "Credencial de chamada indisponível.")?;
        Ok((route, transport))
    }

    /// Resolve a call token using the ordered types negotiated by the source.
    /// Graph and IC3 credentials are intentionally outside this provider.
    #[allow(dead_code)] // Wired into the Linux call transport in CALL-02.
    pub(crate) fn calling_token(
        &self,
        request: CallingTokenRequest<'_>,
    ) -> Result<CallingToken, CallingTokenError> {
        if request.challenge.is_some() {
            return Err(CallingTokenError::ClaimsChallengeUnsupported);
        }

        let mut tokens = self
            .tokens
            .lock()
            .map_err(|_| CallingTokenError::SessionUnavailable)?;
        let fallback = tokens.call_token_fallbacks.get(&request.source).copied();
        let fallback_limit = fallback
            .and_then(|fallback| {
                request
                    .supported_types
                    .iter()
                    .position(|token_type| *token_type == fallback)
            })
            .map(|index| index + 1)
            .unwrap_or(request.supported_types.len());
        for token_type in &request.supported_types[..fallback_limit] {
            let (value, expires_in_secs) = match token_type {
                CallingTokenType::Aad => {
                    if request.force_renew || !tokens.aad.valid() {
                        tokens.aad = self
                            .refresh_grant(&mut tokens, &self.config.scope, "Token AAD de chamada")
                            .map_err(CallingTokenError::RenewalFailed)?;
                    }
                    (tokens.aad.value.clone(), tokens.aad.remaining())
                }
                CallingTokenType::Skype => {
                    if request.force_renew {
                        tokens.skype = None;
                    }
                    if !tokens.skype.as_ref().is_some_and(AccessToken::valid) {
                        self.ensure_skype(&mut tokens)
                            .map_err(CallingTokenError::RenewalFailed)?;
                    }
                    let token = tokens
                        .skype
                        .as_ref()
                        .filter(|token| token.valid())
                        .ok_or(CallingTokenError::NoSupportedCredential)?;
                    (token.value.clone(), token.remaining())
                }
                // CAE requires a claims-aware acquisition path; never substitute AAD or Skype.
                CallingTokenType::Cae => continue,
            };

            return Ok(CallingToken {
                source: request.source,
                token_type: *token_type,
                value,
                expires_in_secs,
            });
        }

        Err(CallingTokenError::NoSupportedCredential)
    }

    /// Remember a type selected after a claims challenge was resolved for this
    /// source. The next request applies it as an ordered fallback limit. The
    /// call transport must invoke this only after resolving the challenge; the
    /// source and opaque challenge itself are never persisted.
    #[allow(dead_code)] // Called by CALL-02 when challenge-aware acquisition is integrated.
    pub(crate) fn remember_call_token_fallback(
        &self,
        source: CallingTokenSource,
        token_type: CallingTokenType,
    ) -> Result<(), CallingTokenError> {
        let mut tokens = self
            .tokens
            .lock()
            .map_err(|_| CallingTokenError::SessionUnavailable)?;
        tokens.call_token_fallbacks.insert(source, token_type);
        Ok(())
    }
}
