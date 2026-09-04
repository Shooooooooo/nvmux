//! The picker. Milestone 3.
//!
//! # The contract
//!
//! The whole screen is a centered list of session names and one dimmed line of
//! keybind hints on the last row. No borders, no title bar, no status header, no
//! logo, no metadata columns, no help popup. Prompts (create, rename, kill
//! confirm, filter) replace the hint line *in place* rather than opening a modal
//! or a bordered popup.
//!
//! # There is no preview pane, and there must never be one
//!
//! Beyond wanting a clean screen, there is a hard technical reason. Neovim sizes
//! the global grid to the per-dimension **minimum** across every attached UI, so
//! attaching a small second UI to render a preview would shrink the grid of the
//! session the user is actually editing in, and fire `VimResized`:
//!
//! ```text
//!   src/nvim/ui.c, ui_refresh(), v0.11.4:
//!       int width = INT_MAX;
//!       int height = INT_MAX;
//!       for (size_t i = 0; i < ui_count; i++) {
//!         RemoteUI *ui = uis[i];
//!         width = MIN(ui->width, width);
//!         height = MIN(ui->height, height);
//!       }
//!       screen_resize(width, height);
//! ```
//!
//! `ui_refresh()` runs unconditionally at the end of `ui_attach_impl()`.
//! Confirmed empirically: attaching a 40x10 UI alongside a 120x40 one collapses
//! the grid to 40x10 for both.
//!
//! There is no read-only or observer attach mode that opts out of the size
//! calculation. None of the `ui_options` (`rgb`, `ext_cmdline`, `ext_popupmenu`,
//! `ext_tabline`, `ext_wildmenu`, `ext_messages`, `ext_linegrid`,
//! `ext_multigrid`, `ext_hlstate`, `ext_termcolors`) makes an attachment
//! non-sizing, and the `override` flag affects only ext_widgets, never width or
//! height.
//!
//! So: session state in the picker comes from RPC (`nvim_list_bufs`,
//! `nvim_get_option_value`), and a second UI is never attached to a live session.
//!
//! # Colour
//!
//! Use the terminal's default background; set no background colour and hardcode
//! no palette. Respect `NO_COLOR`.
//!
//! Do **not** rely on crossterm's own `NO_COLOR` handling: with `NO_COLOR` set it
//! turns `SetForegroundColor(c)` into a bare `ESC[m`, which is a full SGR reset
//! that wipes bold, reverse and dim mid-line. Call
//! `crossterm::style::force_color_output(true)` once, test the variable
//! directly, and simply do not set colours when it is present.
//! `Modifier::REVERSED` alone makes a good `NO_COLOR`-safe selection highlight.

/// What the picker returned. Milestone 3 fills this in.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Attach to this session.
    Attach(crate::session::Session),
    /// The user quit.
    Quit,
}
