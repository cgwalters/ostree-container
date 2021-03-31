use anyhow::Result;
use camino::Utf8Path;
use indoc::indoc;
use sh_inline::bash;

const EXAMPLEOS_TAR: &[u8] = include_bytes!("fixtures/exampleos.tar.zst");
const TESTREF: &str = "exampleos/x86_64/stable";
const CONTENT_CHECKSUM: &str = "0ef7461f9db15e1d8bd8921abf20694225fbaa4462cadf7deed8ea0e43162120";

fn run() -> Result<()> {
    let cancellable = gio::NONE_CANCELLABLE;
    let tempdir = tempfile::tempdir()?;
    let path = Utf8Path::from_path(tempdir.path()).unwrap();
    std::fs::write(path.join("exampleos.tar.zst"), EXAMPLEOS_TAR)?;
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
    bash!(r"skopeo inspect oci:{ocipath}", ocipath = ocipath.as_str())?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
