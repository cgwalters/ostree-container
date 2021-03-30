//! APIs for creating container images from OSTree commits

use super::Result;

use anyhow::{anyhow, Context};
use camino::Utf8Path;
use flate2::write::GzEncoder;
use fn_error_context::context;
use gio::prelude::*;
use gvariant::aligned_bytes::TryAsAligned;
use gvariant::{gv, Marker, Structure};
use openat_ext::*;
use openssl::hash::{Hasher, MessageDigest};
use ostree::prelude::*;
use phf::phf_map;
use std::{borrow::Cow, collections::HashSet, path::Path};
use std::{convert::TryFrom, io::prelude::*};

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

// We only need name+type when iterating
const BASIC_QUERYINFO_ATTRS: &str = "standard::name,standard::type";
// Full metadata
const DEFAULT_QUERYINFO_ATTRS: &str = "standard::name,standard::type,standard::size,standard::is-symlink,standard::symlink-target,unix::device,unix::inode,unix::mode,unix::uid,unix::gid,unix::rdev";
// Don't follow symlinks
const QUERYINFO_FLAGS: gio::FileQueryInfoFlags = gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS;

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

/// Map GLib file types to Rust `tar` types
fn gio_filetype_to_tar(t: gio::FileType) -> tar::EntryType {
    match t {
        gio::FileType::Regular => tar::EntryType::Regular,
        gio::FileType::SymbolicLink => tar::EntryType::Symlink,
        gio::FileType::Directory => tar::EntryType::Directory,
        o => panic!("Unexpected FileType: {:?}", o),
    }
}

/// Convert /usr/etc back to /etc
fn map_path(p: &Utf8Path) -> std::borrow::Cow<Utf8Path> {
    match p.strip_prefix("./usr/etc") {
        Ok(r) => Cow::Owned(Utf8Path::new("./etc").join(r)),
        _ => Cow::Borrowed(p),
    }
}

/// Recursively walk an OSTree directory, generating a tarball
fn ostree_content_to_tar<W: std::io::Write, C: IsA<gio::Cancellable>>(
    repo: &ostree::Repo,
    dirpath: &Utf8Path,
    f: &gio::File,
    out: &mut tar::Builder<W>,
    cancellable: Option<&C>,
) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    let i = f.query_info(DEFAULT_QUERYINFO_ATTRS, QUERYINFO_FLAGS, cancellable)?;
    let name = camino::Utf8PathBuf::try_from(i.get_name().unwrap_or_else(|| ".".into()))?;
    let path = &dirpath.join(name);
    let path = map_path(&path);
    let path = &*path;
    let t = i.get_file_type();
    h.set_entry_type(gio_filetype_to_tar(t));
    h.set_uid(i.get_attribute_uint32("unix::uid") as u64);
    h.set_gid(i.get_attribute_uint32("unix::gid") as u64);
    h.set_mode(i.get_attribute_uint32("unix::mode"));
    match t {
        gio::FileType::Directory => {
            h.set_path(path)?;
            out.append_data(&mut h, path, &mut std::io::empty())?;
            let it = f.enumerate_children(BASIC_QUERYINFO_ATTRS, QUERYINFO_FLAGS, cancellable)?;
            while let Some(child_info) = it.next_file(cancellable)? {
                let child = &it.get_child(&child_info).expect("file");
                ostree_content_to_tar(repo, path, child, out, cancellable)?;
            }
        }
        gio::FileType::SymbolicLink => {
            h.set_link_name(i.get_symlink_target().unwrap().as_str())?;
            out.append_data(&mut h, path, &mut std::io::empty())?;
        }
        gio::FileType::Regular => {
            h.set_size(i.get_size() as u64);
            let f = f.downcast_ref::<ostree::RepoFile>().expect("downcast");
            let (r, _, _) = repo.load_file(f.get_checksum().unwrap().as_str(), cancellable)?;
            let r = r.unwrap();
            let mut r = r.into_read();
            out.append_data(&mut h, path, &mut r)?;
        }
        o => panic!("Unexpected FileType: {:?}", o),
    };

    Ok(())
}

struct OstreeMetadataWriter<'a, W: std::io::Write> {
    out: &'a mut tar::Builder<W>,
    wrote_dirtree: HashSet<String>,
    wrote_dirmeta: HashSet<String>,
}

impl<'a, W: std::io::Write> OstreeMetadataWriter<'a, W> {
    fn append(
        &mut self,
        objtype: ostree::ObjectType,
        checksum: &str,
        v: &glib::Variant,
    ) -> Result<()> {
        match objtype {
            ostree::ObjectType::Commit => {}
            ostree::ObjectType::DirTree => {
                if self.wrote_dirtree.contains(checksum) {
                    return Ok(());
                }
                let was_present = self.wrote_dirtree.insert(checksum.to_string());
                debug_assert!(!was_present);
            }
            ostree::ObjectType::DirMeta => {
                if self.wrote_dirmeta.contains(checksum) {
                    return Ok(());
                }
                let was_present = self.wrote_dirmeta.insert(checksum.to_string());
                debug_assert!(!was_present);
            }
            o => panic!("Unexpected object type: {:?}", o),
        }
        let mut h = tar::Header::new_gnu();
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(0o644);
        let (first, rest) = checksum.split_at(2);
        let path = format!("./ostree/repo/objects/{}/{}", first, rest);
        let data = v.get_data_as_bytes();
        let data = data.as_ref();
        h.set_size(data.len() as u64);
        self.out.append_data(&mut h, &path, data)?;
        Ok(())
    }

    fn append_dirtree<C: IsA<gio::Cancellable>>(
        &mut self,
        repo: &ostree::Repo,
        checksum: &str,
        cancellable: Option<&C>,
    ) -> Result<()> {
        let v = &repo.load_variant(ostree::ObjectType::DirTree, checksum)?;
        self.append(ostree::ObjectType::DirMeta, checksum, v)?;
        let v = v.get_data_as_bytes();
        let v = v.try_as_aligned()?;
        let v = gv!("(a(say)a(sayay))").cast(v);
        let (_, dirs) = v.to_tuple();

        if let Some(c) = cancellable {
            c.set_error_if_cancelled()?;
        }

        let mut hexbuf = [0u8; 64];
        for item in dirs {
            let (_, contents_csum, meta_csum) = item.to_tuple();
            {
                hex::encode_to_slice(meta_csum, &mut hexbuf)?;
                let meta_csum = std::str::from_utf8(&hexbuf)?;
                let meta_v = &repo.load_variant(ostree::ObjectType::DirMeta, meta_csum)?;
                self.append(ostree::ObjectType::DirMeta, meta_csum, meta_v)?;
            }
            hex::encode_to_slice(contents_csum, &mut hexbuf)?;
            let dirtree_csum = std::str::from_utf8(&hexbuf)?;
            self.append_dirtree(repo, dirtree_csum, cancellable)?;
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
        let path = format!("./ostree/repo/objects/{:#04x}", d);
        out.append_data(&mut h, &path, &mut std::io::empty())?;
    }

    let writer = &mut OstreeMetadataWriter {
        out,
        wrote_dirmeta: HashSet::new(),
        wrote_dirtree: HashSet::new(),
    };
    let (commit_v, _) = repo.load_commit(commit_checksum)?;
    let commit_v = &commit_v;
    writer.append(ostree::ObjectType::Commit, commit_checksum, commit_v)?;
    let commit_v = commit_v.get_data_as_bytes();
    let commit_v = commit_v.try_as_aligned()?;
    let commit = gv!("(a{sv}aya(say)sstayay)").cast(commit_v);
    let commit = commit.to_tuple();
    let contents = &hex::encode(commit.6);
    let metadata_checksum = &hex::encode(commit.7);
    let metadata_v = &repo.load_variant(ostree::ObjectType::DirMeta, metadata_checksum)?;
    writer.append(ostree::ObjectType::DirMeta, metadata_checksum, metadata_v)?;

    writer.append_dirtree(repo, contents, cancellable)?;
    Ok(())
}

/// Write an ostree directory as an OCI blob
#[context("Writing ostree root to blob")]
fn export_ostree_ref_to_blobdir(
    repo: &ostree::Repo,
    root: &gio::File,
    ostree_commit: &str,
    ocidir: &openat::Dir,
) -> Result<Layer> {
    let cancellable = gio::NONE_CANCELLABLE;
    let mut w = LayerWriter::new(ocidir)?;
    {
        let mut tar = tar::Builder::new(&mut w);
        let path = Utf8Path::new("");
        ostree_metadata_to_tar(repo, ostree_commit, &mut tar, cancellable)?;
        ostree_content_to_tar(repo, path, root, &mut tar, cancellable)?;
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
fn build_oci(repo: &ostree::Repo, root: &gio::File, commit: &str, ocidir: &Path) -> Result<()> {
    let utsname = nix::sys::utsname::uname();
    let arch = MACHINE_TO_OCI[utsname.machine()];
    // Explicitly error if the target exists
    std::fs::create_dir(ocidir).context("Creating OCI dir")?;
    let ocidir = &openat::Dir::open(ocidir)?;
    ocidir.ensure_dir_all(BLOBDIR, 0o755)?;
    ocidir.write_file_contents("oci-layout", 0o644, r#"{"imageLayoutVersion":"1.0.0"}"#)?;

    let rootfs_blob = export_ostree_ref_to_blobdir(repo, root, commit, ocidir)?;
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
    let cancellable = gio::NONE_CANCELLABLE;
    let (root, rev) = repo.read_commit(ostree_ref, cancellable)?;
    match target {
        Target::OciDir(d) => return build_oci(repo, &root, rev.as_str(), d),
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
