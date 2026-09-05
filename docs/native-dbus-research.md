# Research: sudo-free / subprocess-free Miracast sink on Ubuntu 24.04

**Question:** can the app drop `sudo` + `wpa_cli`/subprocess and drive P2P + WFD
natively over D-Bus, and can it run as an unprivileged user?

**Answer: yes — including the WFD Information Elements, which I earlier flagged as
a probable blocker. It is not a blocker on this stack.** Verified against the exact
packages Ubuntu 24.04 ships and against the live D-Bus interface on this machine.

## Verified stack (Ubuntu 24.04)

- `wpasupplicant 2:2.10-21ubuntu0.4`
- `network-manager 1.46.0-1ubuntu2.8`

## The finding that changes the plan: WFD IEs ARE on D-Bus

`busctl --system introspect fi.w1.wpa_supplicant1 /fi/w1/wpa_supplicant1` on this
host shows, on the ROOT object:

```
.WFDIEs   property  ay   0   emits-change writable
```

`WFDIEs` (Wi-Fi Display subelements, byte array, **read/write**) is exactly the
`wfd_subelem_set` / `wifi_display 1` functionality the app currently sets through
`wpa_cli`. It is settable over D-Bus. The wpa_supplicant binary also carries the
matching internal handlers (`WFD IEs set`, `WFD: Set subelement`). So the whole
advertiser path can move to D-Bus.

## Full P2P/WPS surface (authoritative, w1.fi/wpa_supplicant/devel/dbus.html)

Everything the app does maps to a documented method/property/signal:

| App action (today via wpa_cli) | D-Bus equivalent |
|---|---|
| `set wifi_display 1` + `wfd_subelem_set …` | root `WFDIEs` property (write `ay`) |
| `set device_name`, `p2p_go_ht40`, go_intent | `P2PDevice.P2PDeviceConfig` property (`DeviceName`, `GOIntent`, …) |
| `p2p_find type=progressive` | `P2PDevice.Find({DiscoveryType:"progressive"})` |
| `p2p_group_add persistent` (autonomous GO) | `P2PDevice.GroupAdd({persistent:true})` |
| group-created detection (`ip link` poll) | `GroupStarted` signal → `interface_object` (no polling, no `ip link`) |
| `wps_pin any <PIN>` (arm registrar) | `WPS.Start({Role:"registrar", Pin:"<PIN>", …})` on the GO group interface |
| `AP-STA-CONNECTED` event monitor (`wpa_cli` ATTACH loop) | `Group.PeerJoined(o: peer)` signal |
| `AP-STA-DISCONNECTED` | `Group.PeerDisconnected(o: peer)` signal |
| `P2P-GROUP-REMOVED` | `GroupFinished` signal |
| peer MAC / details | `Peer` object properties |

This replaces the entire `wpa_cli`-spawn + interactive-ATTACH-parsing design
(`connection.rs` event loop, `advertiser.rs`, `p2p_supplicant.rs`) with
**event-driven D-Bus signals** — strictly better than scraping `wpa_cli` stdout.

## Access control: group-based, NOT polkit — and it answers "no root"

`/usr/share/dbus-1/system.d/wpa_supplicant.conf` on this host:

```
<policy group="netdev">
    <allow send_destination="fi.w1.wpa_supplicant1"/>
    <allow send_interface="fi.w1.wpa_supplicant1"/>
    <allow receive_sender="fi.w1.wpa_supplicant1" receive_type="signal"/>
</policy>
<policy context="default"> <deny .../> </policy>
```

So an unprivileged user **in the `netdev` group** can call every method above and
receive its signals over the system bus — **no `sudo`, no polkit prompt.** On
Ubuntu the desktop user is commonly already in `netdev`; if not, the `.deb`
postinst can add them (or ship a drop-in policy for a dedicated group).

## The parts that genuinely still need privilege (and how to avoid root)

wpa_supplicant handles P2P/WPS/WFD, but **it does not do IP addressing or DHCP** —
that is the app's `ip addr add` + `dnsmasq`, which today run under `sudo`:

1. **IP on the group interface** (`ip addr add 192.168.173.1/24`, `link set up`):
   needs `CAP_NET_ADMIN`. Native option: rtnetlink from the process — but still
   requires the capability. **Best sudo-free route: let NetworkManager own the
   group interface** and run it in shared/AP mode (it already does IP + a DHCP
   server for hotspots, as root, brokered via polkit). NM's `WifiP2PPeer` /
   `NMDeviceWifiP2P` API (NM 1.46 has P2P) can form the group AND handle IP.
2. **DHCP server** (hand out an address to the phone): today `dnsmasq` on port 67
   (needs root). Sudo-free routes: (a) NetworkManager shared mode provides the
   DHCP server itself; (b) grant the binary `CAP_NET_BIND_SERVICE` +
   `CAP_NET_ADMIN` via `setcap` (no root at runtime); (c) a tiny in-process DHCP
   responder under those caps.

**Trade-off / open question:** NetworkManager's P2P API can form groups and do IP,
but it is **not clear NM exposes WFD IE configuration** — that lived only on
wpa_supplicant's `WFDIEs`. So the cleanest architecture may be a **hybrid**:
wpa_supplicant D-Bus for P2P + WFD IEs + WPS + events, and either NM-shared-mode
or a `setcap` capability for the IP/DHCP leg. A pure-NM path might not carry the
WFD IEs; that needs a hands-on test with the hardware before committing.

## Dependency reality for the Rust port

- Current crate is **sync/thread-based, no async runtime.** `zbus` has a
  **blocking API** (`zbus::blocking`) — usable without pulling tokio.
- `neli` offers a **sync** feature for netlink (avoids the async `rtnetlink`
  stack) if we do IP ourselves rather than via NM.
- `ethtool -i` → read `/sys/class/net/<iface>/device/driver` symlink (verified
  working here: `rtw89_8852cu`, `iwlwifi`, …) — zero deps, unprivileged.
- `systemctl --user` → systemd Manager on the **session** bus via `zbus::blocking`
  — unprivileged.

## Recommendation

The sudo-free / subprocess-free target is **achievable**, and the WFD blocker I
worried about does not exist on 2.10. Suggested phasing:

1. **Zero-privilege, zero-risk now:** `ethtool`→sysfs, `systemctl --user`→session
   D-Bus. No behaviour change, drops 3 subprocess sites.
2. **wpa_supplicant D-Bus migration:** replace `wpa_cli` spawns + the ATTACH event
   loop with `zbus::blocking` calls + signal subscriptions (P2PDevice/WPS/Group +
   `WFDIEs`). Requires the user in `netdev`; removes the dedicated-supplicant
   process and `/tmp` control-socket machinery entirely.
3. **IP/DHCP without root:** decide between NM-shared-mode (broadest, but verify
   WFD-IE coexistence) vs `setcap CAP_NET_ADMIN,CAP_NET_BIND_SERVICE` + native
   rtnetlink/in-proc DHCP. This is the only step needing a hardware test.

After 1–3, the app runs with **no `sudo` in the code** and (via `netdev` +
`setcap`, or NM brokering) **no root at runtime**.
