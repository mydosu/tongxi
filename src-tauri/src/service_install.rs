//! 受管 DSH 槽位的只读路径与状态解析。
//!
//! 本批次只做读取：不下载、不安装、不写状态，也不扫描外部安装目录中的其它文件。
//! 受管布局：`<data>/managed-services/dsh-win/{state.json, slots/<uuid>/...}`。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 受管根目录名。
const MANAGED_DIR: &str = "managed-services";
/// 槽位集合目录名。
const SLOTS_DIR: &str = "slots";
/// 状态文件名。
const STATE_FILE: &str = "state.json";
/// node_modules 目录名。
const NODE_MODULES_DIR: &str = "node_modules";
/// Windows 文件属性中的重解析点标志位。
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// 一个受管成员的安装布局。
///
/// 原来这些值是写死的 DSH 常量；现在收进描述符，好让 Codex 用同一套「隔离槽位 + 指纹 + 切换 +
/// 回退」机制。**注意：槽位机制只会写 `<data>/managed-services/<slot>/`，绝不改动成员原本装在哪
/// （外部安装路径只在没有激活槽时作为回退读出来）。**
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    /// `managed-services/<slot>` 的目录名，例如 `dsh-win`。
    pub(crate) slot: &'static str,
    /// 受管包名，例如 `@deepseek-ai/dsh`。
    pub(crate) package: &'static str,
    /// node_modules 内的 scope 父目录名；必须与叶子包目录分开独立检查。
    pub(crate) scope: &'static str,
    /// 覆盖外部安装路径的环境变量名。
    pub(crate) installation_env: &'static str,
    /// PATH 上用于反推外部安装目录的 CLI 壳名（`codex` / `dsh`）。
    pub(crate) cli: &'static str,
    /// 从槽位根解析可执行文件的相对路径；`None` 表示这个成员没有独立可执行文件（走 node 跑包）。
    pub(crate) executable: Option<&'static str>,
}

impl Layout {
    /// DSH：node 直接跑包，没有独立 exe。
    pub(crate) const DSH: Layout = Layout {
        slot: "dsh-win",
        package: "@deepseek-ai/dsh",
        scope: "@deepseek-ai",
        installation_env: "AGENT_HUB_DSH_INSTALLATION",
        cli: "dsh",
        executable: None,
    };

    /// Codex：原生 exe 在平台子包里（与 `codex::discover_executable` 的候选路径一致）。
    pub(crate) const CODEX: Layout = Layout {
        slot: "codex-win",
        package: "@openai/codex",
        scope: "@openai",
        installation_env: "AGENT_HUB_CODEX_INSTALLATION",
        cli: "codex",
        executable: Some(
            "node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe",
        ),
    };
}

/// 按受管成员 id 取布局；不是受管成员返回 `None`。
pub(crate) fn layout_for(service_id: &str) -> Option<Layout> {
    match service_id {
        "dsh-win" => Some(Layout::DSH),
        "codex-win" => Some(Layout::CODEX),
        _ => None,
    }
}

/// `state.json` 的模型；出现未知字段即视为损坏，不做任何宽松兜底。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstallState {
    pub revision: u64,
    pub active: Option<String>,
    pub previous: Option<String>,
}

/// 受管根目录：`<data>/managed-services/<layout.slot>`。数据目录必须为绝对路径；
/// 已存在的受管子组件若是符号链接/重解析点则报错；目录不存在时只返回路径，不创建。
pub(crate) fn root_for(data: &Path, layout: &Layout) -> Result<PathBuf, String> {
    if !data.is_absolute() {
        return Err(format!("数据目录必须是绝对路径: {}", data.display()));
    }
    let canonical = data
        .canonicalize()
        .map_err(|error| format!("数据目录不可用 {}: {error}", data.display()))?;
    // canonicalize 在 Windows 上返回 `\\?\D:\...` 扩展长度形式；npm 无法解析这种路径
    // （Invalid file: URL, must comply with RFC 8089），故只去掉本地盘符前缀。
    let canonical = strip_verbatim_prefix(canonical);
    let managed = canonical.join(MANAGED_DIR);
    ensure_no_reparse(&managed)?;
    let root = managed.join(layout.slot);
    ensure_no_reparse(&root)?;
    Ok(root)
}

/// 去掉仅本地绝对盘符路径的 Windows verbatim 前缀；UNC 与其它形式原样返回。
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    if let Some(text) = path.to_str() {
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            if rest.as_bytes().get(1) == Some(&b':') {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

/// 读取 `root/state.json`。仅“文件不存在”回退为默认值；
/// 其它文件错误、JSON 语法/类型错误、未知字段、非法槽位标识一律报错。
pub(crate) fn read_state(data: &Path) -> Result<InstallState, String> {
    read_state_for(data, &Layout::DSH)
}

/// 读取 `<root>/state.json`。仅“文件不存在”回退为默认值；
/// 其它文件错误、JSON 语法/类型错误、未知字段、非法槽位标识一律报错。
pub(crate) fn read_state_for(data: &Path, layout: &Layout) -> Result<InstallState, String> {
    let path = root_for(data, layout)?.join(STATE_FILE);
    ensure_no_reparse(&path)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InstallState::default())
        }
        Err(error) => return Err(format!("无法读取 {}: {error}", path.display())),
    };
    let state: InstallState = serde_json::from_slice(&bytes)
        .map_err(|error| format!("状态文件损坏 {}: {error}", path.display()))?;
    parse_slot_id(state.active.as_deref())?;
    parse_slot_id(state.previous.as_deref())?;
    Ok(state)
}

/// 槽位目录：`<root>/slots/<uuid>`。拒绝非规范 UUID 以及已存在组件中的重解析点。
pub(crate) fn slot_root_for(data: &Path, layout: &Layout, id: &str) -> Result<PathBuf, String> {
    let id = parse_slot_id(Some(id))?;
    let id = id.ok_or_else(|| "槽位标识缺失".to_string())?;
    let slots = root_for(data, layout)?.join(SLOTS_DIR);
    ensure_no_reparse(&slots)?;
    let slot = slots.join(id);
    ensure_no_reparse(&slot)?;
    Ok(slot)
}

/// 找 CLI 可执行文件的候选目录：`PATH` 的每一项，加上 Windows 上 npm -g 的默认前缀
/// `%APPDATA%/npm`（`codex.rs` 的 PATH 扫描用同一组根）。
fn path_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    if let Some(app_data) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(app_data).join("npm"));
    }
    roots
}

/// 在这些目录里按顺序找第一个存在的文件。
fn find_in(roots: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    roots
        .iter()
        .flat_map(|root| names.iter().map(move |name| root.join(name)))
        .find(|candidate| candidate.is_file())
}

/// 在 PATH 上找一个存在的可执行文件（含 `.cmd` / `.exe` 壳）。
pub(crate) fn on_path(names: &[&str]) -> Option<PathBuf> {
    find_in(&path_roots(), names)
}

/// 从 CLI 壳反推 npm 式安装的包目录：壳所在目录下的 `node_modules/<scope>/<package>`。
/// npm -g 全局前缀（`<prefix>/codex.cmd`）与自带 `node_modules` 的独立安装
/// （`<dir>/bin/dsh.cmd`）两种布局都成立——所以默认值不是某个人的安装路径。
fn installation_in(roots: &[PathBuf], layout: &Layout) -> Option<PathBuf> {
    let names = [
        format!("{}.cmd", layout.cli),
        format!("{}.exe", layout.cli),
        layout.cli.to_string(),
    ];
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let shell = find_in(roots, &names)?;
    let package = shell.parent()?.join(NODE_MODULES_DIR).join(layout.package);
    package.is_dir().then_some(package)
}

/// 外部安装包路径：`layout.installation_env` 已是完整包目录，直接原样使用，不再拼接 `node_modules`；
/// 未设置时按 PATH 上的 CLI 反推；都找不到就报错——不猜本机的安装位置。
pub(crate) fn external(layout: &Layout) -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os(layout.installation_env).filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(value));
    }
    installation_in(&path_roots(), layout).ok_or_else(|| {
        format!(
            "没找到 {} 的安装目录：PATH 上没有 {}，也凑不出 node_modules 布局；请用 {} 指定",
            layout.package, layout.cli, layout.installation_env
        )
    })
}

/// 当前包路径。`active` 为 `Some` 时必须指向受管槽位内合法包（缺失即报错，不偷偷回退）；
/// `None` 时返回外部安装路径。
pub(crate) fn dsh_path(data: &Path) -> Result<PathBuf, String> {
    package_path_for(data, &Layout::DSH)
}

/// 当前包路径（按布局）。语义同 `dsh_path`：没有激活槽就用外部安装，激活槽坏了就报错。
pub(crate) fn package_path_for(data: &Path, layout: &Layout) -> Result<PathBuf, String> {
    let state = read_state_for(data, layout)?;
    let Some(active) = state.active else {
        return external(layout);
    };
    let node_modules = slot_root_for(data, layout, &active)?.join(NODE_MODULES_DIR);
    ensure_no_reparse(&node_modules)?;
    // scope 父目录必须独立检查，不能只查叶子包目录：漏掉它等于放过一层重解析点。
    ensure_no_reparse(&node_modules.join(layout.scope))?;
    let package = node_modules.join(layout.package);
    ensure_no_reparse(&package)?;
    let manifest = package.join("package.json");
    ensure_no_reparse(&manifest)?;
    let bytes = std::fs::read(&manifest).map_err(|error| {
        format!(
            "受管 {} 缺少 package.json {}: {error}",
            layout.package,
            manifest.display()
        )
    })?;
    let document: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("包描述损坏 {}: {error}", manifest.display()))?;
    if document.get("name").and_then(|name| name.as_str()) != Some(layout.package) {
        return Err(format!(
            "受管 {} 包名不合法 {}",
            layout.package,
            manifest.display()
        ));
    }
    Ok(package)
}

/// 激活槽位里的可执行文件；没有激活槽、或该成员没有独立可执行文件时返回 `None`。
/// 激活槽指向坏了就报错（与 `dsh_path` 一样，不偷偷回退到外部安装）。
pub(crate) fn active_executable(data: &Path, layout: &Layout) -> Result<Option<PathBuf>, String> {
    let Some(relative) = layout.executable else {
        return Ok(None);
    };
    let Some(active) = read_state_for(data, layout)?.active else {
        return Ok(None);
    };
    let path = slot_root_for(data, layout, &active)?.join(relative);
    ensure_no_reparse(&path)?;
    if !path.is_file() {
        return Err(format!(
            "受管 {} 槽位缺少可执行文件 {}",
            layout.package,
            path.display()
        ));
    }
    Ok(Some(path))
}

/// 某个槽位里的包目录：`<slot>/node_modules/<layout.package>`。
pub(crate) fn slot_package_for(data: &Path, layout: &Layout, id: &str) -> Result<PathBuf, String> {
    Ok(slot_root_for(data, layout, id)?
        .join(NODE_MODULES_DIR)
        .join(layout.package))
}

/// 写入 `<root>/state.json`（按布局）：只有修订号严格递增、磁盘当前修订与 `expected_revision`
/// 一致、`active` 槽位包真实可用时才提交。提交走同目录临时文件 + `rename`，任何失败都不改动原状态。
pub(crate) fn write_state_for(
    data: &Path,
    layout: &Layout,
    expected_revision: u64,
    next: &InstallState,
) -> Result<(), String> {
    let expected_next = expected_revision
        .checked_add(1)
        .ok_or_else(|| format!("修订号溢出: {expected_revision}"))?;
    if next.revision != expected_next {
        return Err(format!(
            "修订号必须连续: 期望 {expected_next}，实际 {}",
            next.revision
        ));
    }
    parse_slot_id(next.active.as_deref())?;
    parse_slot_id(next.previous.as_deref())?;
    // 坏状态一律报错，不当默认值、不自动修复用户数据。
    let current = read_state_for(data, layout)?;
    if current.revision != expected_revision {
        return Err(format!(
            "状态已被推进，拒绝过期方案: 期望 {expected_revision}，当前 {}",
            current.revision
        ));
    }
    if let Some(active) = next.active.as_deref() {
        // 绝不写入指向缺失/被替换包的坏指针。
        verify_active_package(data, layout, active)?;
    }
    // root 及其 managed 父目录先做重解析点校验，再递归创建。
    let root = root_for(data, layout)?;
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("无法创建受管根目录 {}: {error}", root.display()))?;
    let path = root.join(STATE_FILE);
    ensure_no_reparse(&path)?;
    let bytes = serde_json::to_vec(next).map_err(|error| format!("状态序列化失败: {error}"))?;
    let temp = root.join(format!("state.{}.tmp", Uuid::new_v4()));
    write_temp(&temp, &bytes)?;
    std::fs::rename(&temp, &path).map_err(|error| {
        // 提交失败只清理本次临时文件；原 state 未被触碰。
        let _ = std::fs::remove_file(&temp);
        format!("无法提交状态 {}: {error}", path.display())
    })
}

/// 写入本次临时文件并落盘；失败只删除这个确切的临时路径，不做递归或外部删除。
fn write_temp(temp: &Path, bytes: &[u8]) -> Result<(), String> {
    let written = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(temp);
        return Err(format!("无法写入状态临时文件 {}: {error}", temp.display()));
    }
    Ok(())
}

/// `active` 槽位必须指向真实可用的受管包：各级路径无重解析点，manifest 包名正确且有版本串。
fn verify_active_package(data: &Path, layout: &Layout, id: &str) -> Result<(), String> {
    let node_modules = slot_root_for(data, layout, id)?.join(NODE_MODULES_DIR);
    ensure_no_reparse(&node_modules)?;
    ensure_no_reparse(&node_modules.join(layout.scope))?;
    let package = node_modules.join(layout.package);
    ensure_no_reparse(&package)?;
    let manifest = package.join("package.json");
    ensure_no_reparse(&manifest)?;
    let bytes = std::fs::read(&manifest).map_err(|error| {
        format!(
            "受管 {} 缺少 package.json {}: {error}",
            layout.package,
            manifest.display()
        )
    })?;
    let document: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("包描述损坏 {}: {error}", manifest.display()))?;
    if document.get("name").and_then(|name| name.as_str()) != Some(layout.package) {
        return Err(format!(
            "受管 {} 包名不合法 {}",
            layout.package,
            manifest.display()
        ));
    }
    if document
        .get("version")
        .and_then(|version| version.as_str())
        .is_none()
    {
        return Err(format!(
            "受管 {} 缺少版本串 {}",
            layout.package,
            manifest.display()
        ));
    }
    Ok(())
}

/// 校验槽位标识：必须是 UUID 的规范连字符小写形式，不接受路径或其它写法。
fn parse_slot_id(id: Option<&str>) -> Result<Option<String>, String> {
    let Some(id) = id else { return Ok(None) };
    let parsed = Uuid::parse_str(id).map_err(|_| format!("非法槽位标识: {id}"))?;
    let canonical = parsed.to_string();
    if canonical != id {
        return Err(format!("槽位标识必须是规范 UUID: {id}"));
    }
    Ok(Some(canonical))
}

/// 路径已存在且为符号链接/重解析点时返回错误；不存在则视为尚未创建。
fn ensure_no_reparse(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("无法检查路径 {}: {error}", path.display())),
    };
    if is_reparse(&metadata) {
        return Err(format!("受管路径是符号链接或重解析点: {}", path.display()));
    }
    Ok(())
}

/// Windows 用文件属性判断重解析点，其它平台用符号链接类型。
fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 隔离测试数据目录：绝对路径、每次唯一；Drop 时只删除本目录。
    struct TestData(PathBuf);

    impl TestData {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("dsh-state-{}", Uuid::new_v4()));
            fs::create_dir_all(&dir).expect("创建测试数据目录");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestData {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .expect("列出目录")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn write_state_rejects_stale_revision_and_broken_state() {
        let data = TestData::new();
        let root = root_for(data.path(), &Layout::DSH).expect("解析受管根目录");
        fs::create_dir_all(&root).expect("创建受管根目录");
        let state_path = root.join(STATE_FILE);
        let initial = br#"{"revision":3,"active":null,"previous":null}"#.to_vec();
        fs::write(&state_path, &initial).expect("写入初始状态");

        // next.revision 必须是 expected_revision + 1。
        let jump = InstallState {
            revision: 5,
            active: None,
            previous: None,
        };
        assert!(write_state_for(data.path(), &Layout::DSH, 3, &jump).is_err());
        // 磁盘已是 revision 3，expected 2 属过期方案。
        let stale = InstallState {
            revision: 3,
            active: None,
            previous: None,
        };
        assert!(write_state_for(data.path(), &Layout::DSH, 2, &stale).is_err());
        assert_eq!(fs::read(&state_path).expect("读回状态"), initial);

        // 坏 JSON 不得当默认值，也不得被自动修复。
        let broken = b"{not json".to_vec();
        fs::write(&state_path, &broken).expect("写入坏状态");
        assert!(read_state(data.path()).is_err());
        let next = InstallState {
            revision: 1,
            active: None,
            previous: None,
        };
        assert!(write_state_for(data.path(), &Layout::DSH, 0, &next).is_err());
        assert_eq!(fs::read(&state_path).expect("读回状态"), broken);
        assert!(temp_files(&root).is_empty());
    }

    #[test]
    fn write_state_persists_active_none_state() {
        let data = TestData::new();
        let next = InstallState {
            revision: 1,
            active: None,
            previous: None,
        };
        write_state_for(data.path(), &Layout::DSH, 0, &next).expect("首次写入状态");
        let read = read_state(data.path()).expect("读回状态");
        assert_eq!(read.revision, 1);
        assert_eq!(read.active, None);
        assert_eq!(read.previous, None);
        let root = root_for(data.path(), &Layout::DSH).expect("解析受管根目录");
        assert!(root.join(STATE_FILE).is_file());
        assert!(temp_files(&root).is_empty());
    }

    /// npm 无法解析 `\\?\` 形式的路径，受管根目录必须是普通盘符路径。
    #[test]
    fn managed_root_has_no_verbatim_prefix() {
        let data = TestData::new();
        let root = root_for(data.path(), &Layout::DSH).expect("解析受管根目录");
        let text = root.to_string_lossy();
        assert!(
            !text.starts_with(r"\\?\"),
            "受管根目录带 verbatim 前缀: {text}"
        );
        assert!(!text.starts_with(r"\\.\"), "受管根目录带设备前缀: {text}");
    }

    /// 布局是按成员分开的：Codex 的受管根目录与 DSH 不同，映射函数也只认这两个成员。
    #[test]
    fn layouts_keep_members_in_separate_managed_dirs() {
        let data = TestData::new();
        let dsh = root_for(data.path(), &Layout::DSH).expect("DSH 受管根目录");
        let codex = root_for(data.path(), &Layout::CODEX).expect("Codex 受管根目录");
        assert_ne!(dsh, codex);
        assert!(dsh.ends_with("managed-services/dsh-win"));
        assert!(codex.ends_with("managed-services/codex-win"));
        assert_eq!(layout_for("dsh-win"), Some(Layout::DSH));
        assert_eq!(layout_for("codex-win"), Some(Layout::CODEX));
        assert_eq!(layout_for("hermes-win"), None);
        // Codex 的 exe 在平台子包里；DSH 没有独立 exe（走 node 跑包）。
        assert!(Layout::CODEX.executable.is_some());
        assert_eq!(Layout::DSH.executable, None);
    }

    /// 两个成员的 state.json 互不干扰：给 DSH 写一份，Codex 侧读出来仍是默认值。
    #[test]
    fn states_are_per_layout() {
        let data = TestData::new();
        let next = InstallState {
            revision: 1,
            active: None,
            previous: None,
        };
        write_state_for(data.path(), &Layout::DSH, 0, &next).expect("写 DSH 状态");
        assert_eq!(
            read_state_for(data.path(), &Layout::DSH)
                .expect("读 DSH 状态")
                .revision,
            1
        );
        let codex = read_state_for(data.path(), &Layout::CODEX).expect("读 Codex 状态");
        assert_eq!(codex.revision, 0);
        assert_eq!(codex.active, None);
    }

    /// 外部安装目录由 PATH 上的 CLI 壳反推：npm -g 前缀布局与自带 node_modules 的
    /// 独立安装都要认，找不到就返回 None（而不是某个人的安装路径）。
    #[test]
    fn installation_is_derived_from_the_cli_on_path() {
        let base = std::env::temp_dir().join(format!("agenthub-layout-{}", Uuid::new_v4()));
        let prefix = base.join("prefix");
        let standalone = base.join("standalone");
        fs::create_dir_all(prefix.join(NODE_MODULES_DIR).join(Layout::CODEX.package))
            .expect("建 npm -g 布局");
        fs::create_dir_all(
            standalone
                .join("bin")
                .join(NODE_MODULES_DIR)
                .join(Layout::DSH.package),
        )
        .expect("建独立安装布局");
        fs::write(prefix.join("codex.cmd"), "").expect("写 codex 壳");
        fs::write(standalone.join("bin").join("dsh.cmd"), "").expect("写 dsh 壳");

        assert_eq!(
            installation_in(std::slice::from_ref(&prefix), &Layout::CODEX),
            Some(prefix.join(NODE_MODULES_DIR).join(Layout::CODEX.package))
        );
        assert_eq!(
            installation_in(&[standalone.join("bin")], &Layout::DSH),
            Some(
                standalone
                    .join("bin")
                    .join(NODE_MODULES_DIR)
                    .join(Layout::DSH.package)
            )
        );
        assert_eq!(
            installation_in(&[base.join("nowhere")], &Layout::CODEX),
            None
        );
        // 只有壳、没有包目录时不算数。
        let shell_only = base.join("shellonly");
        fs::create_dir_all(&shell_only).expect("建空布局");
        fs::write(shell_only.join("codex.cmd"), "").expect("写壳");
        assert_eq!(installation_in(&[shell_only], &Layout::CODEX), None);

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn strip_verbatim_prefix_only_rewrites_drive_paths() {
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"\\?\D:\data\managed")),
            PathBuf::from(r"D:\data\managed")
        );
        let unc = PathBuf::from(r"\\?\UNC\server\share");
        assert_eq!(strip_verbatim_prefix(unc.clone()), unc);
    }
}
