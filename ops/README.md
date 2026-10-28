# Ops: building a field kit

The bridge runs on the computer next to the scanner: a Raspberry Pi on the
tripod in the field, or a desktop at the bench. It holds the link to the X9
and serves the bridge API. This page builds a field kit from a fresh Pi, layer
by layer, with a check after each; an agent can follow it step by step. Steps
marked **person** need someone at the kit.

<p align="center">
  <img src="../docs/img/connections.svg" width="900"
       alt="Laptop (the agent, its own internet) to Raspberry Pi 4 over a USB-C cable: CDC-NCM, Pi 10.55.0.1/24, laptop by DHCP with no gateway and no DNS. The Pi runs x9-bridge on port 8795 and the MCP server, and reaches the Trimble X9 over the scanner's own Wi-Fi (192.168.43.1:34567) or USB (CDC-EEM, 192.168.42.1).">
</p>

- **USB-C cable:** a CDC-NCM network; the Pi is `10.55.0.1/24`, the laptop
  gets an address by DHCP with no gateway and no DNS.
- **The scanner:** its own Wi-Fi access point (scanner API
  `192.168.43.1:34567`), or USB (CDC-EEM, `192.168.42.1`), faster for bulk
  copies.

<p align="center">
  <img src="../docs/img/setup.svg" width="900"
       alt="Seven layers, each checked before the next: the Pi (aarch64), the USB-C link (laptop gets 10.55.0.x), the scanner links (scanner answers), the bridge (/v1/health), the MCP server (imports), the laptop (scanner tools listed), acceptance (registered and mapped). Steps 1, 3 and 7 need a person.">
</p>

The laptop keeps its own internet (any Wi-Fi, Starlink); nothing routes
through the Pi. The Pi needs no internet in the field.

## 1. The Pi

**Person:** a Raspberry Pi 4 with Raspberry Pi OS (Debian 13 "trixie",
64-bit), for example written with Raspberry Pi Imager: a hostname, a user,
ssh enabled with the laptop's public key, and passwordless `sudo` for that
user during the build (the steps below install system files). While the Pi
still has home internet, make sure the few packages the build uses are there:

```bash
sudo apt-get install -y python3-venv openssl curl dnsmasq-base
```

(`dnsmasq-base` serves the laptop its address on the USB-C link.)

On a machine with internet, download the release archive for the Pi
(`trimble-x9-bridge-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz`, v0.12.1 or later:
earlier archives lack the files below; Linux builds need glibc 2.36 or newer,
which trixie has), check it against its `.sha256`, copy it to the Pi and unpack
it. The `ops/…` paths below are in it (and in this repository); run the steps
from inside the unpacked folder.

Check: `ssh <pi> uname -m` prints `aarch64`.

## 2. The USB-C link to the laptop

The Pi 4's USB-C port becomes a network device (CDC-NCM: macOS, Linux and
Windows 11 have drivers built in). Use NCM, not the legacy `g_ether`/ECM
gadget: with macOS, ECM passes traffic one way only.

```bash
# The port in device mode (Pi 4 only; keep other models' sections as they are).
printf '\n[pi4]\n# USB-C port as a USB network device for the laptop link (x9 bridge)\ndtoverlay=dwc2,dr_mode=peripheral\n\n[all]\n' | sudo tee -a /boot/firmware/config.txt
grep -q 'modules-load=dwc2' /boot/firmware/cmdline.txt || sudo sed -i 's/$/ modules-load=dwc2/' /boot/firmware/cmdline.txt
# The gadget, NetworkManager managing it, and DHCP with no gateway or DNS.
sudo install -m 0755 ops/usb-gadget-ncm /usr/local/sbin/
sudo install -m 0644 ops/usb-gadget-ncm.service /etc/systemd/system/
sudo install -m 0644 ops/86-usb-gadget-managed.rules /etc/udev/rules.d/
sudo install -D -m 0644 ops/x9link-dnsmasq.conf /etc/NetworkManager/dnsmasq-shared.d/x9link.conf
sudo systemctl enable usb-gadget-ncm
```

The `usb-gadget` NetworkManager profile is made in step 3 with the scanner's
profiles. Then reboot (**person**: power the Pi from the laptop's USB-C port, or
from a supply on the Pi's power input with a data cable to the laptop).

The link comes up once step 3 has made its NetworkManager profile; it is
checked there.

## 3. The scanner links

**Person**, in an interactive session on the Pi (`ssh -t <pi>`, so the script
has a terminal to ask on):

```bash
sudo ops/setup-nm-profiles TRIMBLE-X9-12345     # the scanner's Wi-Fi name
```

It creates `usb-gadget` (step 2), `x9-usb` (the scanner over USB, driver
`cdc_eem`) and `x9-wifi` (the scanner's access point: priority 10 over other
Wi-Fi, never the default route, the scanner's DNS ignored, keeps retrying).
The Pi has one Wi-Fi radio: joining the scanner's network drops home Wi-Fi.
Work over the USB-C link (`10.55.0.1`) from here on, or the session you are in
is cut off; once `x9-wifi` exists, `x9-wifi-prefer` (below) joins it on its
own.

It asks for the scanner's Wi-Fi passphrase on the terminal: whatever is set on
the scanner, often not the factory one. The person types it; an agent never
guesses, types or prints it. It is stored root-only in the profile.

NetworkManager never leaves a working network for a better one, so a Pi that
also knows home Wi-Fi would stay there after the scanner's network drops.
`x9-wifi-prefer` switches back within about 30 s whenever the scanner's
network is in range:

```bash
sudo install -D -m 0755 ops/x9-wifi-prefer -t /opt/x9-bridge/
sudo install -m 0644 ops/x9-wifi-prefer.service ops/x9-wifi-prefer.timer /etc/systemd/system/
sudo systemctl enable --now x9-wifi-prefer.timer
```

It logs to the journal (`journalctl -t x9-wifi-prefer`); set `X9_WIFI_CON` /
`X9_WIFI_DEV` in the service for other names.

Check, after a reboot (**person**: the laptop on the Pi's USB-C port, the
scanner on):

- on the Pi, `nmcli -t -f NAME,DEVICE connection show --active` lists
  `usb-gadget` and `x9-wifi` (or `x9-usb`);
- on the laptop, a new network interface has a `10.55.0.x` address, no
  default route goes through it (`netstat -rn` / `ip route`), and
  `ssh <user>@10.55.0.1 true` works;
- on the Pi, `curl -s -m 3 -o /dev/null -w '%{http_code}\n' http://192.168.43.1:34567/api/v1/tasks`
  prints `200`. (Don't print the scanner's `/status`: it includes its Wi-Fi
  settings.)

## 4. The bridge

From the unpacked release folder (step 1), as one block in one ssh session
(it keeps the tokens in shell variables between the lines):

```bash
sudo install -D -m 0755 x9-bridge ops/x9-bridge-run -t /opt/x9-bridge/
sudo install -D -m 0600 ops/env.example /etc/x9-bridge/env      # listens on 0.0.0.0:8795
sudo install -d /etc/x9-bridge/recipes                          # recipe .toml files go here
# Tokens: one "name role token" per line (roles viewer, agent, operator, admin; 16+ characters).
A=$(openssl rand -hex 24); O=$(openssl rand -hex 24)
printf 'agent agent %s\nperson operator %s\n' "$A" "$O" | sudo install -D -m 0600 /dev/stdin /etc/x9-bridge/tokens
# The MCP server's launcher reads these two (same user as the ssh login).
mkdir -p ~/x9-bridge && umask 077 && echo "$A" > ~/x9-bridge/agent.token && echo "$O" > ~/x9-bridge/operator.token
unset A O
sudo install -m 0644 ops/x9-bridge.service /etc/systemd/system/
sudo install -m 0755 ops/90-x9-bridge /etc/NetworkManager/dispatcher.d/
sudo systemctl enable --now x9-bridge
```

`x9-bridge.service` runs the bridge as a throwaway user on a read-only
system; it writes only `/var/lib/x9-bridge`, and gets the token file as a
systemd credential. `x9-bridge-run` picks the scanner link at start (USB if
it answers, else Wi-Fi; it waits up to 30 s for one), and `90-x9-bridge`
restarts it when a scanner link comes up. The defaults in `env.example`
listen on every interface: the bridge's own default (`127.0.0.1:8795`) is
reachable from the Pi only, not from the laptop.

Check: `curl -s http://10.55.0.1:8795/v1/health` from the laptop prints the
version; with a token, `/v1/status` shows `"connected": true` when the
scanner is on.

## 5. The MCP server

The agent drives the bridge through
[TrimbleX9-mcp](https://github.com/northidaholabs/TrimbleX9-mcp), run on the
Pi over ssh in bridge mode. Its README covers installing it on a Pi without
internet (a wheelhouse built elsewhere) and the launcher, which reads
`~/x9-bridge/agent.token` and `operator.token`.

## 6. The laptop

- An ssh alias for the Pi that takes the cable when it is there and the home
  LAN otherwise (OpenSSH, macOS or Linux):

  ```
  Match originalhost x9-bridge exec "nc -z -G 1 10.55.0.1 22 >/dev/null 2>&1"
    HostName 10.55.0.1

  Host x9-bridge
    HostName <the Pi's LAN address>
    User <user>
    IdentityFile ~/.ssh/<key>
    IdentitiesOnly yes
    HostKeyAlias x9-bridge
    ConnectTimeout 8
    ServerAliveInterval 15
  ```

  (`nc -G` is macOS's connect timeout; on Linux use `nc -z -w 1`.)
- Accept the Pi's host key under the alias once (the MCP entry below runs ssh
  in batch mode, which can't answer a host-key question):
  `ssh -o StrictHostKeyChecking=accept-new x9-bridge true`
- The MCP server in Claude Code:
  `claude mcp add -s user x9-bridge -- ssh -o BatchMode=yes x9-bridge 'X9_MCP_MODE=bridge ~/x9-bridge/bin/trimblex9-mcp-auto'`
  (quoted, so the Pi's `~` is used, not the laptop's)
- Optional, a laptop worker for registration (bridge 0.10.0+): the release's
  macOS archive (`aarch64-apple-darwin` for Apple silicon, `x86_64-apple-darwin`
  for Intel). Fetch it with `curl`; a copy downloaded in a browser is
  quarantined by Gatekeeper until allowed. The agent asks the person once per
  laptop whether to use it (`scanner_processing`); on a yes, on the laptop:

  ```bash
  tag=v0.15.0; t=aarch64-apple-darwin      # x86_64-apple-darwin on Intel
  f=trimble-x9-bridge-$tag-$t
  curl -LO https://github.com/northidaholabs/trimble-x9-bridge/releases/download/$tag/$f.tar.gz
  curl -LO https://github.com/northidaholabs/trimble-x9-bridge/releases/download/$tag/$f.tar.gz.sha256
  shasum -a 256 -c $f.tar.gz.sha256 && tar xzf $f.tar.gz
  # The agent token, copied without printing it.
  mkdir -p ~/.config/x9-worker && (umask 077; ssh x9-bridge 'cat ~/x9-bridge/agent.token' > ~/.config/x9-worker/token)
  # For the visit, in the background; it reconnects by itself.
  $f/x9-bridge worker --bridge http://10.55.0.1:8795 > ~/.config/x9-worker/log 2>&1 &
  ```

Check: in Claude Code, the `x9-bridge` server lists its `scanner_*` tools and
`scanner_status` answers; `GET /v1/workers` lists the worker when it runs.

## 7. Acceptance

With the scanner on and levelled on its tripod (**person**: nobody near the
head): ask the agent for a full-dome scan in a new project. The first scan
asks the person once for the visit's permission (a grant). The scan lands,
is registered (`scanner_registration`: one station, the frame), and the
minimap draws (`scanner_minimap`). Without a scanner, the simulator gives the
same flow ([tools.md](../docs/tools.md#x9-mock)).

## Gotchas

- **Laser pointer:** the X9's own `laser-pointer` API (the bridge doesn't
  expose it). Only with the person at the scanner and having said so, and
  never without explicit horizontal and vertical angles aimed at the floor:
  without them the head sweeps with the laser on.
- **Permission:** an agent's scans need a grant or an operator's approval.
  The person gives it once per visit; the agent never uses the operator token
  itself (the MCP server does, only after the person said yes). This is a
  rule, not enforced: the ssh user that runs the MCP server can read
  `~/x9-bridge/operator.token`.
- **Small SD card:** the bridge caches scan files in `/var/lib/x9-bridge`
  (about 100 MB per full dome, plus station clouds); watch `df -h /` and clear
  `scans/`, `views/` and `exports/` between jobs when space runs low.
- **`/dev/shm` is wiped** when a user's last login session ends: keep
  files a process needs across ssh sessions somewhere else.
- **A cancelled scan lingers:** the X9 keeps a cancelled task listed for
  about 13 s; the bridge waits it out.
- **Memory:** decoding a scan briefly needs about 0.9 GB; prepared stations
  take 10–40 MB each (`BRIDGE_PREPARED_MB`, default 2048).

## `x9-netctl` (desk machines)

A small privileged helper for a desk machine that talks to the X9 over USB:
it brings the scanner's USB link up isolated (no default route, no DNS) and
down again, without full root. Its defaults are for a reference Linux desktop
(`X9_GW`, `X9_ETH_DEV`, … in its header); a field Pi doesn't need it, since
NetworkManager's `x9-usb` profile does the same. Review it, then:

```bash
sudo install -m 0755 ops/x9-netctl /usr/local/sbin/x9-netctl
echo "$USER ALL=(root) NOPASSWD: /usr/local/sbin/x9-netctl" | sudo tee /etc/sudoers.d/x9-netctl
```
