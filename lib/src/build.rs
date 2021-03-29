//! APIs for creating container images from OSTree commits

use super::Result;

use anyhow::{anyhow, Context};
use flate2::write::GzEncoder;
use fn_error_context::context;
use gio::prelude::*;
use openat_ext::*;
use openssl::hash::{Hasher, MessageDigest};
use phf::phf_map;
use std::io::prelude::*;
use std::path::Path;

static OSTREE_ARCH_TO_OCI: phf::Map<&str, &str> = phf_map! {
    "x86_64" => "amd64",
    "aarch64" => "arm64",
};

const OCI_TYPE_CONFIG_JSON: &str = "application/vnd.oci.image.config.v1+json";
const OCI_TYPE_MANIFEST_JSON: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_TYPE_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const BLOBDIR: &str = "blobs/sha256";
const TMPBLOB: &str = ".tmpblob";

// We only need name+type
const QUERYINFO_ATTRS: &str = "standard::name,standard::type";
// Don't follow symlinks
const QUERYINFO_FLAGS: gio::FileQueryInfoFlags = gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS;

/// The location to store the generated image
pub enum Target<'a> {
    /// Generate an Open Containers image directory layout
    OciDir(&'a Path),
}

struct Blob {
    sha256: String,
    size: u64,
}

impl Blob {
    fn digest_id(&self) -> String {
        format!("sha256:{}", self.sha256)
    }
}

struct Layer {
    blob: Blob,
    uncompressed_sha256: String,
}

struct OstreeRepoSource<'a, 'b> {
    path: &'a Path,
    repo: &'b ostree::Repo,
}


struct BlobWriter<'a> {
    ocidir: &'a openat::Dir,
    hash: Hasher,
    target: Option<FileWriter<'a>>,
    size: u64,
}

struct LayerWriter<'a> {
    bw: BlobWriter<'a>,
    uncompressed_hash: Hasher,
    compressor: GzEncoder<Vec<u8>>,
}

impl<'a> Drop for BlobWriter<'a> {
    fn drop(&mut self) {
        if let Some(t) = self.target.take() {
            // Defuse
            let _ = t.abandon();
        }
    }
}

impl<'a> BlobWriter<'a> {
    #[context("Creating blob writer")]
    fn new(ocidir: &'a openat::Dir) -> Result<Self> {
        Ok(Self {
            ocidir,
            hash: Hasher::new(MessageDigest::sha256())?,
            // FIXME add ability to choose filename after completion
            target: Some(ocidir.new_file_writer(TMPBLOB, 0o644)?),
            size: 0,
        })
    }

    #[context("Completing blob")]
    fn complete(mut self) -> Result<Blob> {
        self.target.take().unwrap().complete()?;
        let sha256 = hex::encode(self.hash.finish()?);
        self.ocidir
            .local_rename(TMPBLOB, &format!("{}/{}", BLOBDIR, sha256))?;
        Ok(Blob {
            sha256,
            size: self.size,
        })
    }
}

impl<'a> std::io::Write for BlobWriter<'a> {
    fn write(&mut self, srcbuf: &[u8]) -> std::io::Result<usize> {
        self.hash.update(srcbuf)?;
        self.target.as_mut().unwrap().writer.write_all(srcbuf)?;
        self.size += srcbuf.len() as u64;
        Ok(srcbuf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> LayerWriter<'a> {
    fn new(ocidir: &'a openat::Dir) -> Result<Self> {
        let bw = BlobWriter::new(ocidir)?;
        Ok(Self {
            bw,
            uncompressed_hash: Hasher::new(MessageDigest::sha256())?,
            compressor: GzEncoder::new(Vec::with_capacity(8192), flate2::Compression::default()),
        })
    }

    #[context("Completing layer")]
    fn complete(mut self) -> Result<Layer> {
        self.compressor.get_mut().clear();
        let buf = self.compressor.finish()?;
        self.bw.write_all(&buf)?;
        let blob = self.bw.complete()?;
        let uncompressed_sha256 = hex::encode(self.uncompressed_hash.finish()?);
        Ok(Layer {
            blob,
            uncompressed_sha256,
        })
    }
}

impl<'a> std::io::Write for LayerWriter<'a> {
    fn write(&mut self, srcbuf: &[u8]) -> std::io::Result<usize> {
        self.compressor.get_mut().clear();
        self.compressor.write_all(srcbuf).unwrap();
        let compressed_buf = self.compressor.get_mut().as_slice();
        self.bw.write_all(&compressed_buf)?;
        Ok(srcbuf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[context("write_ostree")]
fn write_ostree<W: std::io::Write>(
    root: &ostree::RepoFile,
    out: &mut tar::Builder<W>,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_path("foo")?;
    header.set_size(4);
    header.set_cksum();

    let data: &[u8] = &[1, 2, 3, 4];
    out.append(&header, data).context("appending tar")?;
    out.finish()?;
    Ok(())
}

#[context("Writing ostree root to blob")]
fn export_ostree_ref_to_blobdir(
    repo: &OstreeRepoSource,
    root: &ostree::RepoFile,
    ocidir: &openat::Dir,
) -> Result<Layer> {
    let mut w = LayerWriter::new(ocidir)?;
    {
        let mut tar = tar::Builder::new(&mut w);
        write_ostree(root, &mut tar)?;
    }
    w.complete()
}

#[context("Writing json blob")]
fn write_json_blob<S: serde::Serialize>(ocidir: &openat::Dir, v: &S) -> Result<Blob> {
    let mut w = BlobWriter::new(ocidir)?;
    {
        cjson::to_writer(&mut w, v).map_err(|e| anyhow!("{:?}", e))?;
    }
    w.complete()
}

#[context("Building oci")]
fn build_oci(repo: &OstreeRepoSource, root: &ostree::RepoFile, ocidir: &Path) -> Result<()> {
    let arch = OSTREE_ARCH_TO_OCI["x86_64"];
    // Explicitly error if the target exists
    std::fs::create_dir(ocidir).context("Creating OCI dir")?;
    let ocidir = &openat::Dir::open(ocidir)?;
    ocidir.ensure_dir_all(BLOBDIR, 0o755)?;
    ocidir.write_file_contents("oci-layout", 0o644, r#"{"imageLayoutVersion":"1.0.0"}"#)?;

    let rootfs_blob = export_ostree_ref_to_blobdir(repo, root, ocidir)?;
    let root_layer_id = format!(
        "sha256:{}",
        rootfs_blob.uncompressed_sha256
    );

    let config = serde_json::json!({
        "architecture": arch,
        "os": "linux",
        "rootfs": {
            "type": "layers",
            "diff_ids": [ root_layer_id ],
        },
        "history": [
            {
                "commit": "created by ostree-container",
            }
        ]
    });
    let config_blob = write_json_blob(ocidir, &config)?;

    let manifest_data = serde_json::json!({
        "schemaVersion": 2,
        "config": {
            "mediaType": OCI_TYPE_CONFIG_JSON,
            "size": config_blob.size,
            "digest": config_blob.digest_id(),
        },
        "layers": [
            { "mediaType": OCI_TYPE_LAYER,
              "size": rootfs_blob.blob.size,
              "digest":  rootfs_blob.blob.digest_id(),
            }
        ],
    });
    let manifest_blob = write_json_blob(ocidir, &manifest_data)?;

    let index_data = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [
            {
                "mediaType": OCI_TYPE_MANIFEST_JSON,
                "digest": manifest_blob.digest_id(),
                "size": manifest_blob.size,
                "platform": {
                    "architecture": arch,
                    "os": "linux"
                }
            }
        ]
    });
    ocidir.write_file_with("index.json", 0o644, |w| -> Result<()> {
        cjson::to_writer(w, &index_data).map_err(|e| anyhow::anyhow!("{:?}", e))?;
        Ok(())
    })?;

    Ok(())
}

fn build_impl(reposrc: &OstreeRepoSource, ostree_ref: &str, target: Target) -> Result<()> {
    let cancellable = gio::NONE_CANCELLABLE;
    reposrc.repo.open(cancellable)?;
    let (root, _) = reposrc.repo.read_commit(ostree_ref, cancellable)?;
    let root = root.downcast::<ostree::RepoFile>().expect("downcast");
    match target {
        Target::OciDir(d) => return build_oci(reposrc, &root, d),
    }
}

/// Given an OSTree repository and ref, generate a container image
pub fn build<R: AsRef<Path>, S: AsRef<str>>(repo: R, ostree_ref: S, target: Target) -> Result<()> {
    let repo_path = repo.as_ref();
    let repo_obj = &ostree::Repo::new_for_path(repo_path);
    let reposrc = &OstreeRepoSource {
        path: repo_path,
        repo: repo_obj,
    };
    build_impl(reposrc, ostree_ref.as_ref(), target)
}
