# Third-party components

Unkn0WnDPI is distributed under the MIT license (see `LICENSE`). It embeds and
redistributes the following open-source components:

## zapret / winws

- Upstream: <https://github.com/bol-van/zapret>
- Windows bundle / presets: <https://github.com/Flowseal/zapret-discord-youtube>
- License: MIT (see the upstream `LICENSE.txt`)
- Included: `winws.exe`, WinDivert, fake TLS/QUIC/STUN binaries, hostlists,
  ipset lists and BAT presets under `assets/engine` and `assets/presets`.

## tg-ws-proxy

- Upstream: <https://github.com/Flowseal/tg-ws-proxy>
- License: MIT
- The built-in MTProto proxy in `src/tgws.rs` is an independent implementation
  based on the protocol ideas from this project.

## WinDivert

- Upstream: <https://github.com/basil00/WinDivert>
- `WinDivert.dll` and `WinDivert64.sys` are redistributed as part of the zapret
  engine. See the upstream project for licensing details.

## Cygwin

- Upstream: <https://cygwin.com/>
- `cygwin1.dll` is required by the upstream `winws.exe`. Cygwin is distributed
  under the GNU GPL; sources are available from the Cygwin project.
