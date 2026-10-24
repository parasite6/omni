-- omni launcher popup.
-- No Hyprland blur: omni's card is an opaque surface (src/glass.rs).
-- Plain fade in/out: a scaling animation made the edges break up while fading.
hl.layer_rule({
    match = { namespace = "omni" },
    animation = "fade",
})
