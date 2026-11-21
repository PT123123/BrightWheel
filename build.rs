use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile ui/app.slint");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icon = manifest.join("assets").join("app.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let rc_source = out.join("app.rc");
    let icon_path = icon.to_string_lossy().replace('\\', "/");
    fs::write(&rc_source, format!("1 ICON \"{icon_path}\"\n")).expect("failed to write .rc");

    let compiler = find_resource_compiler().expect("Windows SDK resource compiler rc.exe not found");
    let resource = out.join("app.res");
    let status = Command::new(&compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(&resource)
        .arg(&rc_source)
        .status()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", compiler.display()));
    assert!(status.success(), "rc.exe failed with status {status}");
    println!("cargo:rustc-link-arg-bin=BrightWheel={}", resource.display());
}

fn find_resource_compiler() -> Option<PathBuf> {
    if let Some(path) = env::var_os("WindowsSdkVerBinPath") {
        let candidate = PathBuf::from(path).join("x64").join("rc.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let root = PathBuf::from(env::var_os("ProgramFiles(x86)")?).join(r"Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    versions.sort_unstable_by(|left, right| right.file_name().cmp(&left.file_name()));
    versions
        .into_iter()
        .map(|version| version.join("x64").join("rc.exe"))
        .find(|candidate| candidate.is_file())
}
