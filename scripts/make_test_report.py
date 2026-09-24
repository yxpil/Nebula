#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
把 `cargo test --workspace` 的输出转换为独立 HTML 测试报告页面。

用法:
  # 直接运行 cargo test 并生成报告(推荐,避免 shell 包装原生 stderr)
  python scripts/make_test_report.py --run reports/test-report.html
  # 或解析已有输出
  cargo test --workspace 2>&1 | python scripts/make_test_report.py reports/test-report.html

报告内容: 平台/工具链信息、通过-失败总览卡片、每个测试目标的结果表、
失败用例的完整断言输出、编译错误块。纯标准库,无外部依赖。
"""

import datetime
import html
import os
import platform
import re
import subprocess
import sys

ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*(?:\x07|\x1b\\)")

RESULT_RE = re.compile(
    r"test result:\s*(\w+)\.\s*(\d+) passed;\s*(\d+) failed;\s*(\d+) ignored;"
    r"\s*(\d+) measured;\s*(\d+) filtered out;.*?"
    r"(?:finished in ([\d.]+)s)?"
)
RUNNING_RE = re.compile(r"Running\s+(.+?)\s+\((.+)\)")
DOCTEST_RE = re.compile(r"Doc-tests\s+(\S+)")
FAILED_TEST_RE = re.compile(r"^test\s+(\S+)\s+\.\.\.\s*FAILED")
COMPILE_ERR_RE = re.compile(r"^(error(?:\[[EW]\d+\])?:.*)$")


def parse(text):
    suites = []  # (名称, 路径, 结果dict)
    failures = []  # 块文本
    failed_names = []
    compile_errors = []
    current_suite = None
    in_failure_block = False
    block_buf = []

    for raw in text.splitlines():
        line = ANSI_RE.sub("", raw.rstrip())
        m = RUNNING_RE.search(line)
        if m:
            current_suite = {"name": m.group(1), "path": m.group(2), "result": None}
            suites.append(current_suite)
            continue
        m = DOCTEST_RE.search(line)
        if m:
            current_suite = {"name": f"doc-tests: {m.group(1)}", "path": "", "result": None}
            suites.append(current_suite)
            continue
        m = RESULT_RE.search(line)
        if m and current_suite is not None:
            current_suite["result"] = {
                "status": m.group(1),
                "passed": int(m.group(2)),
                "failed": int(m.group(3)),
                "ignored": int(m.group(4)),
                "measured": int(m.group(5)),
                "filtered": int(m.group(6)),
                "secs": m.group(7) or "-",
            }
            continue
        m = FAILED_TEST_RE.match(line.strip())
        if m:
            failed_names.append(m.group(1))
        m = COMPILE_ERR_RE.match(line.strip())
        if m:
            compile_errors.append(m.group(1))
        if line.strip() == "failures:":
            in_failure_block = True
            block_buf = []
            continue
        if in_failure_block:
            if line.startswith("test result:"):
                failures.append("\n".join(block_buf).strip())
                block_buf = []
                in_failure_block = False
            else:
                block_buf.append(raw)
    if block_buf:
        failures.append("\n".join(block_buf).strip())
    return suites, failures, failed_names, compile_errors


def summarize(suites):
    tot = dict(passed=0, failed=0, ignored=0, measured=0, filtered=0)
    missing = []
    for s in suites:
        r = s["result"]
        if r is None:
            missing.append(s["name"])
            continue
        for k in tot:
            tot[k] += r[k]
    return tot, missing


CSS = """
:root { color-scheme: light; }
* { box-sizing: border-box; }
body { margin:0; font-family: 'Segoe UI', system-ui, sans-serif; background:#f4f5fb; color:#1b1e2e; }
.hero { background:linear-gradient(120deg,#14152a,#2a1a4a); color:#fff; padding:38px 44px 30px; }
.hero h1 { margin:0 0 6px; font-size:30px; }
.hero .meta { color:#c6c9e4; font-size:14px; line-height:1.7; }
.wrap { max-width:1080px; margin:0 auto; padding:26px 28px 60px; }
.cards { display:flex; gap:16px; flex-wrap:wrap; margin:-52px 0 26px; position:relative; }
.card { flex:1; min-width:150px; background:#fff; border-radius:14px; padding:18px 20px;
        box-shadow:0 6px 22px rgba(30,25,70,.10); }
.card .num { font-size:30px; font-weight:700; }
.card .lbl { color:#6b7089; font-size:13px; margin-top:2px; }
.ok { color:#169b57; } .bad { color:#d83a52; } .muted{color:#8a8fa8;}
.banner { border-radius:12px; padding:14px 18px; margin-bottom:22px; font-weight:600; }
.banner.pass { background:#e2f7ec; color:#0f7a43; }
.banner.fail { background:#fde7eb; color:#b3233b; }
table { width:100%; border-collapse:collapse; background:#fff; border-radius:12px; overflow:hidden;
       box-shadow:0 2px 12px rgba(30,25,70,.07); }
th, td { text-align:left; padding:10px 14px; font-size:13.5px; border-bottom:1px solid #eef0f7; }
th { background:#fafbff; color:#5a607a; font-weight:600; }
td.r { text-align:right; font-variant-numeric:tabular-nums; }
h2 { font-size:18px; margin:34px 0 12px; }
pre { background:#17182b; color:#e8e9f5; padding:16px 18px; border-radius:12px;
      font-size:12.5px; line-height:1.55; overflow-x:auto; white-space:pre-wrap; }
.pill { display:inline-block; padding:2px 10px; border-radius:99px; font-size:12px; font-weight:600; }
.pill.ok { background:#e2f7ec; } .pill.bad { background:#fde7eb; }
footer { color:#8a8fa8; font-size:12px; margin-top:40px; text-align:center; }
"""


def render(suites, failures, failed_names, compile_errors):
    tot, missing = summarize(suites)
    all_ok = tot["failed"] == 0 and not compile_errors and not missing
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    label = os.environ.get("REPORT_LABEL") or f"{platform.system()} {platform.release()}"
    rustc = os.environ.get("REPORT_RUSTC", "(not captured)")

    def esc(x):
        return html.escape(str(x))

    rows = []
    for s in suites:
        r = s["result"]
        if r is None:
            status = '<span class="pill bad">no result</span>'
            vals = ("-",) * 6
        else:
            cls = "ok" if r["failed"] == 0 and r["status"] == "ok" else "bad"
            status = f'<span class="pill {cls}">{esc(r["status"])}</span>'
            vals = (r["passed"], r["failed"], r["ignored"], r["measured"], r["filtered"], r["secs"])
        rows.append(
            "<tr><td>{}<div class='muted' style='font-size:11px;margin-top:2px'>{}</div></td>"
            "<td>{}</td><td class='r'>{}</td><td class='r'>{}</td><td class='r'>{}</td>"
            "<td class='r'>{}</td><td class='r'>{}</td><td class='r'>{}</td></tr>".format(
                esc(s["name"]), esc(s["path"]), status, *[esc(v) for v in vals]
            )
        )

    fail_sections = ""
    if failed_names:
        fail_sections += "<p>失败用例: " + ", ".join(esc(n) for n in failed_names) + "</p>"
    for i, blk in enumerate(failures, 1):
        fail_sections += f"<h2>失败详情 {i}</h2><pre>{esc(blk)}</pre>"
    for i, ce in enumerate(compile_errors[:20], 1):
        fail_sections += f"<h2>编译错误 {i}</h2><pre>{esc(ce)}</pre>"

    banner = (
        '<div class="banner pass">✓ 全部测试通过</div>'
        if all_ok
        else '<div class="banner fail">✕ 存在失败或错误,请查看下方详情</div>'
    )

    return f"""<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<title>Nebula 测试报告 - {esc(label)}</title><style>{CSS}</style></head>
<body>
<div class="hero"><h1>Nebula 全量测试报告</h1>
<div class="meta">平台: {esc(label)} &nbsp;|&nbsp; Rust: {esc(rustc)}<br>生成时间: {esc(now)}</div></div>
<div class="wrap">
<div class="cards">
  <div class="card"><div class="num {'ok' if all_ok else 'bad'}">{'PASS' if all_ok else 'FAIL'}</div><div class="lbl">总体结论</div></div>
  <div class="card"><div class="num ok">{tot['passed']}</div><div class="lbl">通过</div></div>
  <div class="card"><div class="num bad">{tot['failed']}</div><div class="lbl">失败</div></div>
  <div class="card"><div class="num muted">{tot['ignored']}</div><div class="lbl">忽略</div></div>
  <div class="card"><div class="num muted">{tot['filtered']}</div><div class="lbl">过滤</div></div>
</div>
{banner}
<table><thead><tr><th>测试目标</th><th>状态</th><th class="r">通过</th><th class="r">失败</th>
<th class="r">忽略</th><th class="r">度量</th><th class="r">过滤</th><th class="r">耗时(s)</th></tr></thead>
<tbody>{''.join(rows)}</tbody></table>
{fail_sections}
<footer>Nebula · 本地优先的个人记忆检索引擎 · 报告由 scripts/make_test_report.py 自动生成</footer>
</div></body></html>"""


def main():
    args = sys.argv[1:]
    run_cargo = False
    if args and args[0] == "--run":
        run_cargo = True
        args = args[1:]
    if len(args) != 1:
        sys.exit("usage: make_test_report.py [--run] <out.html>")
    out_path = args[0]

    cargo_code = 0
    if run_cargo:
        cmd = ["cargo", "test", "--workspace", "--no-fail-fast"]
        proc = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        text = proc.stdout
        cargo_code = proc.returncode
        log_path = os.path.join(os.path.dirname(os.path.abspath(out_path)), "cargo-test.log")
        with open(log_path, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        text = sys.stdin.read()

    suites, failures, failed_names, compile_errors = parse(text)
    out = render(suites, failures, failed_names, compile_errors)
    os.makedirs(os.path.dirname(os.path.abspath(out_path)), exist_ok=True)
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(out)
    tot, missing = summarize(suites)
    print(
        f"report: {out_path} | cargo_exit={cargo_code} | passed={tot['passed']} "
        f"failed={tot['failed']} suites={len(suites)} missing={len(missing)} "
        f"compile_errors={len(compile_errors)}"
    )
    if cargo_code != 0 or tot["failed"] or compile_errors or missing:
        sys.exit(1)


if __name__ == "__main__":
    main()
