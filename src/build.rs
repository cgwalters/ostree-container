//! APIs for creating container images from OSTree commits

use super::Result;

use openat_ext::*;
use flate2::write::{GzEncoder, ZlibEncoder};
use gio::prelude::*;
use std::{fs::File, io::Write};
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
    compressed_hash: Hasher,
    uncompressed_hash: Hasher,
    compressor: GzEncoder<Vec<u8>>,
    target: FileWriter<'a>,
    size: u64,
}

impl<'a> std::io::Write for BlobWriter<'a> {
    fn write(&mut self, srcbuf: &[u8]) -> std::io::Result<usize> {
        self.uncompressed_hash.update(srcbuf)?;
        self.compressor.get_mut().clear();
        let n = self.compressor.write(srcbuf)?;
        let compressed_buf = self.compressor.get_mut().as_slice();
        assert_eq!(n, compressed_buf.len());
        self.compressed_hash.update(compressed_buf)?;
        self.target.writer.write_all(compressed_buf)?;
        self.size += n as u64;
        Ok(compressed_buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        todo!()
    }
}

fn write_ostree<W: std::io::Write>(root: &ostree::RepoFile, out: &mut tar::Builder<W>) -> Result<()> {
    todo!()
}

fn export_ostree_ref_to_blobdir(repo: &OstreeRepoSource, root: &ostree::RepoFile, ocidir: &openat::Dir) -> Result<Blob> {
    let tmpblob = ".tmpblob";
    let mut w = BlobWriter {
        compressed_hash: Hasher::new(MessageDigest::sha256())?,
        uncompressed_hash: Hasher::new(MessageDigest::sha256())?,
        compressor: GzEncoder::new(Vec::with_capacity(8192), flate2::Compression::default()),
        // FIXME add ability to choose filename after completion
        target: ocidir.new_file_writer(tmpblob, 0o644)?,
        size: 0,
    };
    {
        let mut tar = tar::Builder::new(&mut w);
        write_ostree(root, &mut tar)?;
    }
    w.target.complete()?;
    let compressed_sha256 = hex::encode(w.compressed_hash.finish()?);
    let uncompressed_sha256 = hex::encode(w.uncompressed_hash.finish()?);
    ocidir.local_rename(tmpblob, &uncompressed_sha256)?;
    Ok(Blob{
        compressed_sha256,
        uncompressed_sha256,
        size: w.size,
    })
}

fn build_oci(repo: &OstreeRepoSource, root: &ostree::RepoFile, ocidir: &Path) -> Result<()> {
    std::fs::create_dir_all(ocidir.join("blobs/sha256"))?;
    let ocidir = &openat::Dir::open(ocidir)?;
    ocidir.write_file_contents("oci-layout", 0o644, OCI_LAYOUT_JSON)?;

    let blob = export_ostree_ref_to_blobdir(repo, root, ocidir)?;

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
