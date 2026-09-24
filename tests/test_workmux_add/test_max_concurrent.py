"""Tests for --max-concurrent worker pool functionality."""

from pathlib import Path


from ..conftest import (
    MuxEnvironment,
    ShellCommands,
    pane_quote,
    run_workmux_command,
    write_workmux_config,
)


class TestMaxConcurrent:
    """Tests for --max-concurrent flag."""

    def test_max_concurrent_processes_sequentially(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        shell_cmd: ShellCommands,
    ):
        """Verifies --max-concurrent limits parallel worktrees and processes queue."""
        env = mux_server

        # The pane is the shell this test names, because its command is written
        # for one: a slot is freed when the pane goes away, and a pane left to
        # a host whose `$SHELL` is PowerShell keeps the slot its `sleep` never
        # freed, so the second item waits for it until the command times out.
        env.configure_default_shell(shell_cmd.path)

        # Configure pane to auto-close after a short delay (simulates agent completing)
        write_workmux_config(
            mux_repo_path, panes=[{"command": shell_cmd.exit_after(1)}]
        )

        # 2 items with max-concurrent 1 = sequential processing
        # If worker pool works, this completes; if broken, it hangs forever
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add task --max-concurrent 1 "
            f"--branch-template {pane_quote('{{ base_name }}-{{ index }}')}",
            stdin_input="first\nsecond",
        )

        # Verify both worktrees were created (branches exist)
        for idx in [1, 2]:
            worktree_path = (
                mux_repo_path.parent
                / f"{mux_repo_path.name}__worktrees"
                / f"task-{idx}"
            )
            assert worktree_path.is_dir(), f"Expected worktree at {worktree_path}"
