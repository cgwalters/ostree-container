//! APIs for extracting OSTree commits from container images

use std::collections::HashMap;

use crate::variant_utils::variant_new_from_bytes;

use super::Result;
use anyhow::anyhow;
use camino::Utf8Path;
use fn_error_context::context;

use oci_distribution::manifest::OciDescriptor;

const MAX_XATTR_SIZE: u32 = 1024 * 1024;

const OSTREE_COMMIT_FORMAT: &str = "(a{sv}aya(say)sstayay)";
const OSTREE_DIRTREE_FORMAT: &str = "(a(say)a(sayay))";
const OSTREE_DIRMETA_FORMAT: &str = "(uuua(ayay))";
const OSTREE_XATTRS_FORMAT: &str = "a(ayay)";

/// The result of an imported container with an embedded ostree.
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

#[context("Importing {}", image_ref)]
async fn import_impl(_repo: &ostree::Repo, image_ref: &str) -> Result<Import> {
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
    let mut out: Vec<u8> = Vec::new();
    client
        .pull_layer(&image_ref, &layer.digest, &mut out)
        .await?;

    Ok(Import {
        ostree_commit: "none".to_string(),
        image_digest,
    })
}

/// Download and import the referenced container
pub async fn import<I: AsRef<str>>(repo: &ostree::Repo, image_ref: I) -> Result<Import> {
    Ok(import_impl(repo, image_ref.as_ref()).await?)
}

#[derive(Debug, PartialEq, Eq)]
enum ImportState {
    Initial,
    Importing(String),
}

struct Importer<'a> {
    state: ImportState,
    repo: &'a ostree::Repo,
    xattrs: HashMap<String, glib::Variant>,
    next_xattrs: Option<(String, String)>,
}

impl<'a> Drop for Importer<'a> {
    fn drop(&mut self) {
        let _ = self.repo.abort_transaction(gio::NONE_CANCELLABLE);
    }
}

fn validate_metadata_header(header: &tar::Header, desc: &str) -> Result<usize> {
    if header.entry_type() != tar::EntryType::Regular {
        return Err(anyhow!("Invalid non-regular metadata object {}", desc));
    }
    let size = header.size()?;
    let max_size = 10u64 * 1024 * 1024;
    if size > max_size {
        return Err(anyhow!(
            "object of size {} exceeds {} bytes",
            size,
            max_size
        ));
    }
    Ok(size as usize)
}

fn header_to_gfileinfo(header: &tar::Header) -> Result<gio::FileInfo> {
    let i = gio::FileInfo::new();
    let t = match header.entry_type() {
        tar::EntryType::Regular => gio::FileType::Regular,
        tar::EntryType::Symlink => gio::FileType::SymbolicLink,
        o => return Err(anyhow!("Invalid tar type: {:?}", o)),
    };
    i.set_file_type(t);
    i.set_size(0);
    if t == gio::FileType::Regular {
        i.set_size(header.size()? as i64)
    } else {
        let target = header.link_name()?;
        let target = target.ok_or_else(|| anyhow!("Invalid symlink"))?;
        let target = target
            .as_os_str()
            .to_str()
            .ok_or_else(|| anyhow!("Non-utf8 symlink"))?;
        i.set_symlink_target(target);
    }

    Ok(i)
}

fn format_for_objtype(t: ostree::ObjectType) -> Option<&'static str> {
    match t {
        ostree::ObjectType::DirTree => Some(OSTREE_DIRTREE_FORMAT),
        ostree::ObjectType::DirMeta => Some(OSTREE_DIRMETA_FORMAT),
        ostree::ObjectType::Commit => Some(OSTREE_COMMIT_FORMAT),
        _ => None,
    }
}

fn objtype_from_string(t: &str) -> Option<ostree::ObjectType> {
    Some(match t {
        "commit" => ostree::ObjectType::Commit,
        "dirtree" => ostree::ObjectType::DirTree,
        "dirmeta" => ostree::ObjectType::DirMeta,
        "file" => ostree::ObjectType::File,
        _ => return None,
    })
}

fn entry_to_variant<R: std::io::Read>(
    mut entry: tar::Entry<R>,
    vtype: &str,
    desc: &str,
) -> Result<glib::Variant> {
    let header = entry.header();
    let size = validate_metadata_header(header, desc)?;

    let mut buf: Vec<u8> = Vec::with_capacity(size);
    let n = std::io::copy(&mut entry, &mut buf)?;
    assert_eq!(n as usize, size);
    let v = glib::Bytes::from_owned(buf);
    Ok(crate::variant_utils::variant_new_from_bytes(
        vtype, v, false,
    ))
}

impl<'a> Importer<'a> {
    fn import_commit<R: std::io::Read>(
        &mut self,
        entry: tar::Entry<R>,
        checksum: &str,
    ) -> Result<()> {
        assert_eq!(self.state, ImportState::Initial);
        self.import_metadata(entry, checksum, ostree::ObjectType::Commit)?;
        self.state = ImportState::Importing(checksum.to_string());
        Ok(())
    }

    fn import_metadata<R: std::io::Read>(
        &mut self,
        entry: tar::Entry<R>,
        checksum: &str,
        objtype: ostree::ObjectType,
    ) -> Result<()> {
        let vtype =
            format_for_objtype(objtype).ok_or_else(|| anyhow!("Unhandled objtype {}", objtype))?;
        let v = entry_to_variant(entry, vtype, checksum)?;
        // FIXME insert expected dirtree/dirmeta
        let _ = self
            .repo
            .write_metadata(objtype, Some(checksum), &v, gio::NONE_CANCELLABLE)?;
        Ok(())
    }

    #[context("Processing content object {}", checksum)]
    fn import_content_object<R: std::io::Read>(
        &self,
        entry: tar::Entry<R>,
        checksum: &str,
        xattrs: Option<&glib::Variant>,
    ) -> Result<()> {
        let size = entry.size();
        let i = header_to_gfileinfo(entry.header())?;
        //let entry = gio::ReadInputStream::new(entry);
        Ok(())
    }

    #[context("Importing object {}", path)]
    fn import_object<'b, R: std::io::Read>(
        &mut self,
        entry: tar::Entry<'b, R>,
        path: &Utf8Path,
    ) -> Result<()> {
        let parentname = path
            .parent()
            .map(|p| p.file_name())
            .flatten()
            .ok_or_else(|| anyhow!("Invalid path (no parent) {}", path))?;
        if parentname.len() != 2 {
            return Err(anyhow!("Invalid checksum parent {}", parentname));
        }
        let mut name = path
            .file_name()
            .map(Utf8Path::new)
            .ok_or_else(|| anyhow!("Invalid path (dir) {}", path))?;
        let mut objtype = name
            .extension()
            .ok_or_else(|| anyhow!("Invalid objpath {}", path))?;
        let is_xattrs = objtype == "xattrs";
        let xattrs = self.next_xattrs.take();
        if is_xattrs {
            if xattrs.is_some() {
                return Err(anyhow!("Found multiple xattrs"));
            }
            name = name
                .file_stem()
                .map(Utf8Path::new)
                .ok_or_else(|| anyhow!("Invalid xattrs {}", path))?;
            objtype = name
                .extension()
                .ok_or_else(|| anyhow!("Invalid objpath {}", path))?;
        }
        let checksum_rest = name
            .file_stem()
            .ok_or_else(|| anyhow!("Invalid objpath {}", path))?;

        if checksum_rest.len() != 62 {
            return Err(anyhow!("Invalid checksum rest {}", name));
        }
        let checksum = format!("{}{}", parentname, checksum_rest);
        validate_sha256(&checksum)?;
        let xattr_ref = if let Some((xattr_target, xattr_objref)) = xattrs {
            if xattr_target.as_str() != checksum.as_str() {
                return Err(anyhow!(
                    "Found object {} but previous xattr was {}",
                    checksum,
                    xattr_target
                ));
            }
            let v = self
                .xattrs
                .get(&xattr_objref)
                .ok_or_else(|| anyhow!("Failed to find xattr {}", xattr_objref))?;
            Some(v)
        } else {
            None
        };
        let objtype = objtype_from_string(&objtype)
            .ok_or_else(|| anyhow!("Invalid object type {}", objtype))?;
        dbg!(&checksum, objtype);
        match (objtype, is_xattrs, &self.state) {
            (ostree::ObjectType::Commit, _, ImportState::Initial) => {
                self.import_commit(entry, &checksum)
            }
            (ostree::ObjectType::File, true, ImportState::Importing(_)) => {
                self.import_xattr_ref(entry, checksum)
            }
            (ostree::ObjectType::File, false, ImportState::Importing(_)) => {
                self.import_content_object(entry, &checksum, xattr_ref)
            }
            (objtype, false, ImportState::Importing(_)) => {
                self.import_metadata(entry, &checksum, objtype)
            }
            (o, _, ImportState::Initial) => {
                return Err(anyhow!("Found content object {} before commit", o))
            }
            (ostree::ObjectType::Commit, _, ImportState::Importing(c)) => {
                return Err(anyhow!("Found multiple commit objects; original: {}", c))
            }
            (objtype, true, _) => {
                return Err(anyhow!("Found xattrs for non-file object type {}", objtype))
            }
        }
    }

    #[context("Processing xattr ref")]
    fn import_xattr_ref<'b, R: std::io::Read>(
        &mut self,
        entry: tar::Entry<'b, R>,
        target: String,
    ) -> Result<()> {
        assert!(self.next_xattrs.is_none());
        let header = entry.header();
        if header.entry_type() != tar::EntryType::Link {
            return Err(anyhow!("Non-hardlink xattr reference found for {}", target));
        }
        let xattr_target = entry
            .link_name()?
            .ok_or_else(|| anyhow!("No xattr link content for {}", target))?;
        let xattr_target = Utf8Path::from_path(&*xattr_target)
            .ok_or_else(|| anyhow!("Invalid non-UTF8 xattr link {}", target))?;
        let xattr_target = xattr_target
            .file_name()
            .ok_or_else(|| anyhow!("Invalid xattr link {}", target))?;
        validate_sha256(xattr_target)?;
        self.next_xattrs = Some((target, xattr_target.to_string()));
        dbg!(&self.next_xattrs);
        Ok(())
    }

    fn import_xattrs<'b, R: std::io::Read>(&mut self, mut entry: tar::Entry<'b, R>) -> Result<()> {
        match &self.state {
            ImportState::Initial => return Err(anyhow!("Found xattr object {} before commit")),
            ImportState::Importing(_) => {}
        }
        let checksum = {
            let path = entry.path()?;
            let name = path
                .file_name()
                .ok_or_else(|| anyhow!("Invalid xattr dir: {:?}", path))?;
            let name = name
                .to_str()
                .ok_or_else(|| anyhow!("Invalid non-UTF8 xattr name: {:?}", name))?;
            validate_sha256(name)?;
            name.to_string()
        };
        let header = entry.header();
        if header.entry_type() != tar::EntryType::Regular {
            return Err(anyhow!(
                "Invalid xattr entry of type {:?}",
                header.entry_type()
            ));
        }
        let n = header.size()?;
        if n > MAX_XATTR_SIZE as u64 {
            return Err(anyhow!("Invalid xattr size {}", n));
        }

        let mut contents = Vec::with_capacity(n as usize);
        let c = std::io::copy(&mut entry, &mut contents)?;
        assert_eq!(c, n);
        let contents: glib::Bytes = contents.as_slice().into();
        let contents = variant_new_from_bytes(OSTREE_XATTRS_FORMAT, contents, false);

        self.xattrs.insert(checksum, contents);
        Ok(())
    }

    fn commit(mut self) -> Result<String> {
        self.repo.commit_transaction(gio::NONE_CANCELLABLE)?;
        match std::mem::replace(&mut self.state, ImportState::Initial) {
            ImportState::Importing(c) => Ok(c),
            ImportState::Initial => Err(anyhow!("Failed to find a commit object to import")),
        }
    }
}

fn validate_sha256(s: &str) -> Result<()> {
    if s.len() != 64 {
        return Err(anyhow!("Invalid sha256 checksum (len) {}", s));
    }
    if !s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        return Err(anyhow!("Invalid sha256 checksum {}", s));
    }
    Ok(())
}

/// Import a tarball
#[context("Importing")]
pub fn import_tarball(repo: &ostree::Repo, src: impl std::io::Read) -> Result<String> {
    let mut importer = Importer {
        state: ImportState::Initial,
        repo,
        xattrs: Default::default(),
        next_xattrs: None,
    };
    repo.prepare_transaction(gio::NONE_CANCELLABLE)?;
    let ungz = flate2::read::GzDecoder::new(src);
    let mut archive = tar::Archive::new(ungz);
    for entry in archive.entries()? {
        let entry = entry?;
        if entry.header().entry_type() == tar::EntryType::Directory {
            continue;
        }
        let path = entry.path()?;
        let path = &*path;
        let path =
            Utf8Path::from_path(path).ok_or_else(|| anyhow!("Invalid non-utf8 path {:?}", path))?;
        let path = if let Ok(p) = path.strip_prefix("sysroot/ostree/repo/") {
            p
        } else {
            continue;
        };

        if let Ok(p) = path.strip_prefix("objects/") {
            // Need to clone here, otherwise we borrow from the moved entry
            let p = &p.to_owned();
            importer.import_object(entry, p)?;
        } else if let Ok(_) = path.strip_prefix("xattrs/") {
            importer.import_xattrs(entry)?;
        }
    }

    importer.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_sha256() -> Result<()> {
        validate_sha256("a86d80a3e9ff77c2e3144c787b7769b300f91ffd770221aac27bab854960b964")?;
        assert!(validate_sha256("").is_err());
        assert!(validate_sha256(
            "a86d80a3e9ff77c2e3144c787b7769b300f91ffd770221aac27bab854960b9644"
        )
        .is_err());
        assert!(validate_sha256(
            "a86d80a3E9ff77c2e3144c787b7769b300f91ffd770221aac27bab854960b964"
        )
        .is_err());
        Ok(())
    }
}
