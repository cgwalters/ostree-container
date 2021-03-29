//! APIs for creating container images from OSTree commits

use super::Result;

use anyhow::{anyhow, Context};
use flate2::write::GzEncoder;
use fn_error_context::context;
use gio::prelude::*;
use openat_ext::*;
use openssl::hash::{Hasher, MessageDigest};
use ostree::prelude::*;
use phf::phf_map;
use std::io::prelude::*;
use std::path::Path;

/// Map the value from `uname -m` to the Go architecture.
/// TODO find a more canonical home for this.
static MACHINE_TO_OCI: phf::Map<&str, &str> = phf_map! {
    "x86_64" => "amd64",
    "aarch64" => "arm64",
};

const OCI_TYPE_CONFIG_JSON: &str = "application/vnd.oci.image.config.v1+json";
const OCI_TYPE_MANIFEST_JSON: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_TYPE_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const BLOBDIR: &str = "blobs/sha256";
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

fn gio_filetype_to_tar(t: gio::FileType) -> tar::EntryType {
    match t {
        gio::FileType::Regular => tar::EntryType::Regular,
        gio::FileType::SymbolicLink => tar::EntryType::Symlink,
        gio::FileType::Directory => tar::EntryType::Directory,
        o => panic!("Unexpected FileType: {:?}", o),
    }
}

fn ostree_to_tar<W: std::io::Write, C: IsA<gio::Cancellable>>(
    repo: &ostree::Repo,
    dirpath: &Path,
    f: &gio::File,
    out: &mut tar::Builder<W>,
    cancellable: Option<&C>,
) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    let i = f.query_info(DEFAULT_QUERYINFO_ATTRS, QUERYINFO_FLAGS, cancellable)?;
    let name = &i.get_name().unwrap_or_else(|| ".".into());
    let path = &dirpath.join(name);
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
                ostree_to_tar(repo, path, child, out, cancellable)?;
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

#[context("Writing ostree root to blob")]
fn export_ostree_ref_to_blobdir(
    repo: &ostree::Repo,
    root: &gio::File,
    ocidir: &openat::Dir,
) -> Result<Layer> {
    let mut w = LayerWriter::new(ocidir)?;
    {
        let mut tar = tar::Builder::new(&mut w);
        let path = Path::new("");
        ostree_to_tar(repo, path, root, &mut tar, gio::NONE_CANCELLABLE)?;
        tar.finish()?;
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
fn build_oci(repo: &ostree::Repo, root: &gio::File, ocidir: &Path) -> Result<()> {
    let utsname = nix::sys::utsname::uname();
    let arch = MACHINE_TO_OCI[utsname.machine()];
    // Explicitly error if the target exists
    std::fs::create_dir(ocidir).context("Creating OCI dir")?;
    let ocidir = &openat::Dir::open(ocidir)?;
    ocidir.ensure_dir_all(BLOBDIR, 0o755)?;
    ocidir.write_file_contents("oci-layout", 0o644, r#"{"imageLayoutVersion":"1.0.0"}"#)?;

    let rootfs_blob = export_ostree_ref_to_blobdir(repo, root, ocidir)?;
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

fn build_impl(repo: &ostree::Repo, ostree_ref: &str, target: Target) -> Result<()> {
    let cancellable = gio::NONE_CANCELLABLE;
    let (root, _) = repo.read_commit(ostree_ref, cancellable)?;
    match target {
        Target::OciDir(d) => return build_oci(repo, &root, d),
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
