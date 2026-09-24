"""Tests for the sidebar WezTerm runs in a pane of its own.

The sidebar has a runtime per platform. On Unix a daemon polls tmux and pushes
snapshots down a socket; on Windows each sidebar pane polls WezTerm and the
state store itself. This suite drives the Windows one, which is the runtime the
port added, so it stands aside where tmux is the backend.
"""

from pathlib import Path
from typing import cast

import pytest

from .conftest import (
    MuxEnvironment,
    WezTermEnvironment,
    make_env_script,
    pane_home_env,
    pane_quote,
    pane_temp_env,
    poll_until,
)

# The title the sidebar claims for its pane, so a human can tell it from a
# shell. It is the same one the pane's loop reports.
SIDEBAR_PANE_TITLE = "workmux-sidebar"


def skip_on_tmux(env: MuxEnvironment) -> None:
    """Stand aside where the sidebar under test is the daemon runtime."""
    if env.backend_name == "tmux":
        pytest.skip(
            "the tmux sidebar is the daemon runtime, which this suite does not drive"
        )


def host_pane_id(env: WezTermEnvironment) -> str:
    """The pane the test's own shell runs in, which a sidebar splits off."""
    panes = env._list_panes()
    assert panes, "the test workspace has no pane to put a sidebar beside"
    return str(panes[0]["pane_id"])


def sidebar_panes(env: WezTermEnvironment) -> list[dict]:
    """The sidebar panes of the test workspace, as the mux reports them."""
    return [p for p in env._list_panes() if p.get("title") == SIDEBAR_PANE_TITLE]


def pane_text(env: MuxEnvironment, pane_id: str) -> str:
    """What the pane has drawn, as `wezterm cli get-text` reports it."""
    return env.mux_command(["get-text", "--pane-id", pane_id]).stdout


def send_to_pane(env: MuxEnvironment, pane_id: str, text: str) -> None:
    """Type `text` into one pane, without the newline a command line wants."""
    env.mux_command(["send-text", "--pane-id", pane_id, "--no-paste", text])


def run_in_pane(env: MuxEnvironment, pane_id: str, command: str) -> None:
    """Run `command` in a pane, pointed at this test's own home and temp files.

    A pane inherits the environment of the multiplexer that spawned it, which
    knows nothing of this test, and the sidebar would both read and write the
    account's own state store without being told where else to look.
    """
    invocation = make_env_script(
        env, command, {**pane_home_env(env), **pane_temp_env(env)}
    )
    send_to_pane(env, pane_id, invocation + "\r")


def open_sidebar(env: WezTermEnvironment, workmux_exe_path: Path) -> str:
    """Ask the host pane for a sidebar and answer with the pane it landed in."""
    run_in_pane(env, host_pane_id(env), f"{pane_quote(workmux_exe_path)} sidebar on")
    assert poll_until(lambda: len(sidebar_panes(env)) == 1, timeout=20.0), (
        "no sidebar pane appeared"
    )
    return str(sidebar_panes(env)[0]["pane_id"])


class TestWezTermSidebar:
    """Tests for the polling sidebar runtime, which is the one WezTerm has."""

    def test_on_opens_a_pane_beside_the_host_that_draws(
        self, mux_server: MuxEnvironment, workmux_exe_path: Path
    ):
        """`workmux sidebar on` puts a drawing sidebar in the host pane's tab."""
        env = cast(WezTermEnvironment, mux_server)
        skip_on_tmux(env)
        host = host_pane_id(env)

        sidebar = open_sidebar(env, workmux_exe_path)

        host_pane = next(p for p in env._list_panes() if str(p["pane_id"]) == host)
        sidebar_pane = next(
            p for p in env._list_panes() if str(p["pane_id"]) == sidebar
        )
        assert sidebar_pane["tab_id"] == host_pane["tab_id"], (
            "the sidebar belongs beside the pane it was asked for"
        )
        # A pane is only a sidebar once it has drawn; an empty one would answer
        # the same as a pane that never started.
        assert poll_until(
            lambda: "No agents running" in pane_text(env, sidebar), timeout=20.0
        ), f"the sidebar never drew its empty state:\n{pane_text(env, sidebar)}"

    def test_quit_in_the_sidebar_asks_first_and_leaves_the_host_pane(
        self, mux_server: MuxEnvironment, workmux_exe_path: Path
    ):
        """A quit typed into the sidebar asks, then takes only the sidebar."""
        env = cast(WezTermEnvironment, mux_server)
        skip_on_tmux(env)
        host = host_pane_id(env)
        sidebar = open_sidebar(env, workmux_exe_path)

        send_to_pane(env, sidebar, "q")
        assert poll_until(
            lambda: "Quit sidebar?" in pane_text(env, sidebar), timeout=10.0
        ), "the sidebar went without asking"

        send_to_pane(env, sidebar, "y")
        assert poll_until(lambda: sidebar_panes(env) == [], timeout=15.0), (
            "the sidebar pane outlived its quit"
        )
        assert [str(p["pane_id"]) for p in env._list_panes()] == [host], (
            "the quit took a pane it was not asked to"
        )

    def test_off_from_the_host_pane_closes_the_sidebar(
        self, mux_server: MuxEnvironment, workmux_exe_path: Path
    ):
        """`workmux sidebar off` takes the sidebar and nothing else."""
        env = cast(WezTermEnvironment, mux_server)
        skip_on_tmux(env)
        host = host_pane_id(env)
        open_sidebar(env, workmux_exe_path)

        run_in_pane(env, host, f"{pane_quote(workmux_exe_path)} sidebar off")

        assert poll_until(lambda: sidebar_panes(env) == [], timeout=15.0), (
            "`sidebar off` left the sidebar running"
        )
        assert [str(p["pane_id"]) for p in env._list_panes()] == [host], (
            "`sidebar off` took a pane it was not asked to"
        )
