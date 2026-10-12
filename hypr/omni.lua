-- omni launcher popup.
-- No Hyprland blur: the global blur is far too strong for glass, so omni captures the
-- screen behind its card and blurs it itself (src/glass.rs).
-- Plain fade in/out: a scaling animation made the edges break up while fading.
hl.layer_rule({
    match = { namespace = "omni" },
    animation = "fade",
})
