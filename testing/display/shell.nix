let
  environment = builtins.getFlake ("path:" + toString ../../nix);
  pkgs = import environment.inputs.nixpkgs { system = "x86_64-linux"; };
  libraries = with pkgs; [ gtk3 webkitgtk_4_1 libsoup_3 gdk-pixbuf cairo pango xdotool
    gst_all_1.gstreamer gst_all_1.gst-plugins-base gst_all_1.gst-plugins-good
    gst_all_1.gst-plugins-bad gst_all_1.gst-libav ];
in environment.devShells.x86_64-linux.default.overrideAttrs (previous: {
  buildInputs = previous.buildInputs ++ libraries ++ (with pkgs; [ weston pulseaudio alsa-plugins ]);
  LD_LIBRARY_PATH = previous.LD_LIBRARY_PATH + ":" + pkgs.lib.makeLibraryPath libraries;
  GST_PLUGIN_SYSTEM_PATH_1_0 = pkgs.lib.makeSearchPath "lib/gstreamer-1.0" libraries;
  YAPFAST_FRAMEWORK_ALSA_PLUGINS = "${pkgs.alsa-plugins}/lib/alsa-lib";
  # A cursor theme for `scripts/ui-session start --cursor`; the private display has none.
  YAPFAST_FRAMEWORK_CURSORS = "${pkgs.adwaita-icon-theme}/share/icons";
})
