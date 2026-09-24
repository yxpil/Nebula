// Windows 构建脚本:把应用图标与版本信息嵌入 nebula.exe。
//
// 编译失败(例如缺少 rc.exe)不阻断整体构建,只打印警告 ——
// 缺资源的 exe 依然功能完整。

fn main() {
    println!("cargo:rerun-if-changed=../../assets/nebula.ico");

    #[cfg(target_os = "windows")]
    {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".into());
        let mut res = winres::WindowsResource::new();
        res.set_icon("../../assets/nebula.ico");
        res.set("FileDescription", "Nebula 个人记忆检索引擎");
        res.set("ProductName", "Nebula");
        res.set("LegalCopyright", "MIT License");
        res.set("FileVersion", &version);
        res.set("ProductVersion", &version);
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows resources: {e}");
        }
    }
}
