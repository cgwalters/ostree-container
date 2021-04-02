//! APIs for creating container images from OSTree commits

use super::Result;

use crate::oci;
use crate::ostree_ext::*;
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use fn_error_context::context;
use gio::prelude::*;
use gvariant::aligned_bytes::TryAsAligned;
use gvariant::{gv, Marker, Structure};

use std::{borrow::Cow, collections::HashSet, path::Path};

// This way the default ostree -> sysroot/ostree symlink works.
const OSTREEDIR: &str = "sysroot/ostree";

/// The location to store the generated image
pub enum Target<'a> {
    /// Generate an Open Containers image directory layout
    OciDir(&'a Path),
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
    wrote_xattrs: HashSet<String>,
}

fn object_path(objtype: ostree::ObjectType, checksum: &str) -> Utf8PathBuf {
    let suffix = match objtype {
        ostree::ObjectType::Commit => "commit",
        ostree::ObjectType::CommitMeta => "commitmeta",
        ostree::ObjectType::DirTree => "dirtree",
        ostree::ObjectType::DirMeta => "dirmeta",
        ostree::ObjectType::File => "file",
        o => panic!("Unexpected object type: {:?}", o),
    };
    let (first, rest) = checksum.split_at(2);
    format!("{}/repo/objects/{}/{}.{}", OSTREEDIR, first, rest, suffix).into()
}

fn xattrs_path(checksum: &str) -> Utf8PathBuf {
    format!("{}/repo/xattrs/{}", OSTREEDIR, checksum).into()
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

    fn append_xattrs(
        &mut self,
        xattrs: &glib::Variant,
    ) -> Result<Option<(Utf8PathBuf, tar::Header)>> {
        let xattrs_data = xattrs.get_data_as_bytes();
        let xattrs_data = xattrs_data.as_ref();
        if xattrs_data.is_empty() {
            return Ok(None);
        }

        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(0);
        let digest = openssl::hash::hash(openssl::hash::MessageDigest::sha256(), xattrs_data)?;
        let mut hexbuf = [0u8; 64];
        hex::encode_to_slice(digest, &mut hexbuf)?;
        let checksum = std::str::from_utf8(&hexbuf)?;
        let path = xattrs_path(checksum);

        if !self.wrote_xattrs.contains(checksum) {
            let inserted = self.wrote_xattrs.insert(checksum.to_string());
            debug_assert!(inserted);
            let mut target_header = h.clone();
            target_header.set_size(xattrs_data.len() as u64);
            self.out
                .append_data(&mut target_header, &path, xattrs_data)?;
        }
        Ok(Some((path, h)))
    }

    fn append_content(&mut self, checksum: &str) -> Result<(Utf8PathBuf, tar::Header)> {
        let path = object_path(ostree::ObjectType::File, checksum);

        let (instream, meta, xattrs) = self.repo.load_file(checksum, gio::NONE_CANCELLABLE)?;
        let meta = meta.unwrap();
        let xattrs = xattrs.unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_uid(meta.get_attribute_uint32("unix::uid") as u64);
        h.set_gid(meta.get_attribute_uint32("unix::gid") as u64);
        h.set_mode(meta.get_attribute_uint32("unix::mode"));
        let mut target_header = h.clone();
        target_header.set_size(0);

        if !self.wrote_content.contains(checksum) {
            let inserted = self.wrote_content.insert(checksum.to_string());
            debug_assert!(inserted);

            if let Some((xattrspath, mut xattrsheader)) = self.append_xattrs(&xattrs)? {
                xattrsheader.set_entry_type(tar::EntryType::Link);
                xattrsheader.set_link_name(xattrspath)?;
                let subpath = format!("{}.xattrs", path);
                self.out
                    .append_data(&mut xattrsheader, subpath, &mut std::io::empty())?;
            }

            if let Some(instream) = instream {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(meta.get_size() as u64);
                let mut instream = instream.into_read();
                self.out.append_data(&mut h, &path, &mut instream)?;
            } else {
                h.set_size(0);
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
        h.set_entry_type(tar::EntryType::Directory);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(0o755);
        h.set_size(0);
        let path = format!("{}/repo/objects/{:#04x}", OSTREEDIR, d);
        out.append_data(&mut h, &path, &mut std::io::empty())?;
    }

    // Write out the xattrs directory
    {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_size(0);
        let path = format!("{}/repo/xattrs", OSTREEDIR);
        out.append_data(&mut h, &path, &mut std::io::empty())?;
    }

    let writer = &mut OstreeMetadataWriter {
        repo,
        out,
        wrote_dirmeta: HashSet::new(),
        wrote_dirtree: HashSet::new(),
        wrote_content: HashSet::new(),
        wrote_xattrs: HashSet::new(),
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
    rev: &str,
    ocidir: &openat::Dir,
) -> Result<oci::Layer> {
    let commit = repo.resolve_rev(rev, false)?;
    let cancellable = gio::NONE_CANCELLABLE;
    let mut w = oci::LayerWriter::new(ocidir)?;
    {
        let mut tar = tar::Builder::new(&mut w);
        ostree_metadata_to_tar(repo, commit.unwrap().as_str(), &mut tar, cancellable)?;
        tar.finish()?;
    }
    w.complete()
}

/// Generate an OCI image from a given ostree root
#[context("Building oci")]
fn build_oci(repo: &ostree::Repo, commit: &str, ocidir: &Path) -> Result<()> {
    // Explicitly error if the target exists
    std::fs::create_dir(ocidir).context("Creating OCI dir")?;
    let ocidir = &openat::Dir::open(ocidir)?;
    let writer = &mut oci::OciWriter::new(ocidir)?;

    let rootfs_blob = export_ostree_ref_to_blobdir(repo, commit, ocidir)?;
    writer.set_root_layer(rootfs_blob);
    writer.complete()?;

    Ok(())
}

/// Helper for `build()` that avoids generics
fn build_impl(repo: &ostree::Repo, ostree_ref: &str, target: Target) -> Result<()> {
    match target {
        Target::OciDir(d) => build_oci(repo, ostree_ref, d),
    }
}

/// Given an OSTree repository and ref, generate a container image
pub fn build<S: AsRef<str>>(repo: &ostree::Repo, ostree_ref: S, target: Target) -> Result<()> {
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
