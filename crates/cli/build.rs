// 为 hf.exe 嵌入 Windows 版本资源(VS_VERSION_INFO):FileVersion/ProductVersion 取自
// CARGO_PKG_VERSION,发布流水线改写 workspace 版本后 zip 内 exe 即携带真实版本号;
// 否则 Get-Command/文件属性/scoop 一律显示 0.0.0.0
//
// 资源是**打包关注点,不是正确性关注点**。winresource 自己找 rc.exe 的办法是跑
// `reg query HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots`(见其 get_sdk);
// 一旦 reg.exe 被安全策略挡住,SDK 就找不到,rc.exe 随之解析失败,compile() 抛
// `系统找不到指定的路径 (os error 3)`。若因此直接 panic,整个 cli crate 就无法
// cargo check / cargo test —— 任何 cli 改动都退化为盲改。
//
// 因此在两处加固:
//
//   1. **不靠注册表**。自己扫盘找 rc.exe,再用 RC_PATH 交给 winresource ——
//      RC_PATH 是它内部**优先级最高**的取值(见 compile_with_toolkit_msvc 的第一个
//      分支),设了它,注册表那次探测就彻底失去影响。本机 SDK 位于
//      `C:\Program Files (x86)\Windows Kits\10\bin\<ver>\x64\rc.exe`。
//   2. **降级而非崩溃**。扫盘仍找不到才降级为警告并继续;发布流水线用
//      HF_REQUIRE_WINRES=1 恢复硬保证(见 .github/workflows/release.yml),
//      确保发出去的 exe 一定带版本资源。
fn main() {
    #[cfg(target_os = "windows")]
    {
        let required = std::env::var("HF_REQUIRE_WINRES").is_ok_and(|value| value != "0");

        // Only fill RC_PATH when the caller left it unset: an explicit value wins,
        // so an unusual SDK layout (or a cross build) can still override us.
        if std::env::var_os("RC_PATH").is_none() {
            if let Some(rc) = discover_rc() {
                // Plain stdout, not `cargo:warning`: cargo only surfaces it when
                // the script fails or runs with `-vv`, which is exactly when the
                // selected compiler is worth knowing.
                println!("using rc.exe discovered on disk: {}", rc.display());
                std::env::set_var("RC_PATH", &rc);
            }
        }

        if let Err(error) = winresource::WindowsResource::new().compile() {
            // Build-script assert rather than `if required { panic! }`: same
            // semantics (fatal only under the release pipeline), and it keeps
            // this file free of the `manual_assert` advisory lint.
            assert!(
                !required,
                "failed to compile windows resources: {error}"
            );
            println!(
                "cargo:warning=skipped embedded version resource ({error}); \
                 Get-Command/文件属性 will show 0.0.0.0 for this build. \
                 Set HF_REQUIRE_WINRES=1 to make this failure fatal."
            );
        }
    }
}

/// Find `rc.exe` in an on-disk Windows SDK, newest SDK version first.
///
/// Registry-free on purpose. winresource's own lookup (`get_sdk`) shells out to
/// `reg query`; wherever `reg.exe` is blocked by policy that probe fails and the
/// SDK is left unfound, which is precisely how this machine lost rc.exe. A
/// filesystem scan also keeps working when the registry hive is unavailable
/// (containers, locked-down CI images).
///
/// Returns `None` when no SDK is installed; the caller then degrades to a warning.
#[cfg(target_os = "windows")]
fn discover_rc() -> Option<std::path::PathBuf> {
    // Match the architecture we are *compiling for*, not the one we run on.
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => "x64",
    };

    let mut roots: Vec<std::path::PathBuf> = ["ProgramFiles(x86)", "ProgramFiles"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|base| {
            std::path::PathBuf::from(base)
                .join("Windows Kits")
                .join("10")
                .join("bin")
        })
        .collect();
    // The 32-bit default, in case neither variable survived into our environment.
    roots.push(std::path::PathBuf::from(
        r"C:\Program Files (x86)\Windows Kits\10\bin",
    ));

    let mut versions: Vec<std::path::PathBuf> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        versions.extend(entries.flatten().map(|entry| entry.path()));
    }
    // Newest first: `10.0.26100.0` sorts above `10.0.22621.0` lexicographically,
    // which holds as long as the SDK keeps its `10.0.<build>.0` shape. Entries
    // that are not version directories (`x64`, `arm64`, …) simply fail the
    // `is_file` probe below.
    versions.sort_by(|left, right| right.file_name().cmp(&left.file_name()));

    versions
        .into_iter()
        .map(|version| version.join(arch).join("rc.exe"))
        .find(|candidate| candidate.is_file())
}
