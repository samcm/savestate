//! The object stores of the installation config (`stores`), opened with the
//! credentials in their Secrets, which are read on every use so that rotating them
//! needs no restart. See "Object stores" in `docs/design.md`.

use anyhow::Context as _;
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use savestate_api::Config;
use savestate_objstore::{Credentials, Remote};

/// Opens store `name`, reading its Secret from this pod's namespace
/// (`POD_NAMESPACE`).
pub async fn open(client: &Client, config: &Config, name: &str) -> anyhow::Result<Remote> {
    let store = config
        .stores
        .get(name)
        .with_context(|| format!("store {name} is not in the installation config"))?;
    let namespace = std::env::var("POD_NAMESPACE")
        .context("POD_NAMESPACE: the namespace holding the stores' credentials")?;
    let secret = Api::<Secret>::namespaced(client.clone(), &namespace)
        .get(&store.credentials_secret)
        .await
        .with_context(|| {
            format!(
                "reading Secret {namespace}/{} for store {name}",
                store.credentials_secret
            )
        })?;
    let data = secret.data.unwrap_or_default();
    let field = |key: &str| -> anyhow::Result<Option<String>> {
        data.get(key)
            .map(|value| String::from_utf8(value.0.clone()))
            .transpose()
            .with_context(|| format!("{key} in Secret {} is not UTF-8", store.credentials_secret))
    };
    let required = |key: &str| -> anyhow::Result<String> {
        field(key)?.with_context(|| {
            format!(
                "Secret {} for store {name} has no {key}",
                store.credentials_secret
            )
        })
    };
    let credentials = Credentials {
        access_key_id: required("accessKeyId")?,
        secret_access_key: required("secretAccessKey")?,
        session_token: field("sessionToken")?,
    };
    Remote::s3(
        &store.endpoint,
        &store.bucket,
        &store.region,
        &store.prefix,
        &credentials,
    )
    .with_context(|| format!("store {name}"))
}
