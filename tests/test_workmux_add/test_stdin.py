"""Tests for stdin input support in `workmux add`."""

import sys
from pathlib import Path

from ..conftest import (
    DEFAULT_WINDOW_PREFIX,
    MuxEnvironment,
    WINDOWS_COMMAND_TIMEOUT,
    assert_window_exists,
    get_scripts_dir,
    get_worktree_path,
    pane_quote,
    poll_until_file_has_content,
    read_workmux_text,
    run_workmux_command,
    slugify,
    write_workmux_config,
)


class TestStdinInput:
    """Tests for piping input to workmux add via stdin."""

    def test_open_empty_stdin_pipe_does_not_block(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that an open stdin pipe with no data is ignored."""
        env = mux_server
        # Stdin handling does not depend on split-pane shell handshakes.
        write_workmux_config(mux_repo_path, panes=[{"focus": True}])

        scripts_dir = get_scripts_dir(env)
        stdout_file = scripts_dir / "empty_stdin_stdout.txt"
        stderr_file = scripts_dir / "empty_stdin_stderr.txt"
        exit_code_file = scripts_dir / "empty_stdin_exit_code.txt"
        script_file = scripts_dir / "empty_stdin_run.py"

        for path in [stdout_file, stderr_file, exit_code_file]:
            if path.exists():
                path.unlink()

        # The probe below hands `workmux add` a budget and calls the run
        # blocked when the budget runs out. The thing being ruled out is a
        # wait that never ends, so the budget has to be one a run that is
        # merely working can meet: the suite's own answer for how long a
        # command may take on this host.
        #
        # It also reads what workmux printed. Workmux writes UTF-8, and a
        # pane's Python takes the host's own encoding unless told otherwise,
        # which on a Chinese Windows is a code page that cannot decode the
        # tick in the success line: the probe dies of that before it writes
        # the exit code, and the test then reports the command as blocked.
        budget = WINDOWS_COMMAND_TIMEOUT

        script_file.write_text(
            "import os\n"
            "import subprocess\n"
            "import sys\n"
            "read_fd, write_fd = os.pipe()\n"
            "try:\n"
            "    with os.fdopen(read_fd, 'rb', closefd=True) as stdin:\n"
            "        proc = subprocess.Popen([sys.argv[1], 'add', 'topic'], cwd=sys.argv[2], stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE, encoding='utf-8', errors='replace')\n"
            "        try:\n"
            f"            stdout, stderr = proc.communicate(timeout={budget})\n"
            "        except subprocess.TimeoutExpired:\n"
            "            proc.kill()\n"
            "            proc.communicate()\n"
            f"            raise AssertionError('workmux add blocked on open empty stdin pipe for {budget}s')\n"
            "finally:\n"
            "    os.close(write_fd)\n"
            "open(sys.argv[3], 'w', encoding='utf-8').write(stdout)\n"
            "open(sys.argv[4], 'w', encoding='utf-8').write(stderr)\n"
            "open(sys.argv[5], 'w').write(str(proc.returncode))\n"
        )

        env.send_keys(
            "test:",
            " ".join(
                pane_quote(arg)
                for arg in [
                    sys.executable,
                    script_file,
                    workmux_exe_path,
                    mux_repo_path,
                    stdout_file,
                    stderr_file,
                    exit_code_file,
                ]
            ),
            enter=True,
        )

        # Outlast the probe's own budget, so a run it called blocked is
        # reported with the pane showing why rather than as a bare timeout.
        assert poll_until_file_has_content(exit_code_file, timeout=budget + 10.0), (
            env.capture_pane("test:")
        )
        stdout = read_workmux_text(stdout_file)
        stderr = read_workmux_text(stderr_file)

        assert int(exit_code_file.read_text()) == 0, stderr
        assert "Successfully created worktree" in stdout
        assert get_worktree_path(mux_repo_path, "topic").is_dir()
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}topic")

    def test_stdin_creates_multiple_worktrees(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that piping lines to stdin creates multiple worktrees.

        Stdin lines become the 'input' variable in foreach_vars, which is
        appended to the base branch name by the default branch template.
        """
        env = mux_server
        input_data = "feature-a\nfeature-b"

        write_workmux_config(mux_repo_path)

        # Pipe input - the default branch template appends foreach_vars (input) to base_name
        # So "topic" + "feature-a" -> "topic-feature-a"
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add topic",
            stdin_input=input_data,
        )

        # Verify all expected worktrees and windows exist
        for item in ["feature-a", "feature-b"]:
            expected_handle = slugify(f"topic-{item}")
            worktree_path = (
                mux_repo_path.parent
                / f"{mux_repo_path.name}__worktrees"
                / expected_handle
            )
            assert worktree_path.is_dir(), f"Expected worktree at {worktree_path}"

            expected_window = f"{DEFAULT_WINDOW_PREFIX}{expected_handle}"
            assert_window_exists(env, expected_window)

    def test_stdin_with_custom_branch_template(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that stdin input works with custom branch templates."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        # Use custom branch template that puts input first
        template = "{{ input }}-feature"
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add base --branch-template {pane_quote(template)}",
            stdin_input="api\nauth",
        )

        # Verify worktrees were created with custom template
        for item in ["api", "auth"]:
            expected_handle = slugify(f"{item}-feature")
            worktree_path = (
                mux_repo_path.parent
                / f"{mux_repo_path.name}__worktrees"
                / expected_handle
            )
            assert worktree_path.is_dir(), f"Expected worktree at {worktree_path}"
            assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}{expected_handle}")

    def test_stdin_conflicts_with_foreach_flag(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that using stdin and --foreach simultaneously fails."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-branch --foreach 'region:us,eu'",
            stdin_input="input-data",
            expect_fail=True,
        )

        assert "Cannot use --foreach when piping input from stdin" in result.stderr

    def test_stdin_conflicts_with_name_flag(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that using stdin and --name simultaneously fails (multi-worktree constraint)."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-branch --name custom-name",
            stdin_input="input-data",
            expect_fail=True,
        )

        assert "--name cannot be used with multi-worktree generation" in result.stderr

    def test_stdin_conflicts_with_prompt_editor_and_auto_name(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that stdin cannot be used with interactive prompt editor when using -A."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add -A --prompt-editor",
            stdin_input="input-data",
            expect_fail=True,
        )

        assert "Cannot use interactive prompt editor when piping input" in result.stderr

    def test_stdin_overrides_frontmatter_foreach(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that stdin input takes precedence over 'foreach' defined in prompt frontmatter."""
        env = mux_server

        # Create a prompt file with foreach in frontmatter
        # Use -p for inline prompt (empty) so we don't trigger agent requirement
        prompt_file = mux_repo_path / "prompt_with_matrix.md"
        prompt_file.write_text("""---
foreach:
  env: [dev, prod]
---
Task for {{ input }}
""")

        # Configure with an agent pane to satisfy the prompt requirement
        write_workmux_config(mux_repo_path, panes=[{"command": "<agent>"}])

        # Pipe 'api' via stdin. The prompt has 'foreach env', but stdin should override it
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add deploy -P prompt_with_matrix.md",
            stdin_input="api",
        )

        # Warning should be shown
        assert "stdin input overrides prompt frontmatter" in result.stderr

        # Should create 'deploy-api' (base_name + input), ignoring the 'dev/prod' matrix from frontmatter
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}deploy-api")

        # Ensure the frontmatter expansion did NOT happen
        window_list = env.list_windows()
        assert "deploy-dev" not in window_list
        assert "deploy-prod" not in window_list

    def test_empty_stdin_lines_are_filtered(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that empty lines in stdin are ignored."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        # Include empty lines and whitespace-only lines
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add task",
            stdin_input="first\n\n  \nsecond\n",
        )

        # Should only create two worktrees (empty lines filtered)
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}task-first")
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}task-second")

        # Verify no window for empty input
        window_list = env.list_windows()
        # Ensure we only have the expected windows plus the test session window
        window_names = [w for w in window_list if w.startswith(DEFAULT_WINDOW_PREFIX)]
        assert len(window_names) == 2

    def test_stdin_with_whitespace_trimmed(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that whitespace is trimmed from stdin lines."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        # Include lines with leading/trailing whitespace
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add item",
            stdin_input="  padded  \n",
        )

        # Should create worktree with trimmed name: item-padded
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}item-padded")

    def test_stdin_json_lines_expose_keys_as_variables(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that JSON lines are parsed and keys become template variables."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        # Pipe JSON lines - each key should become a template variable
        json_lines = '{"name":"workmux","id":"1"}\n{"name":"tmux-tools","id":"2"}'
        template = "{{ base_name }}-{{ name }}"

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add analyze --branch-template {pane_quote(template)}",
            stdin_input=json_lines,
        )

        # Verify worktrees created with names from JSON 'name' key
        for name in ["workmux", "tmux-tools"]:
            expected_handle = slugify(f"analyze-{name}")
            worktree_path = (
                mux_repo_path.parent
                / f"{mux_repo_path.name}__worktrees"
                / expected_handle
            )
            assert worktree_path.is_dir(), f"Expected worktree at {worktree_path}"
            assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}{expected_handle}")

    def test_stdin_json_lines_preserve_input_variable(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that {{ input }} contains the raw JSON line."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        # Use {{ input }} in template - should get the raw JSON string (slugified)
        json_line = '{"name":"test"}'
        template = "{{ base_name }}-{{ index }}"

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add task --branch-template {pane_quote(template)}",
            stdin_input=json_line,
        )

        # {{ index }} should be 1 for single item
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}task-1")
