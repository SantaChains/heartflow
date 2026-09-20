# 安装

Windows amd64 便携包与 crates.io 包随每次发布自动产出，四条安装通道：

**scoop（Windows，推荐，带自动更新）**

```powershell
scoop bucket add heartflow https://github.com/SantaChains/heartflow
scoop install heartflow
```

**cargo（crates.io）**

```bash
cargo install heartflow
```

**cargo（直接从 Git，不经过 crates.io）**

```bash
cargo install --git https://github.com/SantaChains/heartflow --locked heartflow
```

**便携 zip 直下**：到 [Releases](https://github.com/SantaChains/heartflow/releases) 下载 `heartflow-<版本>-win-amd64.zip`（内含 `hf.exe`），解压后把目录加入 PATH。

> 命令名是 **`hf`**（不是 heartflow）。

## 从源码构建

需要 Rust 1.88+。

```bash
# 克隆后在仓库根目录（含 Cargo.toml）执行
git clone https://github.com/SantaChains/heartflow.git
cd heartflow
cargo build --release
target/release/hf.exe --help   # Linux/macOS 为 target/release/hf
```
