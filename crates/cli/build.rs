use std::path::Path;
use std::process::Command;

fn run(command: &str, args: &[&str], root: &Path) -> Option<String> {
    let output = Command::new(command)
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;

    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn main() {
    let root = Path::new("../..");
    let revision = run(
        "jj",
        &[
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "commit_id ++ if(empty, \"\", \"-dirty\")",
        ],
        root,
    )
    .or_else(|| {
        let revision = run("git", &["rev-parse", "HEAD"], root)?;
        let dirty = run("git", &["status", "--porcelain"], root).is_some();

        Some(if dirty {
            format!("{revision}-dirty")
        } else {
            revision
        })
    })
    .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=MOON_BUILD_REVISION={revision}");
    println!("cargo:rerun-if-changed=../../.jj/working_copy");
    println!("cargo:rerun-if-changed=../../.jj/repo");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
}
