//! Anonymous catalog images. Public DNS addresses are pinned before each TLS hop.
use std::{net::{IpAddr, SocketAddr}, time::Duration};
use url::Url;

const MAX_BYTES: usize = 2 * 1024 * 1024;

pub fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !(ip.is_private() || ip.is_loopback() || ip.is_link_local()
                || ip.is_unspecified() || ip.is_broadcast() || ip.is_documentation()
                || a == 0 || a >= 224 || (a == 100 && (64..=127).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 192 && (b == 0 || b == 88)))
        }
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() { return public_address(IpAddr::V4(ip)); }
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000 && s[0] != 0x2002
                && s[0] != 0x3fff
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
        }
    }
}

pub fn validated_url(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    let host = url.host_str()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some()
        || url.port_or_known_default() != Some(443) || host == "localhost"
        || host.ends_with(".localhost") || host.ends_with(".local") { return None; }
    if let Some(host) = url.host() {
        let literal = match host {
            url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
            url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
            _ => None,
        };
        if literal.is_some_and(|ip| !public_address(ip)) { return None; }
    }
    Some(url)
}

fn client(host: &str, addresses: &[SocketAddr]) -> Result<reqwest::Client, String> {
    reqwest::Client::builder().no_proxy().https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(12)).resolve_to_addrs(host, addresses).build()
        .map_err(|_| "Não foi possível preparar o download público do ícone.".into())
}

pub async fn download(value: &str) -> Result<Vec<u8>, String> {
    let mut url = validated_url(value).ok_or("Endereço de ícone inválido.")?;
    for _ in 0..=3 {
        let host = url.host_str().ok_or("Servidor de ícone inválido.")?;
        let addresses: Vec<_> = tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, 443)))
            .await.map_err(|_| "O servidor do ícone demorou para responder.")?
            .map_err(|_| "Não foi possível localizar o servidor do ícone.")?.collect();
        if addresses.is_empty() || addresses.iter().any(|addr| !public_address(addr.ip())) {
            crate::live_trace::record("avatar_public_dns_rejected", 0, 0, 0);
            return Err("O servidor do ícone não é público.".into());
        }
        // A separate client has no cookie store or account headers. Pin the
        // validated DNS answers so a second DNS lookup cannot change the peer.
        let mut response = client(host, &addresses)?.get(url.clone()).send().await
            .map_err(|_| "Não foi possível baixar o ícone público.")?;
        let status = response.status().as_u16();
        crate::live_trace::record("avatar_image_http", 0, usize::from(status), 3);
        if response.status().is_redirection() {
            let next = response.headers().get("Location").and_then(|value| value.to_str().ok())
                .and_then(|value| url.join(value).ok()).ok_or("Redirecionamento do ícone inválido.")?;
            url = validated_url(next.as_str()).ok_or("Redirecionamento do ícone não permitido.")?;
            continue;
        }
        if !response.status().is_success() { return Err("O ícone público está indisponível.".into()); }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "Download do ícone interrompido.")? {
            if chunk.len() > MAX_BYTES.saturating_sub(bytes.len()) { return Err("Ícone acima do limite local.".into()); }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(bytes);
    }
    Err("O ícone excedeu o limite de redirecionamentos.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_cdn_is_not_limited_to_one_vendor_hostname() {
        assert!(validated_url("https://store-images.s-microsoft.com/image/apps/icon.png").is_some());
        assert!(validated_url("https://cdn.example.test/app/icon.png").is_some());
        for url in ["https://127.0.0.1/icon", "https://[::ffff:127.0.0.1]/icon", "https://localhost/a", "https://user:secret@cdn.example.test/a", "http://cdn.example.test/a"] {
            assert!(validated_url(url).is_none());
        }
    }
    #[test]
    fn dns_pinning_rejects_internal_and_reserved_addresses() {
        for ip in ["127.0.0.1", "10.0.0.1", "169.254.169.254", "100.64.0.1", "198.18.0.1", "192.0.0.1", "192.88.99.1", "2001:2::1", "3fff::1", "::1", "fc00::1", "fe80::1", "::ffff:10.0.0.1", "2001:db8::1"] {
            assert!(!public_address(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_address("8.8.8.8".parse().unwrap()));
        assert!(public_address("2606:4700:4700::1111".parse().unwrap()));
    }
    #[test]
    fn anonymous_request_contains_no_account_credentials() {
        let request = client("cdn.example.test", &["8.8.8.8:443".parse().unwrap()]).unwrap()
            .get("https://cdn.example.test/icon.png").build().unwrap();
        for header in ["Authorization", "X-Skypetoken", "Cookie"] { assert!(!request.headers().contains_key(header)); }
    }
}
