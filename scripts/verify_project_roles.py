"""角色可配的两段式项目协作：隔离 fixture 上的真实桌面验收。

场景 a-e 覆盖角色、UI、并行与依赖；f 覆盖一次修复；g 覆盖合并冲突；h 覆盖非 Git 拒绝；i 由 Codex 承担全部角色。
除 h 外场景使用真实原生 agent，CDP 只观察和调用应用 IPC，不触碰用户项目。
"""
import argparse
import hashlib
import importlib.util
import json
import subprocess
import sys
import time
import uuid
from pathlib import Path

from playwright.sync_api import sync_playwright

import smoke_desktop as desktop
import verify_stage3 as prior
import verify_stage6 as groups

checks = []
errors = []
REPORT = None
# 各场的并行采样结果（同时 running 的实现任务峰值），随报告一起留档。
MAX_LIVE = {}

GOOD = (
    'from calc import square\n'
    'assert square(3)==9, "square(3) must be 9"\n'
    'assert square(-4)==16, "square(-4) must be 16"\n'
    'assert square(0)==0, "square(0) must be 0"\n'
    'print("3 functional assertions passed")\n'
)
ASK = (
    '把 calc.py 里的 square(value) 改成返回 value 的平方（支持负数、零），只修改 calc.py，'
    '不要修改 check.py 或其他文件。完成后由验收角色对照本条目标检查源码。只分一项任务。'
)
# 依赖链用：第二项必须建在第一项的成果之上，否则它自己的固定检查（断言 square 是平方）跑不过。
ASK_DEPS = (
    '两项任务，第二项必须依赖第一项：（1）把 calc.py 的 square(value) 改成返回平方（支持负数、零）；'
    '（2）新增 calcer.py，里面写 describe_square(value)，调用 calc.square 并返回一句中文说明。'
    '第二项的 depends_on 必须是第一项。只授权 calc.py 与 calcer.py，不能修改 check.py，只分这两项。'
)
# C 场：两项互不依赖的任务同时跑；固定检查写成两个文件都能导入即通过，
# 这样每个任务自己的 worktree 也能单独过检查，真正的改动正确性由主仓库根断言。
PARALLEL_CHECK = (
    'import calc, cube\n'
    'assert callable(calc.square), "calc.square must exist"\n'
    'assert callable(cube.cube), "cube.cube must exist"\n'
    'assert isinstance(calc.square(2), int), "square returns int"\n'
    'assert isinstance(cube.cube(2), int), "cube returns int"\n'
    'print("both modules importable")\n'
)
ASK_PARALLEL = (
    "这是两项互不依赖的改动，必须拆成两项任务、depends_on 都为空，每项只授权一个文件："
    "第一项只改 calc.py，把 square(value) 改成返回 value 的平方（支持负数、零）；"
    "第二项只改 cube.py，把 cube(value) 改成返回 value 的立方（支持负数、零）。"
    "两项都不能修改 check.py，也不要合成一项。"
    "严格只输出一个 ```json 代码块，块内是方案 JSON，代码块外不要写任何解释文字。"
)


def check(name, condition=True):
    if not condition:
        raise AssertionError(name)
    checks.append(name)
    print("PASS " + name, flush=True)


def workflow(page, room):
    rows = desktop.ipc(page, "get_conversation", id=room)["workflows"]
    return rows[0]


def observe(page, room, predicate, timeout=900):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        job = workflow(page, room)
        if predicate(job):
            return job
        if job["status"] in ("failed", "interrupted"):
            raise AssertionError("项目提前终止：" + job["status"] + " " + str(job.get("error")))
        page.wait_for_timeout(120)
    raise AssertionError("等待项目状态超时")


def venue(page, title, root, roles, members=None):
    root.mkdir(parents=True)
    (root / "calc.py").write_text("def square(value):\n    return value\n", encoding="utf-8")
    (root / "check.py").write_text(GOOD, encoding="utf-8")
    git_repo(root)
    project = desktop.ipc(
        page,
        "register_project",
        name=title,
        root=str(root),
    )
    room_members = members or ["hermes-win", "codex-win", "dsh-win"]
    room = groups.group(page, title, room_members)
    desktop.ipc(page, "bind_project", conversationId=room, projectId=project["id"])
    for agent, effort in (("hermes-win", "none"), ("codex-win", "low"), ("dsh-win", "off")):
        if agent not in room_members:
            continue
        desktop.ipc(page, "set_session_settings", id=room, agentId=agent, model="gpt-6-luna" if agent == "codex-win" else None, reasoningEffort=effort)
    return room, project, root / "calc.py", root / "check.py"


def start(page, room, roles, ask=ASK):
    return desktop.ipc(
        page, "plan_project", room=room, message=ask, content=None, roles=roles
    )


def choice(agent, effort):
    return {"agent": agent, "model": "gpt-6-luna" if agent == "codex-win" else None, "effort": effort}


def stage_agents(job, stage):
    return [a["agent_id"] for a in job["attempts"] if a["stage"] == stage]


def one_round(page, root, title, roles, expect_plan, expect_implement):
    # 规划角色是真实模型，偶尔会吐出不带代码块的方案导致落库被拒；重试几次，换新房间重新规划，
    # 只隔离模型抖动，不放宽任何断言。
    planned = None
    for attempt in range(3):
        venue_dir = root / ("try%d" % attempt)
        room, _, calc, script = venue(page, title + ("·" + str(attempt) if attempt else ""), venue_dir, roles)
        before = hashlib.md5(script.read_bytes()).hexdigest()
        started = start(page, room, roles)
        try:
            planned = observe(page, room, lambda j: any(a["stage"] == "plan" and a["status"] == "completed" for a in j["attempts"]))
        except AssertionError:
            continue
        if planned["status"] == "planning":
            break
    if planned is None or planned["status"] != "planning":
        raise AssertionError(title + "：规划未能产出可落库的方案")

    # 规划这一段跑完必须停下等你确认：状态仍是 planning、方案已落库、没有任何执行 attempt、租约还握着。
    check(title + "：规划产出后停在待确认", planned["status"] == "planning")
    check(title + "：方案已落库且有任务", bool(planned["plan"]) and bool(planned["tasks"]))
    check(title + "：确认前不产生任何执行 attempt", not any(a["stage"] in ("implement", "verify", "review") for a in planned["attempts"]))
    check(title + "：待确认期间仍持有写入租约", bool(desktop.ipc(page, "project_status")))
    check(title + "：规划由所选角色承担", stage_agents(planned, "plan") == [expect_plan])
    check(title + "：角色配置已落库", json.loads(planned["roles"])["implement"]["agent"] == expect_implement)

    # 逐任务确认：给它一个明确的强度覆盖，确认后开跑。
    desktop.ipc(page, "confirm_project", workflowId=started["id"], tasks=[{"position": 0, "model": None, "effort": None}])
    running = observe(page, room, lambda j: j["status"] == "running")
    check(title + "：确认后进入执行", running["status"] == "running")
    done = observe(page, room, lambda j: j["status"] in ("completed", "failed"))
    check(title + "：整场跑到终态且成功", done["status"] == "completed")
    check(title + "：执行由所选角色承担", stage_agents(done, "implement") == [expect_implement])
    check(title + "：固定检查由验收角色挂名", stage_agents(done, "verify") == [json.loads(done["roles"])["review"]["agent"]])
    check(title + "：验收由所选角色承担", stage_agents(done, "review") == [json.loads(done["roles"])["review"]["agent"]])
    check(title + "：任务与检查都通过", done["tasks"][0]["status"] == "completed" and done["attempts"][-1]["stage"] == "review")
    check(title + "：源码真的被改动", "return value * value" in calc.read_text(encoding="utf-8"))
    check(title + "：固定验收脚本未被篡改", hashlib.md5(script.read_bytes()).hexdigest() == before)
    check(title + "：结束后释放写入租约", not desktop.ipc(page, "project_status"))
    return done


def git_repo(root):
    """把 fixture 目录初始化成一个干净的 git 仓库，工作树/合并才有基线。"""
    def call(*args):
        subprocess.run(["git", *args], cwd=str(root), check=True, capture_output=True)
    call("init", "-q")
    call("config", "user.email", "hub@test.local")
    call("config", "user.name", "hub-test")
    call("config", "commit.gpgsign", "false")
    call("config", "core.autocrlf", "false")
    call("add", "-A")
    call("commit", "-q", "-m", "init")


def load_module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    # 被加载的文件可能 import 同目录的兄弟模块（如 calcer 里 import calc），临时把它的目录放进搜索路径。
    sys.path.insert(0, str(path.parent))
    try:
        spec.loader.exec_module(module)
    finally:
        sys.path.remove(str(path.parent))
    return module


def venue_parallel(page, title, root):
    root.mkdir(parents=True)
    (root / "calc.py").write_text("def square(value):\n    return value\n", encoding="utf-8")
    (root / "cube.py").write_text("def cube(value):\n    return value\n", encoding="utf-8")
    (root / "check.py").write_text(PARALLEL_CHECK, encoding="utf-8")
    git_repo(root)
    project = desktop.ipc(
        page,
        "register_project",
        name=title,
        root=str(root),
    )
    room = groups.group(page, title, ["hermes-win", "codex-win", "dsh-win"])
    desktop.ipc(page, "bind_project", conversationId=room, projectId=project["id"])
    for agent, effort in (("hermes-win", "none"), ("codex-win", "low"), ("dsh-win", "off")):
        desktop.ipc(page, "set_session_settings", id=room, agentId=agent, model=None, reasoningEffort=effort)
    return room, project


def sample_execution(page, room, timeout=900):
    """执行期间高频采样：数同一时刻有几个任务的实现 attempt 在跑。

    并行要这么证（只看「两个工作树都成功」证明不了并发）；有依赖的场次反过来用同一个数
    证明「没有同时跑」。
    """
    peak = 0
    samples = 0
    done = None
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        job = workflow(page, room)
        live = {
            a["task_id"]
            for a in job["attempts"]
            if a["stage"] == "implement" and a["status"] == "running" and a.get("task_id")
        }
        samples += 1
        peak = max(peak, len(live))
        if job["status"] in ("completed", "failed", "interrupted"):
            done = job
            break
        page.wait_for_timeout(80)
    return peak, samples, done


def parallel_round(page, root, title, roles, expect_plan, expect_implement):
    """两项互不依赖的任务：各自独立工作树并行实现，按序合并回主分支后再统一验收。

    规划角色是真实模型，偶尔会吐出不带代码块的方案导致落库被拒；这一场重试几次，
    换新房间重新规划，隔离模型抖动而非放宽断言。
    """
    planned = None
    venue = None
    for attempt in range(3):
        venue = root / ("try%d" % attempt)
        room, _ = venue_parallel(page, title + ("·" + str(attempt) if attempt else ""), venue)
        started = start(page, room, roles, ASK_PARALLEL)
        try:
            planned = observe(page, room, lambda j: any(a["stage"] == "plan" and a["status"] == "completed" for a in j["attempts"]))
        except AssertionError:
            continue
        if planned["status"] == "planning":
            break
    if planned is None or planned["status"] != "planning":
        raise AssertionError(title + "：规划未能产出可落库的方案")

    check(title + "：规划产出后停在待确认", planned["status"] == "planning")
    check(title + "：方案拆成两项任务", len(planned["tasks"]) == 2)
    check(title + "：规划由所选角色承担", stage_agents(planned, "plan") == [expect_plan])
    check(title + "：角色配置已落库", json.loads(planned["roles"])["implement"]["agent"] == expect_implement)

    desktop.ipc(
        page,
        "confirm_project",
        workflowId=started["id"],
        tasks=[{"position": 0, "model": None, "effort": None}, {"position": 1, "model": None, "effort": None}],
    )
    running = observe(page, room, lambda j: j["status"] == "running")
    check(title + "：确认后进入执行", running["status"] == "running")
    peak_live, samples, done = sample_execution(page, room)
    MAX_LIVE[title] = {"samples": samples, "peak_running_implement_tasks": peak_live}
    if done is None:
        raise AssertionError(title + "：等待执行终态超时")
    check(title + "：两个任务的实现确实同时在跑（采样 " + str(samples) + " 次，同时 running 峰值 " + str(peak_live) + "）", peak_live >= 2)
    check(title + "：整场跑到终态且成功", done["status"] == "completed")

    tasks = done["tasks"]
    check(title + "：两项任务都完成", len(tasks) == 2 and all(t["status"] == "completed" for t in tasks))
    check(title + "：两项任务有非空且互不相同的工作树", all(t["worktree"] for t in tasks) and tasks[0]["worktree"] != tasks[1]["worktree"])
    check(title + "：两项任务都有分支名", all(t["branch"] for t in tasks))
    check(title + "：执行由所选角色承担", stage_agents(done, "implement") == [expect_implement, expect_implement])

    calc = load_module(venue / "calc.py", "root_calc")
    cube = load_module(venue / "cube.py", "root_cube")
    check(title + "：主仓库根能看到 calc.square 的改动", calc.square(3) == 9 and calc.square(-4) == 16 and calc.square(0) == 0)
    check(title + "：主仓库根能看到 cube.cube 的改动", cube.cube(2) == 8 and cube.cube(-3) == -27 and cube.cube(0) == 0)

    verify = [a for a in done["attempts"] if a["stage"] == "verify"][-1]
    check(title + "：主仓库根的固定检查通过", verify["status"] == "completed" and bool(verify["checks"]) and all(c["exit_code"] == 0 for c in verify["checks"]))
    check(title + "：结束后释放写入租约", not desktop.ipc(page, "project_status"))
    return done


def ui_round(page, root, title, roles, expect_plan, expect_implement):
    """绑定项目后的同一群聊入口：一条消息自动经过协调、实现与验收，不再弹执行模式或确认方案。"""
    venue_dir = root / "ui"
    room, _, calc, _ = venue(page, title, venue_dir, roles, members=["codex-win", "dsh-win"])
    desktop.choose(page, room)
    request = '请由 DSH 把 calc.py 里的 square(value) 改成返回平方（支持负数、零），只修改 calc.py，不改其他文件。'
    page.fill("#message-input", request)
    check(title + "：项目群没有独立执行按钮", page.locator("#send-project").count() == 0)
    check(title + "：同一个群聊发送入口启用", page.locator("#send-discussion").is_enabled())
    page.click("#send-discussion")
    done = observe(page, room, lambda j: j["status"] in ("completed", "failed"))
    check(title + "：群聊消息自动跑完协作", done["status"] == "completed")
    check(title + "：无需角色选择弹窗或方案确认", page.locator("#role-form").count() == 0 and page.locator("[data-confirm-project]").count() == 0)
    saved = json.loads(done["roles"])
    check(title + "：阶段成员从群聊成员与当前模型设置自动继承", saved["plan"]["agent"] == "codex-win" and saved["plan"]["model"] == "gpt-6-luna" and saved["implement"]["agent"] == "dsh-win" and saved["review"]["agent"] == "codex-win" and saved["review"]["model"] == "gpt-6-luna")
    check(title + "：本次改动出现在同一群聊工作区", any(change["path"] == "calc.py" for change in done["changes"]))
    calc_module = load_module(venue_dir / "calc.py", "unified_ui_calc")
    check(title + "：最终源码满足本轮明确目标", calc_module.square(3) == 9 and calc_module.square(-4) == 16 and calc_module.square(0) == 0)
    marks = page.eval_on_selector_all(".workflow-card li small", "els => els.map(e => e.textContent)")
    check(title + "：协作结果仍显示实际工作树和分支", any("工作树" in text and "分支" in text for text in marks))
    check(title + "：结束后释放写入租约", not desktop.ipc(page, "project_status"))
    return done


def deps_round(page, root, title, roles, expect_plan, expect_implement):
    """有依赖的两项任务：第二轮必须等第一轮完成，而且它的工作树要能看到第一轮的成果。

    判别力就在「第二项自己的固定检查」上：check.py 断言 square 是平方，而 square 是第一项改的。
    第二项的工作树若不是以第一项的分支为基线，它连自己的检查都跑不过（这条曾经真的不成立）。
    """
    venue_dir = root / "deps"
    room, _, calc, script = venue(page, title, venue_dir, roles)
    started = start(page, room, roles, ASK_DEPS)
    planned = observe(page, room, lambda j: any(a["stage"] == "plan" and a["status"] == "completed" for a in j["attempts"]))
    check(title + "：规划产出后停在待确认", planned["status"] == "planning")
    check(title + "：方案是两项任务且第二项声明依赖第一项", len(planned["tasks"]) == 2 and planned["tasks"][1]["depends_on"] == [0])
    desktop.ipc(
        page,
        "confirm_project",
        workflowId=started["id"],
        tasks=[{"position": 0, "model": None, "effort": None}, {"position": 1, "model": None, "effort": None}],
    )
    running = observe(page, room, lambda j: j["status"] == "running")
    check(title + "：确认后进入执行", running["status"] == "running")
    peak, samples, done = sample_execution(page, room)
    MAX_LIVE[title] = {"samples": samples, "peak_running_implement_tasks": peak}
    if done is None:
        raise AssertionError(title + "：等待执行终态超时")
    check(title + "：有依赖的两项没有同时跑（采样 " + str(samples) + " 次，同时 running 峰值 " + str(peak) + "）", peak <= 1)
    check(title + "：整场跑到终态且成功", done["status"] == "completed")
    check(title + "：两项任务都完成（第二项自己的检查也过了）", len(done["tasks"]) == 2 and all(t["status"] == "completed" for t in done["tasks"]))
    check(title + "：两项任务仍各有独立工作树", all(t["worktree"] for t in done["tasks"]) and done["tasks"][0]["worktree"] != done["tasks"][1]["worktree"])
    calcer = load_module(venue_dir / "calcer.py", "root_calcer")
    check(title + "：主仓库根能看到第二项新增的文件", "平方" in calcer.describe_square(3))
    check(title + "：结束后释放写入租约", not desktop.ipc(page, "project_status"))
    return done


def repair_round(page, root, title):
    """真实触发一次主仓库固定检查失败，确认框架只修复一次并重新验收。"""
    venue_dir = root / "repair"
    venue_dir.mkdir(parents=True)
    source = venue_dir / "calc.py"
    source.write_text("def square(value):\n    return value\n", encoding="utf-8")
    marker = root / "root-check-failed-once.marker"
    check_script = venue_dir / "check.py"
    check_script.write_text(
        "from pathlib import Path\n"
        "from calc import square\n"
        "assert square(3) == 9\n"
        "assert square(-4) == 16\n"
        "assert square(0) == 0\n"
        "if Path('.git').is_dir():\n"
        f"    marker = Path({str(marker)!r})\n"
        "    if not marker.exists():\n"
        "        marker.write_text('transient root check failure', encoding='utf-8')\n"
        "        raise SystemExit(1)\n",
        encoding="utf-8",
    )
    git_repo(venue_dir)
    project = desktop.ipc(
        page,
        "register_project",
        name=title,
        root=str(venue_dir),
    )
    room = groups.group(page, title, ["hermes-win", "codex-win", "dsh-win"])
    desktop.ipc(page, "bind_project", conversationId=room, projectId=project["id"])
    roles = {
        "plan": choice("dsh-win", "off"),
        "implement": choice("dsh-win", "off"),
        "review": choice("hermes-win", "none"),
    }
    started = start(page, room, roles)
    planned = observe(page, room, lambda j: any(a["stage"] == "plan" and a["status"] == "completed" for a in j["attempts"]))
    check(title + "：方案只授权 calc.py", planned["status"] == "planning" and planned["tasks"][0]["files"] == ["calc.py"])
    desktop.ipc(page, "confirm_project", workflowId=started["id"], tasks=[{"position": 0, "model": None, "effort": None}])
    peak, samples, done = sample_execution(page, room)
    if done is None:
        raise AssertionError(title + "：等待执行终态超时")
    attempts = done["attempts"]
    verifies = [a for a in attempts if a["stage"] == "verify"]
    repairs = [a for a in attempts if a["stage"] == "repair"]
    check(title + "：首次主仓库检查按预期失败", len(verifies) == 2 and verifies[0]["status"] == "failed")
    check(title + "：失败后只启动一次所选执行角色修复", len(repairs) == 1 and repairs[0]["agent_id"] == "dsh-win")
    check(title + "：修复后固定检查再次通过", verifies[-1]["status"] == "completed" and all(c["exit_code"] == 0 for c in verifies[-1]["checks"]))
    check(title + "：一次修复与复验后整场成功", done["status"] == "completed" and stage_agents(done, "review") == ["hermes-win"])
    check(title + "：源码符合需求且检查触发标记在项目外", "return value * value" in source.read_text(encoding="utf-8") and marker.is_file())
    check(title + "：结束后释放写入租约", not desktop.ipc(page, "project_status"))
    return done


def conflict_round(page, root, title):
    """两个独立分支改同一行，确认合并冲突会失败并保留主仓库冲突现场。"""
    venue_dir = root / "conflict"
    venue_dir.mkdir(parents=True)
    source = venue_dir / "calc.py"
    source.write_text("VALUE = 0\n", encoding="utf-8")
    (venue_dir / "check.py").write_text(
        "from calc import VALUE\n"
        "assert VALUE in (1, 2), 'task branch must contain its assigned value'\n",
        encoding="utf-8",
    )
    git_repo(venue_dir)
    project = desktop.ipc(
        page,
        "register_project",
        name=title,
        root=str(venue_dir),
    )
    room = groups.group(page, title, ["hermes-win", "codex-win", "dsh-win"])
    desktop.ipc(page, "bind_project", conversationId=room, projectId=project["id"])
    roles = {
        "plan": choice("dsh-win", "off"),
        "implement": choice("dsh-win", "off"),
        "review": choice("hermes-win", "none"),
    }
    ask = (
        "必须规划两项互不依赖的任务，depends_on 都为空，且两项都只授权 calc.py："
        "任务一将 VALUE 从 0 改成 1；任务二将 VALUE 从 0 改成 2。"
        "两项各自的固定检查都应通过，不修改 check.py。严格输出单个 JSON 代码块。"
    )
    started = start(page, room, roles, ask)
    planned = observe(page, room, lambda j: any(a["stage"] == "plan" and a["status"] == "completed" for a in j["attempts"]))
    check(title + "：计划拆成两个相互独立且共享授权文件的任务", planned["status"] == "planning" and len(planned["tasks"]) == 2 and planned["tasks"][0]["depends_on"] == [] and planned["tasks"][1]["depends_on"] == [] and all(t["files"] == ["calc.py"] for t in planned["tasks"]))
    desktop.ipc(page, "confirm_project", workflowId=started["id"], tasks=[{"position": 0, "model": None, "effort": None}, {"position": 1, "model": None, "effort": None}])
    peak, samples, done = sample_execution(page, room)
    if done is None:
        raise AssertionError(title + "：等待执行终态超时")
    check(title + "：两项任务在各自工作树中都通过检查", len(done["tasks"]) == 2 and all(t["status"] == "completed" and t["worktree"] for t in done["tasks"]))
    check(title + "：合并冲突使工作流失败而不自动回退", done["status"] == "failed" and "冲突" in (done.get("error") or ""))
    content = source.read_text(encoding="utf-8")
    check(title + "：主仓库保留未解决的冲突标记", all(token in content for token in ("<<<<<<<", "=======", ">>>>>>>")))
    branches = subprocess.run(["git", "branch", "--list"], cwd=str(venue_dir), check=True, capture_output=True, text=True).stdout
    check(title + "：Git 保留 MERGE_HEAD 与双方任务分支", (venue_dir / ".git" / "MERGE_HEAD").is_file() and all(t["branch"] and t["branch"] in branches for t in done["tasks"]))
    check(title + "：冲突后仍释放写入租约", not desktop.ipc(page, "project_status"))
    return done


def non_git_bind_rejection(page, root, title):
    """Binding an ordinary directory must fail before a project conversation is created."""
    venue_dir = root / "not-a-repository"
    venue_dir.mkdir(parents=True)
    (venue_dir / "calc.py").write_text("def square(value):\n    return value\n", encoding="utf-8")
    project = desktop.ipc(
        page,
        "register_project",
        name=title,
        root=str(venue_dir),
    )
    room = groups.group(page, title, ["hermes-win", "codex-win", "dsh-win"])
    rejected = False
    try:
        desktop.ipc(page, "bind_project", conversationId=room, projectId=project["id"])
    except Exception as exc:
        rejected = "git" in str(exc).lower()
    check(title + "：非 Git 目录无法绑定项目", rejected)
    detail = desktop.ipc(page, "get_conversation", id=room)
    check(title + "：拒绝绑定后会话仍未关联项目", not detail.get("project"))
    return {"project_id": project["id"], "conversation_id": room, "status": "rejected"}


def run(only=None):
    global REPORT
    desktop.DATA = desktop.ARTIFACTS / ("project-roles-" + uuid.uuid4().hex[:10])
    root = desktop.ARTIFACTS / ("project-roles-fixture-" + uuid.uuid4().hex[:10])
    report_suffix = {
        "a": "same-role",
        "b": "role-swap",
        "c": "parallel",
        "d": "ui",
        "e": "dependencies",
        "f": "repair",
        "g": "conflict",
        "h": "nongit",
        "i": "codex-all-roles",
    }.get(only, "")
    REPORT = desktop.ARTIFACTS / ("project-roles-" + report_suffix + "-verification.json" if report_suffix else "project-roles-verification.json")
    proc = None
    success = False
    evidence = []
    try:
        with sync_playwright() as playwright:
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on("pageerror", lambda error: errors.append(str(error)))
            desktop.ipc(page, "connect_hermes")
            desktop.ipc(page, "connect_codex")
            desktop.ipc(page, "connect_dsh")
            prior.wait_connection(page, "hermes")
            prior.wait_connection(page, "codex")
            prior.wait_connection(page, "dsh")

            if only in (None, "a"):
                evidence.append(
                    one_round(
                        page,
                        root / "a",
                        "规划与执行同角色",
                        {"plan": choice("dsh-win", "off"), "implement": choice("dsh-win", "off"), "review": choice("hermes-win", "none")},
                        "dsh-win",
                        "dsh-win",
                    )
                )
            if only in (None, "b"):
                evidence.append(
                    one_round(
                        page,
                        root / "b",
                        "三角色互换",
                        {"plan": choice("hermes-win", "none"), "implement": choice("dsh-win", "off"), "review": choice("dsh-win", "off")},
                        "hermes-win",
                        "dsh-win",
                    )
                )
            if only in (None, "c"):
                evidence.append(
                    parallel_round(
                        page,
                        root / "c",
                        "两任务独立工作树并行",
                        {"plan": choice("dsh-win", "off"), "implement": choice("dsh-win", "off"), "review": choice("hermes-win", "none")},
                        "dsh-win",
                        "dsh-win",
                    )
                )
            if only in (None, "d"):
                evidence.append(
                    ui_round(
                        page,
                        root,
                        "界面全程操作",
                        {"plan": choice("hermes-win", "off"), "implement": choice("dsh-win", "off"), "review": choice("dsh-win", "off")},
                        "hermes-win",
                        "dsh-win",
                    )
                )
            if only in (None, "e"):
                evidence.append(
                    deps_round(
                        page,
                        root,
                        "有依赖的两项任务",
                        {"plan": choice("dsh-win", "off"), "implement": choice("dsh-win", "off"), "review": choice("hermes-win", "none")},
                        "dsh-win",
                        "dsh-win",
                    )
                )
            if only == "f":
                evidence.append(repair_round(page, root, "固定检查失败后一次修复"))
            if only == "g":
                evidence.append(conflict_round(page, root, "并行任务合并冲突"))
            if only == "h":
                evidence.append(non_git_bind_rejection(page, root, "非 Git 项目拒绝绑定"))
            if only == "i":
                evidence.append(
                    ui_round(
                        page,
                        root,
                        "Codex 承担规划执行验收",
                        {"plan": choice("codex-win", "low"), "implement": choice("codex-win", "low"), "review": choice("codex-win", "low")},
                        "codex-win",
                        "codex-win",
                    )
                )
            check("没有 JavaScript 运行错误", not errors)
            success = True
    finally:
        if proc and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)
        REPORT.write_text(
            json.dumps(
                {
                    "success": success,
                    "passed": len(checks),
                    "checks": checks,
                    "javascript_errors": errors,
                    "test_data_directory": str(desktop.DATA),
                    "executable_sha256": hashlib.sha256(desktop.EXE.read_bytes()).hexdigest(),
                    "executable_bytes": desktop.EXE.stat().st_size,
                    "parallel_wall_clock": MAX_LIVE,
                    "evidence": evidence,
                },
                ensure_ascii=False,
                indent=2,
            ),
            encoding="utf-8",
        )


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", choices=["a", "b", "c", "d", "e", "f", "g", "h", "i"], default=None, help="只跑指定场次")
    arguments = parser.parse_args()
    run(arguments.only)
    print("ProjectRoles: " + str(len(checks)) + " passed", flush=True)
