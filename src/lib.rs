use std::time::SystemTime;
use std::{env, fs, path, process};

/// A scoped wrapper for the directory where we'll compile and run the build script.
struct BuildDir {
    pub path: path::PathBuf,
}

impl BuildDir {
    fn new() -> Self {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("Cannot compute duration since UNIX epoch");

        let mut dir = env::temp_dir();
        dir.push(format!("build-script-{}", now.as_secs()));

        BuildDir {
            path: dir,
        }
    }
}

impl Drop for BuildDir {
    fn drop(&mut self) {
        // some paranoia before running 'rm -rf'
        assert!(self.path.starts_with(env::temp_dir()));

        println!("Removing build crate staging dir: {}", self.path.display());
        fs::remove_dir_all(&self.path).expect(&format!(
            "Couldn't clean up build dir: {}",
            self.path.display()
        ));
    }
}

fn cp_r(in_dir: &path::Path, out_dir: &path::Path) {
    for entry in in_dir.read_dir().expect("read_dir call failed") {
        let entry = entry.expect("Cannot access directory entry");
        let entry_type = entry.file_type().expect("Cannot get directory entry file type");
        let entry_path = entry.path();

        let mut out_entry_path = out_dir.to_path_buf();
        out_entry_path.push(entry.file_name());

        if entry_type.is_dir() {
            fs::create_dir_all(out_entry_path.clone()).expect("Cannot create directory");
            cp_r(&entry_path, &out_entry_path);
        } else {
            // Check for potential conflict
            if out_entry_path.exists() {
                if out_entry_path.is_dir() {
                    fs::remove_dir_all(out_entry_path.clone()).expect("Cannot clear conflicting directory");
                } else {
                    fs::remove_file(out_entry_path.clone()).expect("Cannot remove conflicting file");
                }
            }

            let mut out_file = fs::File::create(out_entry_path.clone()).expect(&format!(
                "Couldn't create output file: {}",
                out_entry_path.display()
            ));
            let mut in_file = fs::File::open(entry_path).expect("Cannot open input file");
            std::io::copy(&mut in_file, &mut out_file).expect("Cannot copy file content");
        }
    }
}


fn qualify_cargo_toml_paths_in_text(cargo_toml_content: &str, base_dir: &path::Path) -> String {
    // This is completely manual to avoid introducing any dependencies in this
    // library, since the whole point is to work around dependency issues.

    // Lacking a real parser due to constraints, look for a couple of common
    // patterns. TODO: Roll a little parser for this.
    let mut cargo_toml = cargo_toml_content.to_owned();

    let base_dir = base_dir.to_str().expect("Cannot convert base_path to str").to_string().escape_default().to_string();

    cargo_toml = cargo_toml.replace("path = \"", &format!("path = \"{}/", base_dir));
    cargo_toml = cargo_toml.replace("path=\"", &format!("path=\"{}/", base_dir));
    cargo_toml = cargo_toml.replace("path = '", &format!("path = '{}/", base_dir));
    cargo_toml = cargo_toml.replace("path='", &format!("path='{}/", base_dir));
    cargo_toml
}

fn qualify_cargo_toml_paths(cargo_toml_path: &path::Path, base_dir: &path::Path) {
    let cargo_toml = fs::read_to_string(cargo_toml_path).expect(&format!(
        "Can't read Cargo.toml to stream from {}",
        cargo_toml_path.display()
    ));
    let cargo_toml = qualify_cargo_toml_paths_in_text(&cargo_toml, &base_dir);

    fs::write(cargo_toml_path, cargo_toml).expect(&format!(
        "Failed to write modified Cargo.toml at {}",
        cargo_toml_path.display()
    ));
}

fn compile_build_crate(build_dir: &BuildDir, cargo: &str, temp: &str, path: &str, ssh_auth_sock: &str, rustup_home: &str, rustup_toolchain: &str) {
    // For LLVM dll initialization on Windows.
    let systemroot = env::var("SYSTEMROOT").unwrap_or_default();

    let res = process::Command::new(cargo)
        .args(&["build", "-vv"])
        .env_clear()
        .env("TEMP", temp)
        .env("SYSTEMROOT", systemroot)
        .env("PATH", path)
        .env("SSH_AUTH_SOCK", ssh_auth_sock)
        .env("RUSTUP_HOME", rustup_home)
        .env("RUSTUP_TOOLCHAIN", rustup_toolchain)
        .current_dir(&build_dir.path)
        .stdout(process::Stdio::inherit())
        .stderr(process::Stdio::inherit())
        .output()
        .expect("failed to compile build-script crate");

    assert!(
        res.status.success(),
        "Failed to run compile build crate at {} with {:#?}",
        build_dir.path.display(),
        res
    );
}

fn run_build_script(build_dir: &BuildDir, executable_name: &str, working_dir: &path::Path) {
    // run the build script
    let build_script_path = build_dir
        .path
        .join("target")
        .join("debug")
        .join(executable_name);

    let res = process::Command::new(&build_script_path)
        .current_dir(&working_dir)
        .stdout(process::Stdio::inherit())
        .stderr(process::Stdio::inherit())
        .output()
        .expect(&format!(
            "failed to run build script at {}",
            build_script_path.display()
        ));

    assert!(
        res.status.success(),
        "Failed to run build script at {} with {:#?}",
        build_script_path.display(),
        res
    );
}

pub fn run_build_crate<P: AsRef<path::Path>>(build_crate_src: P) {
    let build_crate_src = build_crate_src.as_ref();
    println!("cargo:rerun-if-changed={}", build_crate_src.display());

    let build_dir = BuildDir::new();

    let executable_name = build_crate_src
        .file_name()
        .and_then(|os_str| os_str.to_str())
        .expect(&format!(
            "Couldn't get file name from build crate src dir: {}",
            build_crate_src.display(),
        ));

    let cargo = env::var("CARGO").expect("Can't get CARGO from env");
    let temp = env::var("TEMP").unwrap_or_default();
    let path = env::var("PATH").expect("Can't get PATH from env");
    let ssh_auth_sock = env::var("SSH_AUTH_SOCK").unwrap_or_default();
    let base_dir = env::var("CARGO_MANIFEST_DIR").expect("Can't get CARGO_MANIFEST_DIR from env");
    let base_dir = path::Path::new(&base_dir).join("build-script");

    let rustup_home = env::var("RUSTUP_HOME").unwrap_or_default();
    let rustup_toolchain = env::var("RUSTUP_TOOLCHAIN").unwrap_or_default();

    // Copy the build crate into /tmp to avoid the influence of .cargo/config
    // settings in the build crate's parent, which cargo gives us no way to
    // ignore.
    println!(
        "Copying build crate source from {} to {}",
        &build_crate_src.display(),
        build_dir.path.display()
    );
    fs::create_dir_all(build_dir.path.clone()).expect("Cannot create build directory");
    cp_r(build_crate_src, &build_dir.path);

    // Having copied the crate, we need to fix any relative paths that were in
    // the Cargo.toml
    qualify_cargo_toml_paths(&build_dir.path.join("Cargo.toml"), &base_dir);

    compile_build_crate(&build_dir, &cargo, &temp, &path, &ssh_auth_sock, &rustup_home, &rustup_toolchain);

    // Run the build script with its original source directory as the working
    // dir.
    run_build_script(&build_dir, &executable_name, &build_crate_src);
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_path_fixup_1() {
        let input = r#"
[dependencies]
lib-crate = { path = "../../lib-crate" }
"#;
        let expected = r#"
[dependencies]
lib-crate = { path = "/basedir/../../lib-crate" }
"#;

        assert_eq!(
            qualify_cargo_toml_paths_in_text(input, path::Path::new("/basedir")),
            expected.to_string()
        );
    }

    #[test]
    fn test_path_fixup_2() {
        let input = r#"
[dependencies]
lib-crate = { path="../../lib-crate" }
"#;
        let expected = r#"
[dependencies]
lib-crate = { path="/basedir/../../lib-crate" }
"#;

        assert_eq!(
            qualify_cargo_toml_paths_in_text(input, path::Path::new("/basedir")),
            expected.to_string()
        );
    }

    #[test]
    fn test_path_fixup_3() {
        let input = r#"
[dependencies]
lib-crate = { path = '../../lib-crate' }
"#;
        let expected = r#"
[dependencies]
lib-crate = { path = '/basedir/../../lib-crate' }
"#;

        assert_eq!(
            qualify_cargo_toml_paths_in_text(input, path::Path::new("/basedir")),
            expected.to_string()
        );
    }

    #[test]
    fn test_path_fixup_4() {
        let input = r#"
[dependencies]
lib-crate = { path='../../lib-crate' }
"#;
        let expected = r#"
[dependencies]
lib-crate = { path='/basedir/../../lib-crate' }
"#;

        assert_eq!(
            qualify_cargo_toml_paths_in_text(input, path::Path::new("/basedir")),
            expected.to_string()
        );
    }

}
