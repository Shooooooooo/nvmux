# Recording script for the README demo, for VHS: https://github.com/charmbracelet/vhs
#
#     vhs demo.tape        # writes assets/demo.gif
#
# Then uncomment the image line at the top of README.md.
#
# This records the LOCAL flow -- `nvmux` with no host argument -- because that
# needs nothing but `nvim` on PATH, so anyone can regenerate it. To record the
# remote flow instead, change the `Type "nvmux"` line below to `Type "nvmux
# myhost"` and use a host your ssh config can already reach unattended.
#
# `nvmux` must be on PATH before recording:  cargo install --path .
#
# On the prefix: `Ctrl-Space` is the default (see `src/config.rs`). If your
# VHS build will not emit it, record against a different prefix instead of
# changing the default --
#
#     printf '[keys]\nprefix = "Ctrl-a"\n' > /tmp/nvmux-demo.toml
#     NVMUX_CONFIG=/tmp/nvmux-demo.toml vhs demo.tape
#
# ...and swap the `Ctrl+Space` below for `Ctrl+a`.

Output assets/demo.gif

Set Shell bash
Set FontSize 16
Set Width 1000
Set Height 620
Set Padding 20
Set TypingSpeed 60ms

# Start from a clean screen rather than recording the prompt setup.
Hide
Type "clear"
Enter
Show

# The picker.
Type "nvmux"
Sleep 600ms
Enter
Sleep 2s

# Move through the sessions, so the selection is visibly live.
Down
Sleep 700ms
Down
Sleep 700ms

# Attach, and let the editor paint.
Enter
Sleep 3s

# Detach: the session keeps running, nvmux exits.
Ctrl+Space
Sleep 400ms
Type "d"
Sleep 2s
