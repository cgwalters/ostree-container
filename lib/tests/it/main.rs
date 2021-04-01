use std::fs::File;

use anyhow::{anyhow, Result};
use camino::{Utf8Path, Utf8PathBuf};
use indoc::indoc;
use sh_inline::bash;

use ostree_container::oci as myoci;

const EXAMPLEOS_TAR: &[u8] = include_bytes!("fixtures/exampleos.tar.zst");
const TESTREF: &str = "exampleos/x86_64/stable";
const CONTENT_CHECKSUM: &str = "0ef7461f9db15e1d8bd8921abf20694225fbaa4462cadf7deed8ea0e43162120";

fn generate_test_oci(dir: &Utf8Path) -> Result<()> {
    let cancellable = gio::NONE_CANCELLABLE;
    let path = Utf8Path::new(dir);
    let tarpath = &path.join("exampleos.tar.zst");
    std::fs::write(tarpath, EXAMPLEOS_TAR)?;
    bash!(
        indoc! {"
        cd {path}
        ostree --repo=repo-archive init --mode=archive
        ostree --repo=repo-archive commit -b {testref} --tree=tar=exampleos.tar.zst
        ostree --repo=repo-archive show {testref}
    "},
        testref = TESTREF,
        path = path.as_str()
    )?;
    std::fs::remove_file(tarpath)?;
    let repopath = &path.join("repo-archive");
    let repo = &ostree::Repo::open_at(libc::AT_FDCWD, repopath.as_str(), cancellable)?;
    let (_, rev) = repo.read_commit(TESTREF, cancellable)?;
    let (commitv, _) = repo.load_commit(rev.as_str())?;
    assert_eq!(
        ostree::commit_get_content_checksum(&commitv)
            .unwrap()
            .as_str(),
        CONTENT_CHECKSUM
    );
    let ocipath = &path.join("exampleos-oci");
    let ocitarget = ostree_container::buildoci::Target::OciDir(ocipath.as_ref());
    ostree_container::buildoci::build(repo, TESTREF, ocitarget)?;
    //bash!(r"skopeo inspect oci:{ocipath}", ocipath = ocipath.as_str())?;
    bash!("ls -al {ocipath}/blobs/sha256", ocipath = ocipath.as_str())?;
    Ok(())
}

fn find_layer_in_oci(ocidir: &Utf8Path) -> Result<Utf8PathBuf> {
    let indexf = std::io::BufReader::new(File::open(ocidir.join("index.json"))?);
    let index: myoci::Index = serde_json::from_reader(indexf)?;
    let manifest = index
        .manifests
        .get(0)
        .ok_or_else(|| anyhow!("Missing manifest in index.json"))?;

    todo!();
}

#[test]
fn test_e2e() -> Result<()> {
    let cancellable = gio::NONE_CANCELLABLE;

    let tempdir = tempfile::tempdir()?;
    let path = Utf8Path::from_path(tempdir.path()).unwrap();
    let srcdir = &path.join("src");
    std::fs::create_dir(srcdir)?;
    generate_test_oci(srcdir)?;
    let destdir = &path.join("dest");
    std::fs::create_dir(destdir)?;
    let destrepodir = &destdir.join("repo");
    let destrepo = ostree::Repo::new_for_path(destrepodir);
    destrepo.create(ostree::RepoMode::Archive, cancellable)?;

    // ostree_container::client::import(repo, )
    Ok(())
}
