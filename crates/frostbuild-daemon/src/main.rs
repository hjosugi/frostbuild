use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(short = 'C', long = "workspace", default_value = ".")]
    workspace: PathBuf,
}

fn main() {
    let args = Args::parse();
    let root = args.workspace.canonicalize().expect("workspace not found");
    if let Err(error) = frostbuild_daemon::serve(&root) {
        eprintln!("frostd: {error:#}");
        std::process::exit(2);
    }
}
