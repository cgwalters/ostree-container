//! APIs for extracting OSTree commits from container images

use super::Result;
use anyhow::anyhow;
use fn_error_context::context;
use futures_util::stream::StreamExt;
use oci_distribution::manifest::OciDescriptor;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use tokio::io::AsyncWriteExt;

#[derive(Debug)]
pub struct Import {
    /// The ostree commit that was imported
    pub ostree_commit: String,
    /// The image digest retrieved
    pub image_digest: String,
}

#[context("Fetching layer descriptor")]
async fn fetch_layer_descriptor(
    client: &mut oci_distribution::Client,
    image_ref: &oci_distribution::Reference,
) -> Result<(String, OciDescriptor)> {
    let (manifest, digest) = client.pull_manifest(image_ref).await?;
    let mut layers = manifest.layers;
    layers.retain(|layer| {
        matches!(
            layer.media_type.as_str(),
            super::oci::DOCKER_TYPE_LAYER | oci_distribution::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE
        )
    });
    let n = layers.len();

    if let Some(layer) = layers.into_iter().nth(1) {
        if n > 1 {
            Err(anyhow!("Expected 1 layer, found {}", n))
        } else {
            Ok((digest, layer))
        }
    } else {
        Err(anyhow!("No layers found"))
    }
}

#[allow(unsafe_code)]
#[context("Importing {}", image_ref)]
async fn import_impl(repo: &ostree::Repo, image_ref: &str) -> Result<Import> {
    let image_ref: oci_distribution::Reference = image_ref.parse()?;
    let client = &mut oci_distribution::Client::default();
    let auth = &oci_distribution::secrets::RegistryAuth::Anonymous;
    client
        .auth(
            &image_ref,
            auth,
            &oci_distribution::secrets::RegistryOperation::Pull,
        )
        .await?;
    let (image_digest, layer) = fetch_layer_descriptor(client, &image_ref).await?;

    let mut req = client
        .request_layer(&image_ref, &layer.digest)
        .await?
        .bytes_stream();
    // FIXME better way to bridge async -> sync?
    let (piperead, mut pipewrite) = tokio_pipe::pipe()?;
    let piperead = unsafe { std::fs::File::from_raw_fd(piperead.into_raw_fd()) };
    let copyin_task = tokio::spawn(async move {
        while let Some(buf) = req.next().await {
            let buf = buf?;
            pipewrite.write_all(&buf).await?;
        }
        Ok::<(), anyhow::Error>(())
    });
    let repo = repo.clone();
    let res = tokio::task::spawn_blocking(move || ostree_ext::tar::import_tar(&repo, piperead));
    copyin_task.await??;
    let ostree_commit = res.await??;

    Ok(Import {
        ostree_commit,
        image_digest,
    })
}

/// Download and import the referenced container
pub async fn import<I: AsRef<str>>(repo: &ostree::Repo, image_ref: I) -> Result<Import> {
    Ok(import_impl(repo, image_ref.as_ref()).await?)
}
