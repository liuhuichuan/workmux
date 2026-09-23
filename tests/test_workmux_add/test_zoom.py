"""Tests for pane zoom configuration in `workmux add`.

The two backends answer differently, so each test asks its own backend. tmux
reports the zoom on the window: `#{window_zoomed_flag}` is set on every pane of
a zoomed window. WezTerm reports it on the pane, as `is_zoomed` in
`wezterm cli list --format json`, which names the one pane that is zoomed.
"""

from pathlib import Path

from ..conftest import (
    MuxEnvironment,
    TmuxEnvironment,
    get_window_name,
    write_workmux_config,
)
from .conftest import add_branch_and_get_worktree


def wezterm_tab_panes(env: MuxEnvironment, window_name: str) -> list[dict]:
    """The panes of a WezTerm tab, in the order they were created."""
    panes = [p for p in env._list_panes() if p.get("tab_title") == window_name]
    return sorted(panes, key=lambda pane: pane["pane_id"])


class TestPaneZoom:
    """Tests for zoom: true pane configuration."""

    def test_zoom_pane_is_zoomed(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that a pane with zoom: true is zoomed after creation."""
        env = mux_server
        branch_name = "feature-zoom"
        window_name = get_window_name(branch_name)

        write_workmux_config(
            mux_repo_path,
            panes=[
                {"command": "echo zoomed", "zoom": True},
                {"command": "echo background", "split": "horizontal", "size": 15},
            ],
        )

        add_branch_and_get_worktree(env, workmux_exe_path, mux_repo_path, branch_name)

        if isinstance(env, TmuxEnvironment):
            # All panes in a zoomed window report the same zoomed flag.
            result = env.mux_command(
                ["list-panes", "-t", window_name, "-F", "#{window_zoomed_flag}"]
            )
            flags = result.stdout.strip().split("\n")
            assert "1" in flags, f"Expected window to be zoomed but got: {flags}"
        else:
            panes = wezterm_tab_panes(env, window_name)
            zoomed = [p for p in panes if p.get("is_zoomed")]
            assert len(zoomed) == 1, f"Expected exactly one zoomed pane, got: {panes}"

    def test_zoom_implies_focus(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that zoom: true implies focus on that pane."""
        env = mux_server
        branch_name = "feature-zoom-focus"
        window_name = get_window_name(branch_name)

        write_workmux_config(
            mux_repo_path,
            panes=[
                {"command": "echo first"},
                {
                    "command": "echo zoomed",
                    "split": "horizontal",
                    "size": 15,
                    "zoom": True,
                },
            ],
        )

        add_branch_and_get_worktree(env, workmux_exe_path, mux_repo_path, branch_name)

        # The second pane is the one that asked to be zoomed, and both backends
        # zoom the pane they focus, so it is the one holding the focus.
        if isinstance(env, TmuxEnvironment):
            result = env.mux_command(
                ["list-panes", "-t", window_name, "-F", "#{pane_active} #{pane_index}"]
            )
            lines = result.stdout.strip().split("\n")
            active_panes = [line for line in lines if line.startswith("1 ")]
            assert len(active_panes) == 1, lines
            assert active_panes[0] == "1 1", lines
        else:
            panes = wezterm_tab_panes(env, window_name)
            assert len(panes) == 2, panes
            zoomed_pane = panes[1]
            assert zoomed_pane["is_zoomed"], panes
            assert zoomed_pane["is_active"], panes
