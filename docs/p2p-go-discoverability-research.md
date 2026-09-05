# Wi-Fi P2P Autonomous GO — Discoverable-for-Miracast Research (wpa_supplicant 2.10, Ubuntu 24.04)

Pure external research. Sources cited inline.

## (1) Config keys for discovery

- **p2p_listen_channel** / **p2p_listen_reg_class**: The Listen channel is the channel the GO
  sits on during Listen state to answer P2P Probe Requests. To FORCE a 2.4 GHz social channel
  set the pair explicitly. For a social channel on the 2.4 GHz band (1/6/11) the operating
  class is **81** and no separate reg_class is strictly required (it defaults to 81) — per the
  README `p2p_set listen_channel` note: "When specifying a social channel on the 2.4 GHz band
  (1/6/11) there is no need to specify the operating class since it defaults to 81."
  [hostap README-P2P, `p2p_set listen_channel`]
  - **To force listen on 2.4 GHz social channel 1**: `p2p_listen_reg_class=81` + `p2p_listen_channel=1`
    (use 6 or 11 for the other social channels). This is the widely-used config idiom.
    [hackerj.tistory.com/33 — documents exactly `p2p_listen_reg_class=81 / p2p_listen_channel=1 /
    p2p_oper_reg_class=81 / p2p_oper_channel=1` to force channel 1]
- **p2p_oper_channel** / **p2p_oper_reg_class**: preferred OPERATING channel for a GO. Set to
  `81` / `1` (or 6/11) to prefer 2.4 GHz. Note this is only a *preference* for negotiation; the
  authoritative channel force for an autonomous GO is `freq=` on `p2p_group_add` (see §2).
- **p2p_go_intent** (0..15): GO Negotiation tie-breaker; `15` forces this device to become GO.
  Irrelevant for an *autonomous* GO (`p2p_group_add` makes you GO unconditionally), but harmless
  to set. [hostap README-P2P, `set p2p_go_intent`; `p2p_connect ... go_intent=`]
- **p2p_no_group_iface**: when `1`, wpa_supplicant runs the group (GO) on the MAIN interface
  instead of spawning a separate `p2p-wlan0-0` virtual interface. Set `1` on single-radio
  adapters or when a tool/driver cannot cope with the dynamic group iface. Trade-off: with a
  separate group iface (default, =0) the P2P Device can keep doing discovery while the group
  runs. [hostap README-P2P group-interface discussion; corroborated widely in Miracast setups]
- **p2p_disabled=1**: turns P2P off entirely — must be **absent or 0**. [wpa_supplicant.conf,
  "Disable P2P functionality # p2p_disabled=1"]
- **country=**: DOES matter. The regulatory domain gates which channels/classes are permitted;
  a wrong or unset country can make the driver refuse channels (P2P then advertises a truncated
  channel list). Set `country=<ISO2>` (e.g. `country=US`/`DE`) and also `set country` at runtime.
  [wpa_supplicant.conf "Country code ... #country=US"; Arch forum id=241240 shows a device
  restricted to `P2P: channels: 81:1..11` when the reg domain limited it]

Answer to the specific sub-question: **p2p_listen_channel=1 (with p2p_listen_reg_class=81)**
forces the GO to listen on 2.4 GHz social channel 1. Use 6 or 11 for the other social channels.

## (2) p2p_group_add parameters on 2.10

From hostap README-P2P, the accepted form is:

    p2p_group_add [persistent|persistent=<network id>] [freq=<freq in MHz>] [ht40] [vht] [he]

- **freq=<MHz>**: forces the GO onto a specific channel. `freq=2412` = 2.4 GHz channel 1
  (2437 = ch6, 2462 = ch11). Special values: **freq=2** = "best 2.4 GHz channel auto-selected",
  **freq=5** = best 5 GHz. [README-P2P `p2p_group_add`]
- **persistent** / **persistent=<id>**: create/restart a persistent group.
- **ht40**, **vht**, **he**: request wider/newer PHY. For Miracast interop, prefer plain
  `freq=2412` (no ht40) unless you know both ends want 40 MHz.

To force a 2.4 GHz GO: `p2p_group_add freq=2412` (deterministic) or `p2p_group_add freq=2`
(best 2.4 GHz). The `freq=` on group_add is the reliable channel force — stronger than the
`p2p_oper_*` config preferences.

## (3) Is an autonomous GO discoverable WITHOUT a concurrent p2p_find?

**Yes.** Once `p2p_group_add` starts the GO it BEACONS and RESPONDS to Probe Requests like any
AP/GO; a peer (phone) discovers it via those beacons/probe responses. A concurrent `p2p_find`
is NOT required for the phone to *see* the GO. [README-P2P: `p2p_listen` note "keep the device
discoverable without having to maintain a group" implies the group itself is already
discoverable; StackOverflow 18578309: "An autonomous GO should be detected by any legacy wifi
device — no special configuration is necessary. The GO should be beaconing and responding to any
probe requests."]

Caveat: `p2p_find`/`p2p_listen` control the P2P **Device** Listen-state discovery (finding
PEERS and answering P2P device-discovery probes on the Listen channel). If your adapter uses a
separate P2P device iface and the phone relies on P2P device discovery rather than scanning the
GO's operating channel, some stacks benefit from the device staying discoverable. But for
Miracast the sink phone typically scans and connects to the GO's BSS directly — the beaconing GO
is discoverable on its operating channel without a running `p2p_find`.

## (4) Common reasons a GO beacons but a phone can't discover it

- **GO on 5 GHz / wrong channel**: many phones only scan 2.4 GHz social channels for Miracast, or
  DFS/5 GHz channels are region-restricted. Fix: force `freq=2412` (or 2437/2462).
  [raspberrypi/linux#4078 shows a dual-band Pi GO started at 2.4 GHz then autonomously steering
  to 5 GHz — the phone loses it; pin the band.]
- **Single-radio SCC constraint**: on a single-radio adapter the GO is forced onto the same
  channel as any active STA connection (Same-Channel Concurrency). If the STA is on a 5 GHz or
  non-social channel, the GO inherits it and the phone can't find it on 2.4 social. Fix:
  disconnect the STA (or use a radio that allows independent channels) so `freq=2412` is honored.
- **Missing Wi-Fi Display (WFD) IE**: for a phone's *Miracast* UI to list the GO as a display
  sink/source, wpa_supplicant must advertise the WFD information element. That requires
  `CONFIG_WIFI_DISPLAY=y` in the build AND enabling WFD at runtime
  (`set wifi_display 1` + `wfd_subelem_set 0 <hex>`). Without it the device beacons as a plain
  P2P GO and the phone's Miracast scanner ignores it. [albfan/miraclecast#50, #92 — "wpa_supplicant
  does not support wifi-display" despite P2P working; miraclecast enables WFD subelems]
- **Wrong reg domain**: unset/wrong `country=` prunes the allowed channel list so the intended
  social channel is never beaconed. [Arch forum id=241240]
- **Separate group iface confusion**: tooling talking to the wrong control socket
  (`wlan0` vs `p2p-wlan0-0`). `p2p_no_group_iface=1` keeps everything on the main iface.

## Concrete recommended config + command sequence (discoverable 2.4 GHz Miracast source GO)

`/etc/wpa_supplicant/p2p.conf`:

    ctrl_interface=/run/wpa_supplicant
    update_config=1
    device_name=Ubuntu-Miracast
    device_type=7-0050F204-1          # 7 = Display category (Miracast source/sink)
    country=US                        # set YOUR ISO2 code
    p2p_go_intent=15
    p2p_listen_reg_class=81
    p2p_listen_channel=1              # force 2.4 GHz social ch 1
    p2p_oper_reg_class=81
    p2p_oper_channel=1
    p2p_no_group_iface=1              # single-radio friendliness (optional; see note)
    # p2p_disabled MUST be absent / 0
    # WFD requires wpa_supplicant built with CONFIG_WIFI_DISPLAY=y

Start + commands (wpa_supplicant must be built with `CONFIG_P2P=y CONFIG_AP=y CONFIG_WPS=y`,
and `CONFIG_WIFI_DISPLAY=y` for Miracast):

    sudo wpa_supplicant -B -i wlan0 -Dnl80211 -c /etc/wpa_supplicant/p2p.conf

    wpa_cli -i wlan0 set country US

    # Enable Wi-Fi Display + advertise WFD IE (primary sink/source subelem).
    # 0x00 = WFD Device Info subelem id; value is the 6-hex-nibble device-info payload.
    # Example WFD Device Information for a Source at RTSP port 7236:
    wpa_cli -i wlan0 set wifi_display 1
    wpa_cli -i wlan0 wfd_subelem_set 0 000600111c440032   # (device-info; adjust bits/port)

    # Create the autonomous GO forced onto 2.4 GHz channel 1:
    wpa_cli -i wlan0 p2p_group_add freq=2412

    # Group runs on p2p-wlan0-0 (or wlan0 if p2p_no_group_iface=1). Confirm:
    wpa_cli -i wlan0 interface                     # list ifaces
    wpa_cli -i p2p-wlan0-0 status                  # GO state, SSID DIRECT-xx, freq 2412

    # (Optional) make the P2P Device discoverable for device-discovery probes too:
    wpa_cli -i wlan0 p2p_listen

    # Allow a client to join via push-button when the phone connects:
    wpa_cli -i p2p-wlan0-0 wps_pbc

The GO now beacons on channel 1 with the WFD IE — a phone's Miracast scanner should list it
WITHOUT you running `p2p_find`. Run `p2p_listen` only if you also want the P2P Device iface to
answer device-discovery probes concurrently.

Notes:
- If the adapter is single-radio and a normal STA connection is up on a different channel, the GO
  will be dragged to that channel (SCC) and `freq=2412` won't stick — drop the STA first.
- `wfd_subelem_set` payload must match the WFD spec Device Information bitmap for your role
  (source vs primary-sink) and RTSP port; the example above is illustrative.
- All P2P operation commands (`p2p_group_add`, `p2p_listen`, `set wifi_display`) go on the MAIN
  iface; WPS/group-status commands (`wps_pbc`, `status`, `wps_pin`) go on the GROUP iface.
  [hostap README-P2P: "Most of the P2P operations are done on the main interface ... Group
  Operations (These are used on the group interface.)"]

## Primary citations
- hostap README-P2P (authoritative): https://w1.fi/cgit/hostap/plain/wpa_supplicant/README-P2P
  — p2p_group_add / p2p_find / p2p_listen / p2p_set listen_channel / group vs main iface.
- wpa_supplicant.conf man/example: p2p_disabled, p2p_go_max_inactivity, country.
- Config-key idiom (reg_class 81 + channel 1 forces 2.4 GHz): https://hackerj.tistory.com/33
- Autonomous GO discoverable without p2p_find: https://stackoverflow.com/questions/18578309
- WFD-IE requirement (CONFIG_WIFI_DISPLAY / set wifi_display): albfan/miraclecast issues #50, #92.
- Band-steering / 5 GHz pitfall: https://github.com/raspberrypi/linux/issues/4078
- Reg-domain channel pruning: https://bbs.archlinux.org/viewtopic.php?id=241240
