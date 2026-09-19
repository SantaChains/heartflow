// 为 hf.exe 嵌入 Windows 版本资源(VS_VERSION_INFO):FileVersion/ProductVersion 取自
// CARGO_PKG_VERSION,发布流水线改写 workspace 版本后 zip 内 exe 即携带真实版本号;
// 否则 Get-Command/文件属性/scoop 一律显示 0.0.0.0
fn main() {
    #[cfg(target_os = "windows")]
    if let Err(e) = winresource::WindowsResource::new().compile() {
        panic!("failed to compile windows resources: {e}");
    }
}
