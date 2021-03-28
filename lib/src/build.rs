//! APIs for creating container images from OSTree commits

use super::Result;

use openat_ext::*;
use flate2::write::GzEncoder;
use gio::prelude::*;
use phf::phf_map;
use openssl::hash::{Hasher, MessageDigest};
use std::path::Path;

static OSTREE_ARCH_TO_OCI: phf::Map<&str, &str> = phf_map! {
    "x86_64" => "amd64",
    "aarch64" => "arm64",
};

const OCI_CONFIG_JSON: &str = "application/vnd.oci.image.config.v1+json";
const OCI_MANIFEST_JSON: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const OCI_LAYOUT_JSON: &str = r#"{"imageLayoutVersion":"1.0.0"}"#;
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
    compressed_sha256: String,
    uncompressed_sha256: String,
    size: u64,
}

struct OstreeRepoSource<'a, 'b> {
    path: &'a Path,
    repo: &'b ostree::Repo,
}

struct BlobWriter<'a> {
    ocidir: &'a openat::Dir,
    compressed_hash: Hasher,
    uncompressed_hash: Hasher,
    compressor: GzEncoder<Vec<u8>>,
    target: FileWriter<'a>,
    size: u64,
}

impl<'a> BlobWriter<'a> {
    fn new(ocidir: &'a openat::Dir) -> Result<Self> {
        Ok(Self {
            ocidir,
            compressed_hash: Hasher::new(MessageDigest::sha256())?,
            uncompressed_hash: Hasher::new(MessageDigest::sha256())?,
            compressor: GzEncoder::new(Vec::with_capacity(8192), flate2::Compression::default()),
            // FIXME add ability to choose filename after completion
            target: ocidir.new_file_writer(TMPBLOB, 0o644)?,
            size: 0,
        })
    }

    fn complete(mut self) -> Result<Blob> {
        self.target.complete()?;
        let compressed_sha256 = hex::encode(self.compressed_hash.finish()?);
        let uncompressed_sha256 = hex::encode(self.uncompressed_hash.finish()?);
        self.ocidir.local_rename(TMPBLOB, &format!("{}/{}", BLOBDIR, uncompressed_sha256))?;
        Ok(Blob{
            compressed_sha256,
            uncompressed_sha256,
            size: self.size,
        })
    }
}

impl<'a> std::io::Write for BlobWriter<'a> {
    fn write(&mut self, srcbuf: &[u8]) -> std::io::Result<usize> {
        self.uncompressed_hash.update(srcbuf)?;
        self.compressor.get_mut().clear();
        self.compressor.write_all(srcbuf)?;
        let compressed_buf = self.compressor.get_mut().as_slice();
        self.compressed_hash.update(compressed_buf)?;
        self.target.writer.write_all(compressed_buf)?;
        self.size += compressed_buf.len() as u64;
        Ok(compressed_buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_ostree<W: std::io::Write>(root: &ostree::RepoFile, out: &mut tar::Builder<W>) -> Result<()> {
    Ok(())
}

fn export_ostree_ref_to_blobdir(repo: &OstreeRepoSource, root: &ostree::RepoFile, ocidir: &openat::Dir) -> Result<Blob> {
    let mut w = BlobWriter::new(ocidir)?;
    {
        let mut tar = tar::Builder::new(&mut w);
        write_ostree(root, &mut tar)?;
    }
    w.complete()
}

fn write_json_blob<S: serde::Serialize>(ocidir: &openat::Dir, v: &S) -> Result<Blob> {
    let mut w = BlobWriter::new(ocidir)?;
    {
        cjson::to_writer(&mut w, v)
            .map_err(|e| format!("{:?}", e))?;
    }
    w.complete()
}

fn build_oci(repo: &OstreeRepoSource, root: &ostree::RepoFile, ocidir: &Path) -> Result<()> {
    // Explicitly error if the target exists
    std::fs::create_dir(ocidir)?;
    let ocidir = &openat::Dir::open(ocidir)?;
    ocidir.ensure_dir_all(BLOBDIR, 0o755)?;
    ocidir.write_file_contents("oci-layout", 0o644, OCI_LAYOUT_JSON)?;

    let rootfs_blob = export_ostree_ref_to_blobdir(repo, root, ocidir)?;
    let root_id = format!("sha256:{}", rootfs_blob.uncompressed_sha256);

    let config = serde_json::json!({
        "created": "today", 
        "architecture": "x86_64", 
        "os": "linux",
        "rootfs": {
            "type": "layers",
            "diff_ids": [root_id],
        },
        "history": [
            {
                "created": "today",
                "commit": "created by ostree-container",
            }
        ]
    });
    let config_blob = write_json_blob(ocidir, &config)?;

    // let manifest_data = serde_json::json!({
    //     "schemaVersion": 2,
    //     "config": {
    //         "mediaType": "application/vnd.oci.image.config.v1+json",
    //         "size": config_blob.size,
    //         "digest': 'sha256:' + config_blob.sha256,
    //     },
    //     "layers": [
    //         { 'mediaType': 'application/vnd.oci.image.layer.v1.tar+gzip',
    //           'size': baselayer_blob.size,
    //           'digest': 'sha256:' + baselayer_blob.sha256,
    //         }
    //     ],
    // });

    todo!()
}

fn build_impl(reposrc: &OstreeRepoSource, ostree_ref: &str, target: Target) -> Result<()> {
    let cancellable = gio::NONE_CANCELLABLE;
    reposrc.repo.open(cancellable)?;
    let (root, _) = reposrc.repo.read_commit(ostree_ref, cancellable)?;
    let root = root.downcast::<ostree::RepoFile>().expect("downcast");
    match target {
        Target::OciDir(d) => {
            return build_oci(reposrc, &root, d)
        }
    }
}

/// Given an OSTree repository and ref, generate a container image
pub fn build<R: AsRef<Path>, S: AsRef<str>>(repo: R, ostree_ref: S, target: Target) -> Result<()> {
    let repo_path = repo.as_ref();
    let repo_obj = &ostree::Repo::new_for_path(repo_path);
    let reposrc = &OstreeRepoSource {
        path: repo_path,
        repo: repo_obj
    };
    build_impl(reposrc, ostree_ref.as_ref(), target)
}
