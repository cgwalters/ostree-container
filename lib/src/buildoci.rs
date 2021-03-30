//! APIs for creating container images from OSTree commits

use super::Result;

use crate::ostree_ext::*;
use anyhow::{anyhow, Context};
use camino::Utf8Path;
use flate2::write::GzEncoder;
use fn_error_context::context;
use gio::prelude::*;
use gvariant::aligned_bytes::TryAsAligned;
use gvariant::{gv, Marker, Structure};
use openat_ext::*;
use openssl::hash::{Hasher, MessageDigest};
use phf::phf_map;
use std::io::prelude::*;
use std::{borrow::Cow, collections::HashSet, path::Path};

/// Map the value from `uname -m` to the Go architecture.
/// TODO find a more canonical home for this.
static MACHINE_TO_OCI: phf::Map<&str, &str> = phf_map! {
    "x86_64" => "amd64",
    "aarch64" => "arm64",
};

// OCI types, see https://github.com/opencontainers/image-spec/blob/master/media-types.md
const OCI_TYPE_CONFIG_JSON: &str = "application/vnd.oci.image.config.v1+json";
const OCI_TYPE_MANIFEST_JSON: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_TYPE_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
/// Path inside an OCI directory to the blobs
const BLOBDIR: &str = "blobs/sha256";
// FIXME get rid of this after updating to https://github.com/coreos/openat-ext/pull/27
const TMPBLOB: &str = ".tmpblob";

// This way the default ostree -> sysroot/ostree symlink works.
const OSTREEDIR: &str = "./sysroot/ostree";

/// The location to store the generated image
pub enum Target<'a> {
    /// Generate an Open Containers image directory layout
    OciDir(&'a Path),
}

/// Completed blob metadata
struct Blob {
    sha256: String,
    size: u64,
}

impl Blob {
    fn digest_id(&self) -> String {
        format!("sha256:{}", self.sha256)
    }
}

/// Completed layer metadata
struct Layer {
    blob: Blob,
    uncompressed_sha256: String,
}

/// Create an OCI blob.
struct BlobWriter<'a> {
    ocidir: &'a openat::Dir,
    hash: Hasher,
    target: Option<FileWriter<'a>>,
    size: u64,
}

/// Create an OCI layer (also a blob).
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

/// Convert /usr/etc back to /etc
fn map_path(p: &Utf8Path) -> std::borrow::Cow<Utf8Path> {
    match p.strip_prefix("./usr/etc") {
        Ok(r) => Cow::Owned(Utf8Path::new("./etc").join(r)),
        _ => Cow::Borrowed(p),
    }
}

struct OstreeMetadataWriter<'a, W: std::io::Write> {
    repo: &'a ostree::Repo,
    out: &'a mut tar::Builder<W>,
    wrote_dirtree: HashSet<String>,
    wrote_dirmeta: HashSet<String>,
    wrote_content: HashSet<String>,
}

fn object_path(objtype: ostree::ObjectType, checksum: &str) -> String {
    let suffix = match objtype {
        ostree::ObjectType::Commit => "commit",
        ostree::ObjectType::CommitMeta => "commitmeta",
        ostree::ObjectType::DirTree => "dirtree",
        ostree::ObjectType::DirMeta => "dirmeta",
        ostree::ObjectType::File => "file",
        o => panic!("Unexpected object type: {:?}", o),
    };
    let (first, rest) = checksum.split_at(2);
    format!("{}/repo/objects/{}/{}.{}", OSTREEDIR, first, rest, suffix)
}

impl<'a, W: std::io::Write> OstreeMetadataWriter<'a, W> {
    fn append(
        &mut self,
        objtype: ostree::ObjectType,
        checksum: &str,
        v: &glib::Variant,
    ) -> Result<()> {
        let set = match objtype {
            ostree::ObjectType::Commit => None,
            ostree::ObjectType::DirTree => Some(&mut self.wrote_dirtree),
            ostree::ObjectType::DirMeta => Some(&mut self.wrote_dirmeta),
            o => panic!("Unexpected object type: {:?}", o),
        };
        if let Some(set) = set {
            if set.contains(checksum) {
                return Ok(());
            }
            let inserted = set.insert(checksum.to_string());
            debug_assert!(inserted);
        }

        let mut h = tar::Header::new_gnu();
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(0o644);
        let data = v.get_data_as_bytes();
        let data = data.as_ref();
        h.set_size(data.len() as u64);
        self.out
            .append_data(&mut h, &object_path(objtype, checksum), data)?;
        Ok(())
    }

    fn append_content(&mut self, checksum: &str) -> Result<(String, tar::Header)> {
        let path = object_path(ostree::ObjectType::File, checksum);

        let (instream, meta, xattrs) = self.repo.load_file(checksum, gio::NONE_CANCELLABLE)?;
        let meta = meta.unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_uid(meta.get_attribute_uint32("unix::uid") as u64);
        h.set_gid(meta.get_attribute_uint32("unix::gid") as u64);
        h.set_mode(meta.get_attribute_uint32("unix::mode"));
        let target_header = h.clone();

        if !self.wrote_content.contains(checksum) {
            let inserted = self.wrote_content.insert(checksum.to_string());
            debug_assert!(inserted);

            if let Some(instream) = instream {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(meta.get_size() as u64);
                let mut instream = instream.into_read();
                self.out.append_data(&mut h, &path, &mut instream)?;
            } else {
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_link_name(meta.get_symlink_target().unwrap().as_str())?;
                self.out.append_data(&mut h, &path, &mut std::io::empty())?;
            }
        }

        Ok((path, target_header))
    }

    fn append_dirtree<C: IsA<gio::Cancellable>>(
        &mut self,
        dirpath: &Utf8Path,
        repo: &ostree::Repo,
        checksum: &str,
        cancellable: Option<&C>,
    ) -> Result<()> {
        let v = &repo.load_variant(ostree::ObjectType::DirTree, checksum)?;
        self.append(ostree::ObjectType::DirMeta, checksum, v)?;
        let v = v.get_data_as_bytes();
        let v = v.try_as_aligned()?;
        let v = gv!("(a(say)a(sayay))").cast(v);
        let (files, dirs) = v.to_tuple();

        if let Some(c) = cancellable {
            c.set_error_if_cancelled()?;
        }

        // A reusable buffer to avoid heap allocating these
        let mut hexbuf = [0u8; 64];

        for file in files {
            let (name, csum) = file.to_tuple();
            let name = name.to_str();
            hex::encode_to_slice(csum, &mut hexbuf)?;
            let checksum = std::str::from_utf8(&hexbuf)?;
            let (objpath, mut h) = self.append_content(checksum)?;
            h.set_entry_type(tar::EntryType::Link);
            h.set_link_name(&objpath)?;
            let subpath = &dirpath.join(name);
            let subpath = map_path(subpath);
            self.out
                .append_data(&mut h, &*subpath, &mut std::io::empty())?;
        }

        for item in dirs {
            let (name, contents_csum, meta_csum) = item.to_tuple();
            let name = name.to_str();
            {
                hex::encode_to_slice(meta_csum, &mut hexbuf)?;
                let meta_csum = std::str::from_utf8(&hexbuf)?;
                let meta_v = &repo.load_variant(ostree::ObjectType::DirMeta, meta_csum)?;
                self.append(ostree::ObjectType::DirMeta, meta_csum, meta_v)?;
            }
            hex::encode_to_slice(contents_csum, &mut hexbuf)?;
            let dirtree_csum = std::str::from_utf8(&hexbuf)?;
            let subpath = &dirpath.join(name);
            let subpath = map_path(subpath);
            self.append_dirtree(&*subpath, repo, dirtree_csum, cancellable)?;
        }

        Ok(())
    }
}

/// Recursively walk an OSTree commit, injecting all of its metadata
/// into the
fn ostree_metadata_to_tar<W: std::io::Write, C: IsA<gio::Cancellable>>(
    repo: &ostree::Repo,
    commit_checksum: &str,
    out: &mut tar::Builder<W>,
    cancellable: Option<&C>,
) -> Result<()> {
    // Pre create the object directories
    for d in 0..0xFF {
        let mut h = tar::Header::new_gnu();
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(0o755);
        let path = format!("{}/repo/objects/{:#04x}", OSTREEDIR, d);
        out.append_data(&mut h, &path, &mut std::io::empty())?;
    }

    let writer = &mut OstreeMetadataWriter {
        repo,
        out,
        wrote_dirmeta: HashSet::new(),
        wrote_dirtree: HashSet::new(),
        wrote_content: HashSet::new(),
    };
    let (commit_v, _) = repo.load_commit(commit_checksum)?;
    let commit_v = &commit_v;
    writer.append(ostree::ObjectType::Commit, commit_checksum, commit_v)?;

    if let Some(commitmeta) =
        repo.x_load_variant_if_exists(ostree::ObjectType::CommitMeta, commit_checksum)?
    {
        writer.append(ostree::ObjectType::CommitMeta, commit_checksum, &commitmeta)?;
    }

    let commit_v = commit_v.get_data_as_bytes();
    let commit_v = commit_v.try_as_aligned()?;
    let commit = gv!("(a{sv}aya(say)sstayay)").cast(commit_v);
    let commit = commit.to_tuple();
    let contents = &hex::encode(commit.6);
    let metadata_checksum = &hex::encode(commit.7);
    let metadata_v = &repo.load_variant(ostree::ObjectType::DirMeta, metadata_checksum)?;
    writer.append(ostree::ObjectType::DirMeta, metadata_checksum, metadata_v)?;

    writer.append_dirtree(Utf8Path::new("./"), repo, contents, cancellable)?;
    Ok(())
}

/// Write an ostree directory as an OCI blob
#[context("Writing ostree root to blob")]
fn export_ostree_ref_to_blobdir(
    repo: &ostree::Repo,
    ostree_commit: &str,
    ocidir: &openat::Dir,
) -> Result<Layer> {
    let cancellable = gio::NONE_CANCELLABLE;
    let mut w = LayerWriter::new(ocidir)?;
    {
        let mut tar = tar::Builder::new(&mut w);
        ostree_metadata_to_tar(repo, ostree_commit, &mut tar, cancellable)?;
        tar.finish()?;
    }
    w.complete()
}

/// Write a serializable data (JSON) as an OCI blob
#[context("Writing json blob")]
fn write_json_blob<S: serde::Serialize>(ocidir: &openat::Dir, v: &S) -> Result<Blob> {
    let mut w = BlobWriter::new(ocidir)?;
    {
        cjson::to_writer(&mut w, v).map_err(|e| anyhow!("{:?}", e))?;
    }

    w.complete()
}

/// Generate an OCI image from a given ostree root
#[context("Building oci")]
fn build_oci(repo: &ostree::Repo, commit: &str, ocidir: &Path) -> Result<()> {
    let utsname = nix::sys::utsname::uname();
    let arch = MACHINE_TO_OCI[utsname.machine()];
    // Explicitly error if the target exists
    std::fs::create_dir(ocidir).context("Creating OCI dir")?;
    let ocidir = &openat::Dir::open(ocidir)?;
    ocidir.ensure_dir_all(BLOBDIR, 0o755)?;
    ocidir.write_file_contents("oci-layout", 0o644, r#"{"imageLayoutVersion":"1.0.0"}"#)?;

    let rootfs_blob = export_ostree_ref_to_blobdir(repo, commit, ocidir)?;
    let root_layer_id = format!("sha256:{}", rootfs_blob.uncompressed_sha256);

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

/// Helper for `build()` that avoids generics
fn build_impl(repo: &ostree::Repo, ostree_ref: &str, target: Target) -> Result<()> {
    match target {
        Target::OciDir(d) => return build_oci(repo, ostree_ref, d),
    }
}

/// Given an OSTree repository and ref, generate a container image
pub fn build<R: AsRef<Path>, S: AsRef<str>>(repo: R, ostree_ref: S, target: Target) -> Result<()> {
    let cancellable = gio::NONE_CANCELLABLE;
    let repo_path = repo.as_ref();
    let repo = &ostree::Repo::new_for_path(repo_path);
    repo.open(cancellable)?;
    build_impl(repo, ostree_ref.as_ref(), target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_path() {
        assert_eq!(map_path("/".into()), Utf8Path::new("/"));
        assert_eq!(
            map_path("./usr/etc/blah".into()),
            Utf8Path::new("./etc/blah")
        );
    }
}
