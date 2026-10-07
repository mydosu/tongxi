"""本机 agent harness 的安装位置：环境变量优先，其次从 PATH 上的 CLI 壳反推。

npm -g 全局前缀（`<prefix>/codex.cmd`）与自带 `node_modules` 的独立安装
（`<dir>/bin/dsh.cmd`）两种布局都成立，所以这里没有写死任何人的安装路径。
"""
import os
import shutil
from pathlib import Path


def _installation(cli, scope, package, env):
    explicit = os.environ.get(env)
    if explicit:
        return explicit
    shell = shutil.which(cli + ".cmd") or shutil.which(cli)
    if not shell:
        return ""
    directory = Path(shell).parent / "node_modules" / scope / package
    return str(directory) if directory.is_dir() else ""


def dsh_installation():
    return _installation("dsh", "@deepseek-ai", "dsh", "AGENT_HUB_DSH_INSTALLATION")


def codex_installation():
    return _installation("codex", "@openai", "codex", "AGENT_HUB_CODEX_INSTALLATION")


def codex_executable():
    """原生 codex.exe；npm -g 布局里平台包嵌在主包内部（与后端 codex.rs 的候选一致）。"""
    explicit = os.environ.get("AGENT_HUB_CODEX_EXE")
    if explicit:
        return explicit
    found = shutil.which("codex.exe")
    if found:
        return found
    installation = codex_installation()
    if not installation:
        return ""
    nested = Path(installation) / (
        "node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe"
    )
    return str(nested) if nested.is_file() else ""


if __name__ == "__main__":  # 自查：本机两种布局都要能反推出来
    print("dsh   :", dsh_installation() or "(未找到)")
    print("codex :", codex_installation() or "(未找到)")
    print("exe   :", codex_executable() or "(未找到)")
