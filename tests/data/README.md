# Test data

- `imac5k-drm/` mirrors `/sys/class/drm` on a 27" 5K iMac running Ubuntu:
  `card0-eDP-1` is the connected internal panel (native mode 5120x2880),
  `card0-DP-1`/`card0-DP-2` are the unused Thunderbolt/DisplayPort outputs.
  The EDID is a minimal, checksum-valid 128-byte block built for these tests
  (manufacturer `APP`, 60x34 cm in the base block, a 2560x1440@59.95 Hz
  preferred timing with a 597x336 mm image size). A real 5K panel advertises
  its native mode in a DisplayID extension block, which is why the receiver
  takes the native mode from `modes`, not from the EDID.
- `imac5k-machine-id` stands in for `/etc/machine-id` when checking that
  `display_id` stays stable.
