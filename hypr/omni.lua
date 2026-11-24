-- omni launcher popup.
-- No Hyprland blur: omni blurs its own screen capture in Glass mode (src/glass.rs).
-- Plain fade in/out: a scaling animation made the edges break up while fading.
hl.layer_rule({
    match = { namespace = "omni" },
    animation = "fade",
})
