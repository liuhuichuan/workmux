"""Helpers for interactive `workmux setup` tests."""

import json
import tempfile
from pathlib import Path

from ..conftest import (
    MuxEnvironment,
    get_scripts_dir,
    make_env_script,
    pane_exit_status,
    pane_home_env,
    pane_quote,
    poll_until_file_has_content,
    wait_for_pane_output,
)


def write_claude_manual_status_hook(claude_dir: Path) -> None:
    claude_dir.mkdir(parents=True, exist_ok=True)
    plugin_path = Path(__file__).parents[2] / ".claude-plugin" / "plugin.json"
    plugin = json.loads(plugin_path.read_text())
    (claude_dir / "settings.json").write_text(json.dumps({"hooks": plugin["hooks"]}))


BUNDLED_DIR = Path(__file__).parents[2] / "resources"


def bundled_resource(*parts: str) -> str:
    """The text of a bundled resource, line endings and all.

    Read and written as bytes, because `read_text` and `write_text` translate
    line endings to the platform's own: a Windows copy of the plugin then
    differs from the bundled one on every line, workmux reads that difference
    as an outdated install and asks to replace it, and a test that means
    "already installed" gets a prompt instead of an answer.
    """
    return BUNDLED_DIR.joinpath(*parts).read_bytes().decode("utf-8")


def write_verbatim(path: Path, text: str) -> None:
    """Write `text` keeping the line endings it carries."""
    path.write_bytes(text.encode("utf-8"))


def run_setup_interactive(env: MuxEnvironment, workmux_exe_path: Path) -> Path:
    scripts_dir = get_scripts_dir(env)
    exit_code_file = scripts_dir / "setup_exit_code.txt"
    if exit_code_file.exists():
        exit_code_file.unlink()

    script = make_env_script(
        env,
        # Read by the pane's own shell, so written in the words that shell
        # uses -- `;` is no separator to cmd.exe, and the exit code of the
        # line just run is `%ERRORLEVEL%` there. A line of its own, because
        # `%ERRORLEVEL%` beside the command would be read before it ran.
        f"{pane_quote(workmux_exe_path)} setup\n{pane_exit_status(exit_code_file)}",
        {
            "PATH": env.env["PATH"],
            "TMPDIR": env.env.get("TMPDIR") or tempfile.gettempdir(),
            # The home a setup run reads: `$HOME` on POSIX, and on Windows the
            # profile variable, without which the run finds the real account's
            # agent directories and answers about those instead of the test's.
            **pane_home_env(env),
        },
    )
    env.send_keys("test:", script, enter=True)
    return exit_code_file


def run_setup_with_answers(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    *,
    hooks_answer: str = "y",
    skills_answer: str = "n",
    expected_output: tuple[str, ...] = (),
    timeout: float = 5.0,
) -> Path:
    exit_code_file = run_setup_interactive(env, workmux_exe_path)
    for text in expected_output:
        wait_for_pane_output(env, "test", text, timeout=timeout)
    wait_for_pane_output(
        env, "test", "Install or update status tracking hooks?", timeout=timeout
    )
    env.send_keys("test:", hooks_answer)
    wait_for_pane_output(env, "test", "Install bundled skills?", timeout=timeout)
    env.send_keys("test:", skills_answer)

    assert poll_until_file_has_content(exit_code_file, timeout=timeout)
    assert exit_code_file.read_text().strip() == "0"
    return exit_code_file
