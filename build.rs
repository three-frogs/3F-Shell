use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // 仅 Windows（gnullvm 工具链）需要特殊链接处理：
    // gnullvm 工具链缺少 mingw-w64 的 shlwapi 导入库，而 webbrowser crate
    // 仍以传统 #[link(name = "shlwapi")] 方式链接它（仅用于打开外部链接）。
    // 这里做两件事绕过：
    //  1) 生成一个空的 libshlwapi.a，满足链接器对 -lshlwapi 的库查找；
    //  2) 把系统 shlwapi.dll 作为链接器输入文件，lld COFF 后端可直接从 DLL 导入符号。
    // user32.dll 用于 panic 弹窗（崩溃诊断 MessageBox）。
    if target_os == "windows" {
        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
        fs::write(out_dir.join("libshlwapi.a"), b"!<arch>\n").expect("write empty libshlwapi.a");
        println!("cargo:rustc-link-search=native={}", out_dir.display());
        println!("cargo:rustc-link-arg=C:\\Windows\\System32\\shlwapi.dll");
        println!("cargo:rustc-link-arg=C:\\Windows\\System32\\user32.dll");
    }
    // macOS / Linux：无额外链接需求，直接使用系统 framework/库
    println!("cargo:rerun-if-changed=build.rs");
}
