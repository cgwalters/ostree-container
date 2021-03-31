use anyhow::Result;
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
#[structopt(name = "ostree-container")]
#[structopt(rename_all = "kebab-case")]
enum Opt {
    Build(BuildOpts),
}

fn build(opts: &BuildOpts) -> Result<()> {
    let path = Path::new(&opts.oci_dir);
    Ok(ostree_container::buildoci::build(
        &opts.repo,
        &opts.ostree_ref,
        ostree_container::buildoci::Target::OciDir(path),
    )?)
}

fn run() -> Result<()> {
    let opt = Opt::from_args();
    match opt {
        Opt::Build(ref buildopts) => build(buildopts),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
