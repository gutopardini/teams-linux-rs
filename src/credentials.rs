//! Secret Service storage. Secret values travel over pipes, never arguments.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn profile_key(client_id: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(client_id.as_bytes()))
}

fn execute(action: &str, profile: &str, secret: Option<&str>) -> Result<(bool, String), String> {
    let mut command = Command::new("secret-tool");
    command.arg(action);
    if action == "store" {
        command.args(["--label", "Teams Linux — sessão de trabalho"]);
    }
    let mut child = command
        .args(["application", crate::APP_ID, "profile", profile])
        .stdin(if secret.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "O chaveiro do sistema não está disponível.".to_owned())?;
    if let Some(secret) = secret {
        let written = child
            .stdin
            .take()
            .ok_or_else(|| "Não foi possível acessar o chaveiro.".to_owned())?
            .write_all(secret.as_bytes());
        if written.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Não foi possível enviar a sessão ao chaveiro.".into());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("O chaveiro do sistema não respondeu.".into());
            }
        }
    };
    let mut output = String::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(65536)
            .read_to_string(&mut output)
            .map_err(|_| "Não foi possível ler a sessão do chaveiro.".to_owned())?;
    }
    Ok((
        status.success(),
        output.trim_end_matches(['\n', '\r']).to_owned(),
    ))
}

pub fn load(profile: &str) -> Result<Option<String>, String> {
    let (success, secret) = execute("lookup", profile, None)?;
    Ok((success && !secret.is_empty()).then_some(secret))
}

pub fn save(profile: &str, secret: &str) -> Result<(), String> {
    if execute("store", profile, Some(secret))?.0 {
        Ok(())
    } else {
        Err("Não foi possível salvar a sessão no chaveiro do sistema.".into())
    }
}

pub fn clear(profile: &str) -> Result<(), String> {
    let (removed, _) = execute("clear", profile, None)?;
    if removed {
        Ok(())
    } else {
        Err("Não foi possível remover a sessão salva no chaveiro.".into())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn profile_key_does_not_expose_the_configured_id() {
        let key = super::profile_key("test-owned-client-id");
        assert_eq!(key, super::profile_key("test-owned-client-id"));
        assert_ne!(key, super::profile_key("another-client"));
        assert!(!key.contains("test-owned-client-id"));
    }
}
