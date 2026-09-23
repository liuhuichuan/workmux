"""
Basic workmux add tests that work with both tmux and WezTerm backends.

Run with:
    WORKMUX_TEST_BACKEND=wezterm pytest tests/test_workmux_add/test_basic_multibackend.py -v
    WORKMUX_TEST_BACKEND=tmux pytest tests/test_workmux_add/test_basic_multibackend.py -v
    pytest --backend=wezterm tests/test_workmux_add/test_basic_multibackend.py -v
"""

from pathlib import Path

import pytest

from ..conftest import (
    MuxEnvironment,
    assert_window_exists,
    get_window_name,
    get_worktree_path,
    run_workmux_command,
    run_workmux_open,
    setup_git_repo,
    write_workmux_config,
)


@pytest.fixture
def repo_path(mux_server: MuxEnvironment) -> Path:
    """Initialize a git repo in the test env and return its path."""
    path = mux_server.tmp_path
    setup_git_repo(path, mux_server.env)
    return path


def skip_on_tmux(env: MuxEnvironment) -> None:
    """Skip a test that checks what happens where tmux is *not* the backend."""
    if env.backend_name == "tmux":
        pytest.skip("session mode is what tmux does, so tmux has nothing to refuse")


class TestWorktreeCreation:
    """Tests for basic worktree creation with workmux add."""

    def test_add_creates_worktree(
        self, mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
    ):
        """workmux add should create a new git worktree."""
        branch_name = "test-feature"

        write_workmux_config(repo_path)
        run_workmux_command(
            mux_server, workmux_exe_path, repo_path, f"add {branch_name}"
        )

        expected_path = get_worktree_path(repo_path, branch_name)
        assert expected_path.exists(), f"Worktree not created at {expected_path}"
        assert (expected_path / ".git").exists(), "Worktree missing .git"

    def test_add_creates_window(
        self, mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
    ):
        """workmux add should create a new multiplexer window/tab."""
        branch_name = "test-feature"

        write_workmux_config(repo_path)
        run_workmux_command(
            mux_server, workmux_exe_path, repo_path, f"add {branch_name}"
        )

        expected_window = get_window_name(branch_name)
        assert_window_exists(mux_server, expected_window)


class TestSessionModeIsRefusedOffTmux:
    """A backend without sessions has to refuse session mode out loud.

    The session suite itself is tmux-only (tests/test_workmux_add/test_session.py),
    which leaves the refusal untested everywhere else. This is where that half
    lives: the command must fail with the backend named, and must fail before it
    has touched the repository.
    """

    def test_add_refuses_session_mode(
        self, mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
    ):
        """`workmux add --session` fails, and leaves no worktree behind."""
        skip_on_tmux(mux_server)
        branch_name = "test-session-refused"

        write_workmux_config(repo_path)
        result = run_workmux_command(
            mux_server,
            workmux_exe_path,
            repo_path,
            f"add {branch_name} --session --background",
            expect_fail=True,
        )

        assert "only supported with tmux" in result.stderr, result.stderr
        assert not get_worktree_path(repo_path, branch_name).exists()

    def test_open_refuses_session_mode(
        self, mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
    ):
        """`workmux open --session` fails for a worktree created in window mode."""
        skip_on_tmux(mux_server)
        branch_name = "test-open-session-refused"

        write_workmux_config(repo_path)
        run_workmux_command(
            mux_server, workmux_exe_path, repo_path, f"add {branch_name}"
        )

        result = run_workmux_open(
            mux_server,
            workmux_exe_path,
            repo_path,
            branch_name,
            session=True,
            expect_fail=True,
        )

        assert "only supported with tmux" in result.stderr, result.stderr
