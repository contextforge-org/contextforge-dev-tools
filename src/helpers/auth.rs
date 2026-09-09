//! Ephemeral test signing keys and a loopback public JWKS endpoint.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::KeyPair as _;
use axum::{Json, Router, routing::get};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};

const KEY_ID: &str = "cf-integration-standalone";

pub(super) fn router(key_path: &Path) -> Result<Router> {
    if !key_path.exists() {
        let key = KeyPair::generate(KeySize::Rsa2048).context("failed to generate test RSA key")?;
        let der: Pkcs8V1Der<'_> = key.as_der().context("failed to encode test RSA key")?;
        let pem = pem::encode(&pem::Pem::new("PRIVATE KEY", der.as_ref()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(key_path)?.write_all(pem.as_bytes())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))?;
    }
    let key = pem::parse(fs::read(key_path)?).context("invalid test signing key PEM")?;
    let key = KeyPair::from_pkcs8(key.contents()).context("invalid test RSA signing key")?;
    let public = key.public_key();
    let jwks = json!({"keys": [{
        "kty": "RSA", "kid": KEY_ID, "alg": "RS256", "use": "sig",
        "n": URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),
        "e": URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero()),
    }]});
    Ok(Router::new().route(
        "/.well-known/jwks.json",
        get(move || {
            let jwks = jwks.clone();
            async move { Json(jwks) }
        }),
    ))
}

pub(super) fn issue_token(key_path: &Path, tenant_id: &str, user_id: &str) -> Result<String> {
    let key =
        EncodingKey::from_rsa_pem(&fs::read(key_path)?).context("invalid test signing key")?;
    let now = jsonwebtoken::get_current_timestamp();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KEY_ID.to_owned());
    jsonwebtoken::encode(
        &header,
        &json!({
            "sub": user_id, "tenant_id": tenant_id, "iat": now, "nbf": now, "exp": now + 86400,
        }),
        &key,
    )
    .context("failed to sign test JWT")
}

pub(super) fn token_subject(token: &str) -> Result<String> {
    // Only read the routing identity here. The dataplane verifies the signature.
    let claims = jsonwebtoken::dangerous::insecure_decode::<Value>(token)
        .context("MCP_CONFORMANCE_TOKEN has invalid JWT claims")?
        .claims;
    let subject = claims["sub"]
        .as_str()
        .context("MCP_CONFORMANCE_TOKEN has no string subject")?;
    ensure!(
        !subject.is_empty(),
        "MCP_CONFORMANCE_TOKEN has no string subject"
    );
    Ok(subject.to_owned())
}
