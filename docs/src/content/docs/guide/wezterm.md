---
title: "WezTerm"
description: Use WezTerm as an alternative multiplexer backend
---

:::caution[Experimental]
The WezTerm backend is new and experimental. Expect rough edges and potential issues.
:::

[WezTerm](https://wezterm.org/) can be used as an alternative to tmux. Detected automatically via `$WEZTERM_PANE`. Works on macOS, Linux, and Windows.

## How workmux talks to WezTerm

workmux shells out to `wezterm cli`, which connects to the mux domain named by `$WEZTERM_UNIX_SOCKET`. WezTerm sets that variable in every pane it spawns -- to the socket of the GUI that owns the pane, or to the mux server that GUI attached to -- so workmux needs no configuration to find the panes around it: it uses the instance of the pane it runs in.

That also defines what workmux can see. A pane belongs to exactly one instance, and panes of another instance (a second `wezterm-gui` process that is not attached to the same mux server) are invisible to it. In that case `workmux list` and the dashboard only show the current instance's agents, and `workmux status` reports that state files exist but none match the current instance, because the agents were registered elsewhere.

Run workmux from inside a WezTerm pane instead of an unrelated terminal. `wezterm cli` needs `$WEZTERM_UNIX_SOCKET` and `$WEZTERM_PANE` from that pane; outside one it has to guess at a socket, and on Windows it fails with `failed to connect to Socket("gui-sock-<pid>")`.

The CLI itself is looked for in four places, and the first that has one wins: beside `$WEZTERM_EXECUTABLE`, on `PATH`, in the directories a Windows install is laid out in, and beside the WezTerm that is running. A portable Windows install, which is in none of the first three, still answers -- the GUI that is running says where it was started from.

### Sharing one instance across GUI windows

Optional, but useful if you keep several WezTerm windows open and want one workmux to see the agents in all of them: connect every GUI to a mux server so they share a single domain.

```lua
local config = wezterm.config_builder()

-- Connect each GUI to the same mux server on startup
config.unix_domains = {
    { name = 'unix' },
}
config.default_gui_startup_args = { 'connect', 'unix' }
```

If you have custom keybindings for creating tabs, keep new tabs in the pane's own domain so they join the same instance:

```lua
-- CORRECT: Uses the current pane's domain
{ key = 't', mods = 'SUPER', action = act.SpawnTab('CurrentPaneDomain') },

-- WRONG: This spawns in the GUI domain, which is a different instance
-- { key = 't', mods = 'SUPER', action = act.SpawnTab({ DomainName = 'local' }) },
```

## Differences from tmux

| Feature              | tmux                 | WezTerm           |
| -------------------- | -------------------- | ----------------- |
| Agent status in tabs | Yes (window names)   | Dashboard only    |
| Tab ordering         | Insert after current | Appends to end    |
| Scope                | tmux session         | WezTerm workspace |

- **Tab ordering**: New tabs appear at the end of the tab bar (no "insert after" support like tmux)
- **Workspace isolation**: workmux operates within the current WezTerm workspace (analogous to tmux sessions). Tabs in other workspaces are not affected.
- **Exit detection**: Uses title heuristics to detect when agents exit

## Requirements

- WezTerm with its CLI available (`wezterm cli` must work inside a pane)
- macOS, Linux, or Windows

## Cross-workspace navigation

The dashboard can show agents from all workspaces with `--all` (or pressing `a`). However, WezTerm's CLI cannot directly switch workspaces. To enable jumping to tabs in other workspaces, add this to your `wezterm.lua`:

```lua
local wezterm = require("wezterm")

wezterm.on("user-var-changed", function(window, pane, name, value)
    if name == "workmux-switch-pane" then
        local data = wezterm.json_parse(value)
        -- Switch to the target workspace
        window:perform_action(
            wezterm.action.SwitchToWorkspace({ name = data.workspace }),
            pane
        )
        -- Find and activate the tab by title (stable across mux contexts)
        wezterm.time.call_after(0.1, function()
            for _, win in ipairs(wezterm.mux.all_windows()) do
                for _, tab in ipairs(win:tabs()) do
                    if tab:get_title() == data.tab_title then
                        tab:activate()
                        local panes = tab:panes()
                        if #panes > 0 then panes[1]:activate() end
                        return
                    end
                end
            end
        end)
    end
end)
```

Without this configuration, the dashboard can display agents from all workspaces but jumping to panes in other workspaces will not work.

## Known limitations

- Cross-workspace jumping requires the Lua handler above
- Agent status icons do not appear in tab titles; the dashboard (and, on Windows, the sidebar) shows the status instead
- The sidebar refreshes on events as well as on a timer. tmux wakes the daemon with a signal; here the pane polls, and a state change made by a workmux command leaves a wake-up token in the state store for that poll to read, so a new status does not sit behind the timer. A change made outside workmux (a pane appearing or closing) is noticed by the timer, about a second later
- Sidebar panes are tracked by the ids workmux records in its settings file, not by title: WezTerm applies a pane's title only on its focused tab, so a sidebar started by hand (`_sidebar-run`) is found only while its tab is focused
- On Windows, `wezterm cli list-clients` reports nothing, so host-window focus is read from the active tab: "the window is focused" and "the tab is active" are the same signal
- `wezterm cli` waits on the mux server for as long as the server takes to answer, and a mux can stop answering for good -- this is what a wedged WezTerm GUI looks like. Every call workmux makes runs under a 20-second deadline instead: the call is killed and reported as an error, so a mux that is gone reads as "Failed to list WezTerm panes" rather than taking the caller down with it
- `wezterm cli` is a process per call, so a listing is read once and shared by everything in that run that asks for it (`workmux list` was measured at ten readings of the same listing). Any command that can change the panes drops the reading, and the callers that run in a loop -- the sidebar's poll, a dashboard refresh, a state reconciliation -- read the mux again on every turn
- Some edge cases may not be as thoroughly tested as the tmux backend

## Credits

Thanks to [@JeremyBYU](https://github.com/JeremyBYU) for contributing WezTerm support.
