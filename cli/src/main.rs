use anyhow::Result;
use ostree_container::client;
use std::path::Path;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
struct BuildOpts {
    #[structopt(long)]
    repo: String,

    #[structopt(long = "ref")]
    ostree_ref: String,

    #[structopt(long)]
    oci_dir: String,
}

#[derive(Debug, StructOpt)]
struct PullOpts {
    #[structopt(long)]
    repo: String,

    /// Source container image location
    imgref: String,
}

#[derive(Debug, StructOpt)]
#[structopt(name = "ostree-container")]
#[structopt(rename_all = "kebab-case")]
enum Opt {
    Build(BuildOpts),
    Pull(PullOpts),
}

fn build(opts: &BuildOpts) -> Result<()> {
    let path = Path::new(&opts.oci_dir);
    let repo = &ostree::Repo::open_at(libc::AT_FDCWD, opts.repo.as_str(), gio::NONE_CANCELLABLE)?;
    Ok(ostree_container::buildoci::build(
        repo,
        &opts.ostree_ref,
        ostree_container::buildoci::Target::OciDir(path),
    )?)
}

async fn pull(opt: &PullOpts) -> Result<()> {
    let repo = &ostree::Repo::open_at(libc::AT_FDCWD, opt.repo.as_str(), gio::NONE_CANCELLABLE)?;
    let res = client::import(repo, &opt.imgref).await?;
    println!("Imported: {}", res.ostree_commit);
    Ok(())
}

async fn run() -> Result<()> {
    let opt = Opt::from_args();
    match opt {
        Opt::Build(ref opt) => build(opt),
        Opt::Pull(ref opt) => pull(opt).await,
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
